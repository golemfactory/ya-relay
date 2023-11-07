use std::net::{SocketAddr};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use quick_cache::sync::Cache;
use tokio::task::spawn_local;

use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto::{request, response, Message, Packet, StatusCode};

use crate::server::ip_checker::IpChecker;
use crate::server::{counter_ack, noop_ack, CompletionHandler, IpCache};
use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::udp_server::UdpSocket;
use crate::{AddrStatus, SessionManager};

mod metric {
    use metrics::{recorder, Counter, Key};

    static KEY_START: Key = Key::from_static_name("ya-relay.session.establish.register");
    static KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.node.error");
    static KEY_DONE: Key = Key::from_static_name("ya-relay.session.establish.finished");

    #[derive(Clone)]
    pub struct RegisterMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
    }

    impl Default for RegisterMetric {
        fn default() -> Self {
            let recorder = recorder();
            let start = recorder.register_counter(&KEY_START);
            let done = recorder.register_counter(&KEY_DONE);
            let error = recorder.register_counter(&KEY_ERROR);
            Self { start, done, error }
        }
    }
}

pub struct RegisterHandler {
    session_manager: Arc<SessionManager>,
    #[allow(dead_code)]
    slot_manager: Arc<SlotManager>,
    metrics: metric::RegisterMetric,
    ack: CompletionHandler,
    ip_checker: IpChecker,
    cache: Arc<Cache<SocketAddr, (Instant, bool)>>,
    reply_socket: Weak<UdpSocket>,
}

impl RegisterHandler {
    pub fn new(
        session_manager: &Arc<SessionManager>,
        slot_manager: &Arc<SlotManager>,
        ip_checker: IpChecker,
        reply_socket: &Rc<UdpSocket>,
        cache: IpCache,
    ) -> Self {
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics: metric::RegisterMetric = Default::default();
        let ack = counter_ack(&metrics.done, &metrics.error);

        let reply_socket = Rc::downgrade(reply_socket);

        Self {
            session_manager,
            slot_manager,
            metrics,
            ack,
            ip_checker,
            cache,
            reply_socket,
        }
    }

    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: SessionId,
        _register: &request::Register,
    ) -> Option<(CompletionHandler, Packet)> {
        let session_ref = match self.session_manager.session(&session_id) {
            Some(session_ref) => session_ref,
            None => {
                log::debug!(target: "request::register", "[{src}] session not found {session_id}");
                self.metrics.error.increment(1);
                return Some((
                    noop_ack(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Unauthorized,
                        response::Register::default(),
                    ),
                ));
            }
        };
        clock.touch(&session_ref.ts);
        self.metrics.start.increment(1);
        match self.cache.get(&src) {
            Some((ts, v)) if ts.elapsed() < Duration::from_secs(60) => {
                log::debug!(target: "request::register", "[{src}] resolving from cache: {v:?}");
                let new_addr_status = if v {
                    AddrStatus::Valid(Instant::now())
                } else {
                    AddrStatus::Invalid(Instant::now())
                };
                *session_ref.addr_status.lock() = new_addr_status;

                let endpoints = session_ref.endpoint().into_iter().collect();
                self.session_manager.link_sessions(&session_ref);
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Ok,
                        response::Register { endpoints },
                    ),
                ));
            }
            _ => (),
        }

        {
            let reply_socket = self.reply_socket.clone();
            let ack = self.ack.clone();
            let sm = self.session_manager.clone();
            log::debug!(target: "request::register", "[{src}] resolving from ip_checker {session_id}");
            self.ip_checker.check_ip_status(clock.time(), session_ref, move |status, session_ref| {
                let reply_socket = match reply_socket.upgrade() {
                    Some(v) => v,
                    None => return
                };
                let mut g = session_ref.addr_status.lock();
                g.set_valid(status);
                drop(g);
                log::debug!(target: "request::register", "[{src}] set_valid {session_id} {status}");
                let endpoints = session_ref.endpoint().into_iter().collect();
                let session_id = session_ref.session_id;
                let peer = session_ref.peer;
                sm.link_sessions(&session_ref);
                drop(session_ref);

                let data = Packet::response(
                    request_id,
                    session_id.to_vec(),
                    StatusCode::Ok,
                    response::Register { endpoints },
                );
                spawn_local(async move {
                    let result = reply_socket.send_to(&data.encode_to_vec(), peer).await;
                    let clock = Clock::now();
                    match  result {
                        Ok(_) => ack.done(&clock),
                        Err(e) => {
                            log::error!("[{:?}] failed to send response: {:?}", peer, e);
                            ack.error(&clock);
                        }
                    }
                });
            });
        };
        None
    }
}
