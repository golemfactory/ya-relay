use std::cell::RefCell;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::Counter;
use quick_cache::sync::Cache;
use quick_cache::OptionsBuilder;
use rand::{thread_rng, Rng};
use tokio_util::codec::Decoder;

use ya_relay_core::challenge;
use ya_relay_core::challenge::ChallengeDigest;
use ya_relay_core::server_session::SessionId;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{BytesMut, PacketKind};
use ya_relay_proto::proto::{
    control, packet, request, response, Control, Forward, Message, Packet, Request, Response,
    StatusCode,
};

use crate::state::slot_manager::SlotManager;
use crate::state::{Clock, LastSeen};
use crate::udp_server::{worker_err_fn, PacketType, UdpServer, UdpServerBuilder, UdpSocket};
use crate::{Config, ServerControl, SessionManager};

mod neighbours;
mod session;

mod node;

mod slot;

mod forward;

mod register;

mod reverse_connection;

mod state_decoder;

mod ip_checker;

pub(crate) use ip_checker::IpChecker;
pub use ip_checker::IpCheckerConfig;
pub use session::SessionHandlerConfig;

#[derive(clap::Args)]
/// Ip Checker configuration args
#[command(next_help_heading = "Server options")]
pub struct ServerConfig {
    #[arg(
        short = 'a',
        long = "listen-on",
        env = "RELAY_LISTEN_ON",
        default_value = "0.0.0.0:7477"
    )]
    pub address: SocketAddr,
    #[arg(long, env = "RELAY_WORKERS", default_value_t = default_workers())]
    pub workers: usize,
    #[arg(long, env = "RELAY_TASKS_PER_WORKER", default_value = "32")]
    pub tasks_per_worker: usize,
}

fn default_workers() -> usize {
    let n = std::thread::available_parallelism().unwrap();
    n.into()
}

type IpCache = Arc<Cache<SocketAddr, (Instant, bool)>>;

pub struct Server {
    udp_server: UdpServer,
    pub(crate) session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    control: ServerControl,
}

#[inline]
fn slots_path<P: AsRef<Path>>(state_dir: P) -> PathBuf {
    state_dir.as_ref().join("slots.state")
}

#[inline]
fn sessions_path<P: AsRef<Path>>(state_dir: P) -> PathBuf {
    state_dir.as_ref().join("sessions.state")
}

impl Server {
    pub fn save_state(&self, state_dir: &Path) -> anyhow::Result<()> {
        self.slot_manager.save(&slots_path(state_dir))?;
        self.session_manager.save(&sessions_path(state_dir))?;
        Ok(())
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.udp_server.bind_addr()
    }

    pub fn sessions(&self) -> Arc<SessionManager> {
        self.session_manager.clone()
    }

    pub fn control(&self) -> ServerControl {
        self.control.clone()
    }

    #[cfg(feature = "test-utils")]
    pub fn stop(&self) {}
}

impl Drop for Server {
    fn drop(&mut self) {
        self.udp_server.stop_internal();
    }
}

pub async fn run(config: &Config) -> anyhow::Result<Server> {
    let bind_addr: SocketAddr = config.server.address;

    let slot_manager = config
        .state_dir
        .as_ref()
        .map(slots_path)
        .and_then(|p| {
            if p.exists() {
                SlotManager::load(&p).ok()
            } else {
                None
            }
        })
        .unwrap_or_else(|| SlotManager::new());

    let session_manager = config
        .state_dir
        .as_ref()
        .map(sessions_path)
        .and_then(|p| {
            if p.exists() {
                match SessionManager::load(&p) {
                    Ok(v) => {
                        log::info!("sessions loaded");
                        Some(v)
                    }
                    Err(e) => {
                        log::error!("failed to load: {:?}", e);
                        None
                    }
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| SessionManager::new());

    let server_config = &config.server;
    let session_handler_config = config.session_handler.clone();
    let ip_check_config = config.ip_check.clone();

    session_manager.start_cleanup_processor(&config.session_manager);

    let ip_test_cache: IpCache =
        Arc::new(quick_cache::sync::Cache::<SocketAddr, (Instant, bool)>::new(128));

    let (control, control_handle) = crate::state::control::create();

    let server = {
        let session_manager = session_manager.clone();
        let slot_manager = slot_manager.clone();

        UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
            let session_manager = session_manager.clone();
            let slot_manager = slot_manager.clone();
            let checker_ip = reply.local_addr()?.ip();
            let ip_checker = Rc::new(ip_check_config.build(checker_ip)?);

            control_handle.spawn(&session_manager, &reply, &ip_checker);

            let session_handler = session::SessionHandler::new(&session_manager, &session_handler_config);
            let register_handler = register::RegisterHandler::new(&session_manager, &slot_manager, ip_checker, &reply, ip_test_cache.clone());
            let neighbours_handler = neighbours::NeighboursHandler::new(&session_manager, &slot_manager);
            let node_handler = node::NodeHandler::new(&session_manager, &slot_manager);
            let slot_handler = slot::SlotHandler::new(&session_manager, &slot_manager);
            let forward_handler = forward::ForwardHandler::new(&session_manager, &slot_manager, &reply);
            let rc_handler = reverse_connection::RcHandler::new(&session_manager, &reply);
            let mut disconnect_cache = DisconnectCache::new();

            worker_err_fn(move |pt, mut packet: BytesMut, src| {
                let mut codec = Codec;
                let reply = reply.clone();
                let p = codec.decode(&mut packet)?.ok_or_else(|| anyhow::anyhow!("invalid packet"))?;

                let clock = Clock::now();

                let response =
                    match pt {
                        PacketType::Other => {
                            log::error!("[{src}] recv unknown error");
                            None
                        }
                        PacketType::Unreachable(reason) => {
                            match p {
                                PacketKind::Forward(Forward { session_id, .. }) => {
                                    let session_id = SessionId::from(session_id);
                                    if let Some(session_ref) = session_manager.session(&session_id) {
                                        if session_ref.peer == src && session_ref.addr_status.lock().age() > Duration::from_secs(300) {
                                            log::info!("[{src}] Unreachable (forward) {reason:?} removing session");
                                            session_manager.remove_session(&session_id);
                                        }
                                    }
                                    None
                                }
                                PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Control(Control { kind: Some(control::Kind::ReverseConnection(_)) })) }) => {
                                    session_id.try_into().ok().and_then(|session_id| session_manager.session(&session_id))
                                        .and_then(|session_ref| {
                                            if session_ref.peer == src && session_ref.addr_status.lock().age() > Duration::from_secs(300) {
                                                log::info!("[{src}] Unreachable (reverse connection) {reason:?} removing session");
                                                session_manager.remove_session(&session_ref.session_id);
                                            }
                                            None
                                        })
                                }
                                _ => {
                                    None
                                }
                            }
                        }
                        PacketType::Data => match p {
                            PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Request(Request { request_id, kind: Some(request) })) }) => {
                                let session_id: Option<SessionId> = session_id.try_into().ok();

                                log::debug!("[{src}] got session_id={:?}: request_id={}: {:?}", session_id, request_id, request);

                                match request {
                                    request::Kind::Session(session) => {
                                        session_handler.handle(&clock, src, request_id, session_id, &session)
                                    }
                                    request::Kind::Ping(_) => {
                                        session_id.and_then(|session_id| handle_ping(&clock, src, request_id, session_id, &session_manager, &mut disconnect_cache))
                                    }
                                    request::Kind::Neighbours(neighbours) => {
                                        session_id.and_then(|session_id|
                                            neighbours_handler.handle(&clock, src, request_id, session_id, &neighbours))
                                    }
                                    request::Kind::Node(node) => {
                                        session_id.and_then(|session_id|
                                            node_handler.handle(&clock, src, request_id, session_id, &node))
                                    }
                                    request::Kind::Slot(slot) =>
                                        session_id.and_then(|session_id|
                                            slot_handler.handle(&clock, src, request_id, session_id, &slot)),
                                    request::Kind::Register(register) =>
                                        session_id.and_then(|session_id|
                                            register_handler.handle(&clock, src, request_id, session_id, &register)),
                                    request::Kind::ReverseConnection(rc) =>
                                        session_id.and_then(|session_id| rc_handler.handle(&clock, src, request_id, session_id, &rc)),
                                }
                            }
                            PacketKind::Packet(Packet { session_id: _, kind: None }) => {
                                log::debug!(target: "request::error", "[{src}] unrecognized packet");
                                None
                            }
                            PacketKind::Packet(Packet {
                                                   session_id,
                                                   kind: Some(packet::Kind::Control(Control {
                                                                                        kind: Some(control::Kind::Disconnected(control::Disconnected {
                                                                                                                                   by: Some(control::disconnected::By::SessionId(_))
                                                                                                                               }))
                                                                                    }))
                                               }) => {
                                let session_id: Option<SessionId> = session_id.try_into().ok();
                                if let Some(session_id) = session_id {
                                    session_manager.remove_session(&session_id);
                                    log::debug!(target: "request:disconnect", "[{src}] session {session_id} disconnected");
                                }
                                None
                            }
                            PacketKind::Packet(Packet { session_id: _, kind: Some(packet::Kind::Control(Control { kind: Some(control::Kind::ResumeForwarding(_)) })) }) => {
                                // ignore
                                None
                            }
                            PacketKind::Forward(Forward { session_id, slot, flags, payload }) => {
                                let session_id = session_id.into();
                                forward_handler.handle(&clock, src, session_id, slot, flags, payload)
                            }
                            other => {
                                log::error!("[{src}] unknown packet: {other:?}");
                                None
                            }
                        }
                    };

                let io_part = response.map(|(ack, p)| (ack, p.encode_to_vec()));

                Ok(async move {
                    if let Some((ack, bytes)) = io_part {
                        match reply.send_to(&bytes, src).await {
                            Ok(_) => ack.done(&clock),
                            Err(_) => ack.error(&clock)
                        }
                    }
                    Ok(())
                })
            })
        }).max_tasks_per_worker(server_config.tasks_per_worker)
            .workers(server_config.workers)
            .start(bind_addr).await?
    };

    Ok(Server {
        udp_server: server,
        session_manager,
        slot_manager,
        control,
    })
}

fn handle_ping(
    clock: &Clock,
    src: SocketAddr,
    request_id: u64,
    session_id: SessionId,
    session_manager: &SessionManager,
    disconnect_cache: &mut DisconnectCache,
) -> Option<(CompletionHandler, Packet)> {
    let is_ok = session_manager
        .with_session(&session_id, |session| {
            if session.peer == src {
                clock.touch(&session.ts);
                true
            } else {
                false
            }
        })
        .unwrap_or_default();

    if is_ok {
        log::debug!(target: "request::ping", "[{src}] ping");
        Some((
            noop_ack(),
            Packet {
                session_id: session_id.to_vec(),
                kind: Some(packet::Kind::Response(Response {
                    code: StatusCode::Ok.into(),
                    request_id,
                    kind: Some(response::Kind::Pong(Default::default())),
                })),
            },
        ))
    } else {
        if disconnect_cache.mark_session(clock, session_id) {
            log::warn!(target: "request::ping", "[{src}] ping for unknown session, sending disconnect");
            Some((
                noop_ack(),
                Packet {
                    session_id: session_id.to_vec(),
                    kind: Some(packet::Kind::Control(Control {
                        kind: Some(control::Kind::Disconnected(control::Disconnected {
                            by: Some(control::disconnected::By::SessionId(session_id.to_vec())),
                        })),
                    })),
                },
            ))
        } else {
            log::warn!(target: "request::ping", "[{src}] ping for unknown session");
            None
        }
    }
}

pub trait DoneAck {
    fn done(&self, clock: &Clock);

    fn error(&self, clock: &Clock);
}

type CompletionHandler = Rc<dyn DoneAck>;

fn noop_ack() -> CompletionHandler {
    struct Noop;

    impl DoneAck for Noop {
        fn done(&self, _clock: &Clock) {}

        fn error(&self, _clock: &Clock) {}
    }

    Rc::new(Noop)
}

fn counter_ack(success: &Counter, error: &Counter) -> CompletionHandler {
    struct CounterAck(Counter, Counter);

    impl DoneAck for CounterAck {
        fn done(&self, _clock: &Clock) {
            self.0.increment(1);
        }

        fn error(&self, _clock: &Clock) {
            self.1.increment(1);
        }
    }

    Rc::new(CounterAck(success.clone(), error.clone()))
}

struct DisconnectCache {
    min_age: Duration,
    sessions: quick_cache::unsync::Cache<SessionId, LastSeen>,
}

impl DisconnectCache {
    fn new() -> Self {
        let min_age = Duration::from_secs(60);
        let sessions = quick_cache::unsync::Cache::new(128);

        Self { sessions, min_age }
    }

    fn mark_session(&mut self, clock: &Clock, session_id: SessionId) -> bool {
        if let Some(mut it) = self.sessions.get_mut(&session_id) {
            return if it.age() > self.min_age {
                *it = clock.last_seen();
                true
            } else {
                false
            };
        }
        let _ = self.sessions.insert(session_id, clock.last_seen());
        true
    }
}
