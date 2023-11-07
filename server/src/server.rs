use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use metrics::Counter;
use quick_cache::sync::Cache;
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
use crate::state::Clock;
use crate::udp_server::{
    worker_err_fn, PacketType, UdpServer, UdpServerBuilder, UdpSocket,
};
use crate::{Config, SessionManager};

mod neighbours;
mod session;

mod node;

mod slot;

mod forward;

mod register;

mod reverse_connection;

mod state_decoder;

mod ip_checker;

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

pub async fn run(config: &Config) -> anyhow::Result<UdpServer> {
    let bind_addr: SocketAddr = config.server.address;
    let session_manager = SessionManager::new();
    let slot_manager = SlotManager::new();
    let server_config = &config.server;
    let session_handler_config = config.session_handler.clone();
    let ip_check_config = config.ip_check.clone();

    session_manager.start_cleanup_processor(&config.session_manager);

    let ip_test_cache: IpCache =
        Arc::new(quick_cache::sync::Cache::<SocketAddr, (Instant, bool)>::new(128));

    let server = UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
        let session_manager = session_manager.clone();
        let slot_manager = slot_manager.clone();
        let checker_ip = reply.local_addr()?.ip();

        let session_handler = session::SessionHandler::new(&session_manager, &session_handler_config);
        let ip_checker = ip_check_config.build(checker_ip)?;
        let register_handler = register::RegisterHandler::new(&session_manager, &slot_manager, ip_checker, &reply, ip_test_cache.clone());
        let neighbours_handler = neighbours::NeighboursHandler::new(&session_manager, &slot_manager);
        let node_handler = node::NodeHandler::new(&session_manager, &slot_manager);
        let slot_handler = slot::SlotHandler::new(&session_manager, &slot_manager);
        let forward_handler = forward::ForwardHandler::new(&session_manager, &slot_manager, &reply);
        let rc_handler = reverse_connection::RcHandler::new(&session_manager, &reply);

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
                            let session_id= SessionId::from(session_id);
                            if let Some(session_ref) = session_manager.session(&session_id) {
                                if session_ref.peer == src {
                                    log::info!("[{src}] Unreachable {reason:?} removing session");
                                    session_manager.remove_session(&session_id);
                                }
                            }
                            None
                        }
                        PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Control(Control { kind: Some(control::Kind::ReverseConnection(_)) })) }) => {
                            session_id.try_into().ok().and_then(|session_id| session_manager.session(&session_id))
                                .and_then(|session_ref| {
                                    if session_ref.peer == src {
                                        log::info!("[{src}] Unreachable {reason:?} removing session");
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
                                session_id.and_then(|session_id| handle_ping(&clock, src, request_id, session_id, &session_manager))
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
        .start(bind_addr).await?;

    Ok(server)
}

fn handle_ping(
    clock: &Clock,
    src: SocketAddr,
    request_id: u64,
    session_id: SessionId,
    session_manager: &SessionManager,
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
        log::warn!(target: "request::ping", "[{src}] ping to unknown session");
        None
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
