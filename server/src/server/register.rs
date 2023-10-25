use crate::server::{counter_ack, noop_ack, CompletionHandler, IpCache};
use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::{AddrStatus, SessionManager, SessionRef, SessionState, SessionWeakRef};
use anyhow::anyhow;
use bytes::BytesMut;
use itertools::Itertools;
use metrics::recorder;
use socket2::Protocol;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::DerefMut;
use std::rc;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use quick_cache::sync::Cache;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{spawn_local, JoinHandle};
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{
    packet, request, response, Endpoint, Identity, Message, Packet, RequestId, Response, StatusCode,
};

mod metric {
    use crate::server::DoneAck;
    use crate::state::Clock;
    use metrics::{recorder, Counter, Gauge, Histogram, Key, Label};

    const KEY_START: Key = Key::from_static_name("ya-relay.session.establish.register");
    const KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.node.error");
    const KEY_DONE: Key = Key::from_static_name("ya-relay.session.establish.finished");

    const KEY_IPCHECK_REQUESTS: Key = Key::from_static_name("ya-relay.ipcheck.addrs");

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

    pub fn ip_check_requests(worker_idx: usize) -> Gauge {
        let l = ("worker", worker_idx.to_string());
        let key = KEY_IPCHECK_REQUESTS.with_extra_labels(vec![(&l).into()]);

        recorder().register_gauge(&key)
    }
}

struct CheckIpRequest {
    session_w: SessionWeakRef,
    session_id: SessionId,
    addr: SocketAddr,
    reply_request_id: RequestId,
    ts: Instant,
    retry_cnt: usize,
}

pub struct RegisterHandler {
    session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    metrics: metric::RegisterMetric,
    ack: CompletionHandler,
    check_ip_tx: UnboundedSender<CheckIpRequest>,
    cache : Arc<Cache<SocketAddr, (Instant, bool)>>
}

static WORKER_IDX: AtomicUsize = AtomicUsize::new(0);
impl RegisterHandler {
    pub fn new(
        session_manager: &Arc<SessionManager>,
        slot_manager: &Arc<SlotManager>,
        checker_ip: IpAddr,
        reply_socket: &Rc<UdpSocket>,
        cache : IpCache
    ) -> Self {
        let worker_idx = WORKER_IDX.fetch_add(1, Ordering::SeqCst);
        let retry_time = Duration::from_millis(150);
        let timeout = Duration::from_secs(2);
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics: metric::RegisterMetric = Default::default();
        let ack = counter_ack(&metrics.done, &metrics.error);
        let (check_ip_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CheckIpRequest>();

        let reply_socket = Rc::downgrade(reply_socket);


        let reply_send = {
            let session_manager = Arc::clone(&session_manager);
            let ack = ack.clone();

            move |session_w: SessionWeakRef, reply_request_id, addr_status| {
                let reply_socket = reply_socket.clone();
                let session_manager = session_manager.clone();
                let ack = ack.clone();

                log::info!("sending reply");
                async move {
                    if let Some((addr, bytes)) = session_w.upgrade().and_then(|session_ref| {
                        log::info!("[{}] sending reply", session_ref.peer);
                        let endpoints = Vec::new();
                        let completed = match &mut *session_ref.state.lock() {
                            SessionState::Est {
                                addr_status: status @ AddrStatus::Pending,
                                keys,
                                ..
                            } => {
                                *status = addr_status;
                                for key in keys {
                                    session_manager.link_session(key.node_id, &session_ref);
                                }
                                true
                            }
                            SessionState::Est {
                                addr_status: status @ (AddrStatus::Invalid | AddrStatus::Unknown),
                                ..
                            } => {
                                *status = addr_status;
                                false
                            }
                            _ => return None,
                        };


                        let resp = Packet::response(
                            reply_request_id,
                            session_ref.session_id.to_vec(),
                            StatusCode::Ok,
                            response::Register { endpoints },
                        )
                            .encode_to_vec();
                        Some((session_ref.peer, resp))
                    }) {
                        let clock = Clock::now();
                        let socket = reply_socket.upgrade()?;
                        match socket.send_to(&bytes, addr).await {
                            Ok(_) => { ack.done(&clock); Some(()) },
                            Err(_) => { ack.error(&clock); None }
                        }
                    } else {
                        None
                    }
                }
            }
        };
        let reply_send = Rc::new(reply_send);

        let _ = {
            let g_pending_requests = metric::ip_check_requests(worker_idx);
            let reply_send = Rc::clone(&reply_send);
            let error = metrics.error.clone();
            let cache = cache.clone();

            let ip_sender = spawn_local(async move {
                let checker_socket = Rc::new(UdpSocket::bind((checker_ip, 0)).await?);
                let cache = cache.clone();

                let pending_requests = Rc::new(RefCell::new(HashMap::<
                    SocketAddr,
                    Vec<CheckIpRequest>,
                >::new()));

                let reply_send = Rc::clone(&reply_send);

                let _ = {
                    /// Job for retry pings
                    let checker_socket = Rc::downgrade(&checker_socket);
                    let pending_requests = Rc::downgrade(&pending_requests);
                    let reply_send = Rc::clone(&reply_send);
                    let cache = cache.clone();

                    let _: JoinHandle<Option<()>> = spawn_local(async move {
                        loop {
                            tokio::time::sleep(retry_time).await;
                            let mut to_send = Vec::new();
                            let mut to_drop = Vec::new();
                            let g = pending_requests.upgrade()?;
                            let mut pending_requests = g.borrow_mut();
                            for mut entry in pending_requests.values_mut() {
                                if let Some(r) = entry.first_mut() {
                                    let do_drop = if r.ts.elapsed() > retry_time {
                                        if r.retry_cnt > 0 {
                                            to_send.push((
                                                r.addr,
                                                Packet::request(
                                                    r.session_id.to_vec(),
                                                    request::Ping {},
                                                ),
                                            ));
                                            r.retry_cnt -= 1;
                                            false
                                        } else {
                                            if let Some((ts, prev)) = cache.get(&r.addr) {
                                                if ts.elapsed() > Duration::from_secs(300) {
                                                    cache.insert(r.addr, (Instant::now(), false))
                                                }
                                            }
                                            else {
                                                cache.insert(r.addr, (Instant::now(), false))
                                            }
                                            true
                                        }
                                    } else {
                                        false
                                    };
                                    if do_drop {
                                        to_drop.extend(entry.drain(..));
                                    }
                                }
                            }
                            pending_requests.retain(|_, v| !v.is_empty());
                            let n = pending_requests.len();

                            drop(pending_requests);
                            drop(g);

                            g_pending_requests.set(n as f64);
                            for r in to_drop {
                                reply_send(r.session_w, r.reply_request_id, AddrStatus::Invalid)
                                    .await;
                            }
                            let socket = checker_socket.upgrade()?;
                            for (addr, packet) in to_send {
                                let bytes = packet.encode_to_vec();
                                if let Err(e) = socket.send_to(&bytes, addr).await {
                                    log::error!("failed to send result: {:?}", e);
                                }
                            }
                        }
                    });
                };

                let _ = {
                    let checker_socket = checker_socket.clone();
                    let pending_requests = Rc::downgrade(&pending_requests);
                    let reply_send = Rc::clone(&reply_send);
                    let cache = cache.clone();

                    let _ = spawn_local(async move {
                        let mut buffer = BytesMut::with_capacity(32000);

                        loop {
                            buffer.reserve(1500);
                            let (n, addr) =
                                checker_socket.recv_buf_from(&mut buffer).await.unwrap();

                            match Packet::decode(buffer.split()) {
                                Ok(Packet {
                                    session_id: _,
                                    kind:
                                        Some(packet::Kind::Response(Response {
                                            kind: Some(response::Kind::Pong(_)),
                                            ..
                                        })),
                                }) => (),
                                _ => continue,
                            };

                            cache.insert(addr, (Instant::now(), true));

                            if let Some(ready) = {
                                let g = match pending_requests.upgrade() {
                                    Some(g) => g,
                                    None => break,
                                };
                                let mut gp = g.borrow_mut();
                                gp.remove(&addr)
                            } {
                                for request in ready {
                                    reply_send(
                                        request.session_w,
                                        request.reply_request_id,
                                        AddrStatus::Valid,
                                    )
                                    .await;
                                }
                            }
                        }
                    });
                };

                while let Some(request) = rx.recv().await {
                    if request.ts.elapsed() > timeout {
                        reply_send(
                            request.session_w,
                            request.reply_request_id,
                            AddrStatus::Invalid,
                        )
                        .await;
                        error.increment(1);
                        continue;
                    }
                    let mut pr = pending_requests.borrow_mut();
                    if let Some(requests) = pr.get_mut(&request.addr) {
                        requests.push(request);
                        continue;
                    } else {
                        let ping = Packet::request(request.session_id.to_vec(), request::Ping {})
                            .encode_to_vec();
                        let dst = request.addr;
                        pr.insert(dst, vec![request]);
                        drop(pr);
                        checker_socket.send_to(&ping, dst).await?;
                    }
                }

                anyhow::Ok(())
            });
        };

        Self {
            session_manager,
            slot_manager,
            metrics,
            ack,
            check_ip_tx,
            cache
        }
    }

    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: SessionId,
        register: &request::Register,
    ) -> Option<(CompletionHandler, Packet)> {

        let session_ref = match self.session_manager.session(&session_id) {
            Some(session_ref) => session_ref,
            None => {
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
                let new_addr_status = if v { AddrStatus::Valid } else { AddrStatus::Invalid};
                let endpoints = new_addr_status.endpoints(session_ref.peer);
                match &mut *session_ref.state.lock() {
                    SessionState::Est { addr_status, .. } => {
                        *addr_status = new_addr_status; 
                    }
                    _ => ()
                }
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Ok,
                        response::Register {
                            endpoints
                        },
                    ),
                ));
            }
            _ => ()
        }

        let session_w = Arc::downgrade(&session_ref);

        let mut g = session_ref.state.lock();
        match g.deref_mut() {
            SessionState::Est {
                ref mut addr_status,
                node_id,
                ..
            } => {
                let next_status = match addr_status {
                    AddrStatus::Pending => AddrStatus::Pending,
                    AddrStatus::Unknown => {
                        self.check_ip_status(clock.time(), session_id, request_id, session_ref.peer, session_w);
                        AddrStatus::Pending
                    }
                    AddrStatus::Invalid => {
                        return Some((
                            noop_ack(),
                            Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::Ok.into(),
                                    request_id,
                                    kind: Some(response::Kind::Register(Default::default())),
                                })),
                            },
                        ))
                    }
                    AddrStatus::Valid => {
                        let endpoint = Endpoint {
                            protocol: Protocol::UDP.into(),
                            address: session_ref.peer.ip().to_string(),
                            port: session_ref.peer.port() as u32,
                        };
                        return Some((
                            noop_ack(),
                            Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::Ok.into(),
                                    request_id,
                                    kind: Some(response::Kind::Register(response::Register {
                                        endpoints: vec![endpoint],
                                    })),
                                })),
                            },
                        ));
                    }
                };
                *addr_status = next_status;
                None
            }
            _ => {
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
        }
    }

    fn check_ip_status(
        &self,
        ts: Instant,
        session_id: SessionId,
        reply_request_id: RequestId,
        peer: SocketAddr,
        session_w: SessionWeakRef,
    ) {
        let request = CheckIpRequest {
            session_w,
            session_id,
            addr: peer,
            reply_request_id: 0,
            ts,
            retry_cnt: 5,
        };
        self.check_ip_tx.send(request).ok();
    }
}
