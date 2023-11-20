use anyhow::bail;
use clap::{Parser, Subcommand};
use futures::prelude::*;
use rand::Rng;

use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use ya_relay_core::challenge;
use ya_relay_core::challenge::ChallengeDigest;
use ya_relay_core::crypto::{Crypto, CryptoProvider, FallbackCryptoProvider, PublicKey};
use ya_relay_core::server_session::SessionId;
use ya_relay_core::utils::ResultExt;
use ya_relay_proto::codec::BytesMut;

use ya_relay_proto::proto::{packet, request, response, Message, Packet, Request, Response};

#[derive(Parser)]
#[command(author, version, about = "Server crashing tool")]
struct Args {
    /// Server that we want to crash.
    target: SocketAddr,
    /// how many different sockets to use.
    #[arg(short, default_value = "16")]
    concurrent: usize,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "0s")]
    /// delay between requests.
    pub delay: Duration,
    #[command(subcommand)]
    command: GremlinCmd,
}

#[derive(Subcommand, Clone)]
enum GremlinCmd {
    /// Sends session open without continuation.
    FloodSessionOpen {},
    FloodRegister {},
}

async fn gen_crypto(n: usize) -> anyhow::Result<(Vec<PublicKey>, Vec<Rc<dyn Crypto>>)> {
    let pairs: Vec<_> = futures::stream::iter((0..n).map(anyhow::Ok))
        .then(|_| async {
            let bytes = rand::thread_rng().gen::<[u8; 32]>();
            let secret = ethsign::SecretKey::from_raw(&bytes)?;
            let public = secret.public();

            let provider = FallbackCryptoProvider::new(secret);
            let crypto = provider.get(provider.default_id().await?).await?;

            Ok::<_, anyhow::Error>((public, crypto))
        })
        .try_collect()
        .await?;

    Ok(pairs.into_iter().unzip())
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let local = tokio::task::LocalSet::new();

    env_logger::init();

    for worker in 0..args.concurrent {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let cmd = args.command.clone();
        let _: JoinHandle<anyhow::Result<()>> = local.spawn_local(async move {
            match cmd {
                GremlinCmd::FloodSessionOpen {} => {
                    let mut request_id = 1;
                    loop {
                        request_id += 1;
                        let packet = Packet {
                            session_id: Default::default(),
                            kind: Some(packet::Kind::Request(Request {
                                request_id,
                                kind: Some(request::Kind::Session(Default::default())),
                            })),
                        };
                        socket.send_to(&packet.encode_to_vec(), args.target).await?;
                        if !args.delay.is_zero() {
                            tokio::time::sleep(args.delay).await;
                        }
                    }
                }
                GremlinCmd::FloodRegister {} => {
                    let mut request_id = 1;
                    let mut buffer = BytesMut::new();
                    let (_keys, signers) = gen_crypto(1).await?;
                    loop {
                        if !args.delay.is_zero() {
                            tokio::time::sleep(args.delay).await;
                        }

                        let _ = tokio::time::timeout(Duration::from_secs(4), async {
                            request_id += 1;
                            let request_packet = Packet {
                                session_id: Default::default(),
                                kind: Some(packet::Kind::Request(Request {
                                    request_id,
                                    kind: Some(request::Kind::Session(Default::default())),
                                })),
                            };
                            log::info!("{}:{} challenge request", worker, request_id);
                            socket
                                .send_to(&request_packet.encode_to_vec(), args.target)
                                .await?;
                            drop(request_packet);

                            buffer.reserve(64000);

                            let (_s, _src) = socket.recv_buf_from(&mut buffer).await?;
                            log::info!("{}:{} challenge response", worker, request_id);
                            let packet_bytes = buffer.split();
                            let response_packet = Packet::decode(packet_bytes)?;
                            let (challenge_resp, session_id) = match response_packet {
                                Packet {
                                    session_id,
                                    kind:
                                        Some(packet::Kind::Response(Response {
                                            request_id,
                                            code,
                                            kind:
                                                Some(response::Kind::Session(response::Session {
                                                    challenge_req: Some(challenge),
                                                    challenge_resp: _,
                                                    supported_encryptions: _,
                                                    identities: _,
                                                })),
                                        })),
                                } => {
                                    log::info!("{}:{} got challenge {}", worker, request_id, code);
                                    (
                                        challenge::solve::<ChallengeDigest, _>(
                                            challenge.challenge,
                                            challenge.difficulty,
                                            signers.clone(),
                                        )
                                        .await?,
                                        session_id,
                                    )
                                }
                                p => {
                                    log::error!(
                                        "{}:{} invalid response challenge {:?}",
                                        worker,
                                        request_id,
                                        p
                                    );
                                    bail!("unexpected packet");
                                }
                            };
                            request_id += 1;
                            let solve_packet = Packet {
                                session_id: session_id.clone(),
                                kind: Some(packet::Kind::Request(Request {
                                    request_id,
                                    kind: Some(request::Kind::Session(request::Session {
                                        challenge_resp: Some(challenge_resp),
                                        ..Default::default()
                                    })),
                                })),
                            };
                            socket
                                .send_to(&solve_packet.encode_to_vec(), args.target)
                                .await?;
                            log::info!("{}:{} challenge solved", worker, request_id);
                            drop(solve_packet);

                            loop {
                                let (_s, _src) = socket.recv_buf_from(&mut buffer).await?;
                                let packet_bytes = buffer.split();
                                let response_packet = Packet::decode(packet_bytes)?;
                                match response_packet {
                                    Packet {
                                        session_id,
                                        kind:
                                            Some(packet::Kind::Response(Response {
                                                request_id: _,
                                                code: _,
                                                kind:
                                                    Some(response::Kind::Session(response::Session {
                                                        challenge_req: None,
                                                        challenge_resp: None,
                                                        ..
                                                    })),
                                            })),
                                    } => {
                                        let session_id: Option<SessionId> =
                                            session_id.try_into().ok();
                                        log::info!("session established: {:?}", session_id);
                                        break;
                                    }
                                    p => {
                                        log::info!(
                                            "{}:{} unexpected packet {:?}",
                                            worker,
                                            request_id,
                                            p
                                        );
                                        continue;
                                    }
                                };
                            }

                            request_id += 1;
                            let register_packet = Packet {
                                session_id: session_id.clone(),
                                kind: Some(packet::Kind::Request(Request {
                                    request_id,
                                    kind: Some(request::Kind::Register(request::Register {
                                        ..Default::default()
                                    })),
                                })),
                            };
                            socket
                                .send_to(&register_packet.encode_to_vec(), args.target)
                                .await?;
                            log::info!("{}:{} register send", worker, request_id);
                            let (_s, _src) = socket.recv_buf_from(&mut buffer).await?;
                            let packet_bytes = buffer.split();
                            let response_packet = Packet::decode(packet_bytes)?;
                            log::info!(
                                "{}:{} register done: {:?}",
                                worker,
                                request_id,
                                response_packet
                            );
                            Ok(())
                        })
                        .await
                        .on_error(|_e| log::error!("{} timeout", worker));
                    }
                }
            }
        });
    }

    local.await;
    Ok(())
}
