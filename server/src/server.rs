use std::net::SocketAddr;
use std::rc::Rc;
use tokio::net::UdpSocket;
use tokio_util::codec::Decoder;
use ya_relay_core::challenge;
use ya_relay_core::challenge::ChallengeDigest;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::utils::parse_udp_url;
use ya_relay_proto::codec::{BytesMut, PacketKind};
use ya_relay_proto::codec::datagram::Codec;
use crate::{Config, SessionManager, SessionState};
use crate::udp_server::{UdpServer, UdpServerBuilder, worker_fn};
use ya_relay_proto::proto::{
    control, packet, request, response, Control, Message, Packet, Request, Response, StatusCode,
};
use crate::state::Clock;

mod session;

pub async fn run(config: &Config) -> anyhow::Result<UdpServer> {
    let bind_addr: SocketAddr = parse_udp_url(&config.address)?.parse()?;
    let session_manager = SessionManager::new();

    let server = UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
        let session_manager = session_manager.clone();
        let session_handler = session::SessionHandler::new(16, &session_manager);

        worker_fn(move |mut packet: BytesMut, src| {
            let mut codec = Codec;
            let reply = reply.clone();
            let p = codec.decode(&mut packet)?.ok_or_else(|| anyhow::anyhow!("invalid packet"))?;
            let clock = Clock::now();

            let response = match p {
                PacketKind::Packet(Packet { session_id, kind: Some(packet::Kind::Request(Request { request_id, kind: Some(request) }))}) => {
                    let session_id: Option<SessionId> = session_id.try_into().ok();

                    match request {
                        request::Kind::Session(session) => {
                            session_handler.handle(&clock, src, request_id, session_id, &session)
                        }
                        request::Kind::Ping(_) => {
                            session_id.and_then(|session_id| handle_ping(&clock, src, request_id, session_id, &session_manager))
                        }
                        request::Kind::Neighbours(_) => {
                            log::info!(target: "request::neighbours", "[{src}] respond with default");
                            session_id.map(|session_id| Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::Ok.into(),
                                    request_id,
                                    kind: Some(response::Kind::Neighbours(Default::default()))
                                }))
                            })
                        }
                        request::Kind::Register(register) => {
                            log::info!(target: "request::register", "[{src}] respond with default for {register:?}");
                            session_id.map(|session_id| Packet {
                                session_id: session_id.to_vec(),
                                kind: Some(packet::Kind::Response(Response {
                                    code: StatusCode::Ok.into(),
                                    request_id,
                                    kind: Some(response::Kind::Register(Default::default()))
                                }))
                            })
                        }
                        request::Kind::Slot(slot) => {
                            None
                        }
                        _ => None
                    }
                }
                PacketKind::Packet(Packet { session_id, kind: None}) => {
                    log::debug!(target: "request::error", "[{src}] unrecognized packet");
                    None
                }
                other => {
                    log::error!("unknown packet: {other:?}");
                    None
                }
            };

            let bytes = response.map(|p| p.encode_to_vec());

            Ok(async move {
                if let Some(bytes) = bytes {
                    reply.send_to(&bytes, src).await?;
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
) -> Option<Packet> {
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
        Some(Packet {
            session_id: session_id.to_vec(),
            kind: Some(packet::Kind::Response(Response {
                code: StatusCode::Ok.into(),
                request_id,
                kind: Some(response::Kind::Pong(Default::default())),
            })),
        })
    } else {
        log::warn!(target: "request::ping", "[{src}] ping to unknown session");
        None
    }
}
