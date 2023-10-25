use metrics::Counter;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use quick_cache::sync::Cache;
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio_util::codec::Decoder;

use ya_relay_core::challenge;
use ya_relay_core::challenge::ChallengeDigest;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::utils::{parse_udp_url};
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{BytesMut, PacketKind};
use ya_relay_proto::proto::{
    control, packet, request, response, Control, Forward, Message, Packet, Request, Response,
    StatusCode,
};

use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::udp_server::{worker_fn, UdpServer, UdpServerBuilder};
use crate::{Config, SessionManager, SessionState};

mod neighbours;
mod session;

mod node;

mod slot;

mod forward;

mod register;

mod reverse_connection;

type IpCache = Arc<Cache<SocketAddr, (Instant, bool)>>;

pub async fn run(config: &Config) -> anyhow::Result<UdpServer> {
    let bind_addr: SocketAddr = parse_udp_url(&config.address)?.parse()?;
    let session_manager = SessionManager::new();
    let slot_manager = SlotManager::new();
    let session_cleaner_interval = config.session_cleaner_interval;
    let session_timeout = config.session_timeout;
    let session_purge_timeout = config.session_purge_timeout;

    session_manager.start_cleanup_processor(
        session_cleaner_interval,
        session_timeout,
        session_purge_timeout,
    );

    let ip_test_cache : IpCache = Arc::new(quick_cache::sync::Cache::<SocketAddr, (Instant, bool)>::new(128));

    let server = UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
        let session_manager = session_manager.clone();
        let slot_manager = slot_manager.clone();
        let checker_ip = reply.local_addr()?.ip();

        let session_handler = session::SessionHandler::new(16, &session_manager);
        let register_handler = register::RegisterHandler::new(&session_manager, &slot_manager, checker_ip, &reply, ip_test_cache.clone());
        let neighbours_handler = neighbours::NeighboursHandler::new(&session_manager, &slot_manager);
        let node_handler = node::NodeHandler::new(&session_manager, &slot_manager);
        let slot_handler = slot::SlotHandler::new(&session_manager, &slot_manager);
        let forward_handler = forward::ForwardHandler::new(&session_manager, &slot_manager, &reply);
        let rc_handler = reverse_connection::RcHandler::new(&session_manager, &reply);

        worker_fn(move |mut packet: BytesMut, src| {
            let mut codec = Codec;
            let reply = reply.clone();
            let p = codec.decode(&mut packet)?.ok_or_else(|| anyhow::anyhow!("invalid packet"))?;
            let clock = Clock::now();
            //let mut dst = src;

            //log::info!("message from {src} {p:?}");
            let response = match p {
                PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Request(Request { request_id, kind: Some(request) })) }) => {
                    let session_id: Option<SessionId> = session_id.try_into().ok();

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

                        other => {
                            log::info!(target: "request::invalid", "[{src}] got unsupported {other:?}");
                            None
                        }
                    }
                }
                PacketKind::Packet(Packet { session_id, kind: None }) => {
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
                    let session_id : Option<SessionId> = session_id.try_into().ok();
                    if let Some(session_id) = session_id {
                        session_manager.remove_session(&session_id);
                        log::debug!(target: "request:disconnect", "[{src}] session {session_id} disconnected");
                    }
                    None
                }
                PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Control(Control { kind: Some(control::Kind::ResumeForwarding(_)) }))})=> {
                    // ignore
                    None
                },

                    PacketKind::Forward(Forward {
                                        session_id,
                                        slot,
                                        flags,
                                        payload
                                    }) => {
                    let session_id = session_id.into();
                    forward_handler.handle(&clock, src, session_id, slot, flags, payload)
                }

                other => {
                    log::error!("unknown packet: {other:?}");
                    None
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
    }).max_tasks_per_worker(config.tasks_per_worker)
        .workers(config.workers)
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

        fn error(&self, clock: &Clock) {
            self.1.increment(1);
        }
    }

    Rc::new(CounterAck(success.clone(), error.clone()))
}
