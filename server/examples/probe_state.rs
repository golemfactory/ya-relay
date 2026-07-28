mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::bail;
use clap::Parser;
use futures::{stream, StreamExt};
use rand::random;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use ya_relay_proto::proto::{
    control, packet, request, response, Identity, Message, Packet, StatusCode, SESSION_ID_SIZE,
};
use ya_relay_server::{Selector, SessionManager};

use common::is_public_ip;

#[derive(Parser)]
#[command(about = "Probe public, address-valid nodes from a relay state file")]
struct Args {
    /// Path to the sessions_v2.state file.
    #[arg(default_value = "sessions_v2.state")]
    state_file: PathBuf,

    /// Maximum time to wait for a handshake response.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "1s")]
    timeout: Duration,

    /// Maximum number of probes running at once.
    #[arg(long, default_value_t = 64)]
    concurrency: usize,
}

struct Target {
    address: SocketAddr,
    node_ids: BTreeSet<String>,
}

struct ActiveTarget {
    target: Target,
    round_trip: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.concurrency == 0 {
        bail!("--concurrency must be greater than zero");
    }

    let session_manager = SessionManager::load(&args.state_file)?;
    let targets = collect_targets(&session_manager);
    let target_count = targets.len();
    let probe_node_id = random::<[u8; 20]>();

    let mut active = stream::iter(targets.into_values().map(|target| {
        let probe_timeout = args.timeout;
        async move {
            probe(target.address, probe_node_id, probe_timeout)
                .await
                .map(|round_trip| ActiveTarget { target, round_trip })
        }
    }))
    .buffer_unordered(args.concurrency)
    .filter_map(|result| async move { result })
    .collect::<Vec<_>>()
    .await;

    active.sort_unstable_by_key(|result| result.target.address);

    println!("node_id\tip\tport\trtt_ms");
    let mut active_node_count = 0;
    for result in &active {
        for node_id in &result.target.node_ids {
            println!(
                "{}\t{}\t{}\t{}",
                node_id,
                result.target.address.ip(),
                result.target.address.port(),
                result.round_trip.as_millis()
            );
            active_node_count += 1;
        }
    }

    eprintln!(
        "Active: {} node(s) on {} of {} endpoint(s)",
        active_node_count,
        active.len(),
        target_count
    );

    Ok(())
}

fn collect_targets(session_manager: &SessionManager) -> BTreeMap<SocketAddr, Target> {
    let mut targets = BTreeMap::new();

    for (node_id, sessions) in session_manager.nodes_for(Selector::All, usize::MAX) {
        for session in sessions.into_iter().filter_map(|session| session.upgrade()) {
            if !session.addr_status.lock().is_valid() || !is_public_ip(session.peer.ip()) {
                continue;
            }

            targets
                .entry(session.peer)
                .or_insert_with(|| Target {
                    address: session.peer,
                    node_ids: BTreeSet::new(),
                })
                .node_ids
                .insert(node_id.to_string());
        }
    }

    targets
}

async fn probe(
    address: SocketAddr,
    probe_node_id: [u8; 20],
    probe_timeout: Duration,
) -> Option<Duration> {
    let bind_address = match address.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let socket = UdpSocket::bind(bind_address).await.ok()?;

    // Supplying an identity is required by a peer node. Omitting a challenge
    // keeps this probe to the first, inexpensive round-trip of the handshake.
    let packet = Packet::request(
        Vec::new(),
        request::Session {
            identities: vec![Identity {
                node_id: probe_node_id.to_vec(),
                public_key: Vec::new(),
            }],
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        },
    );
    let request_id = match &packet.kind {
        Some(packet::Kind::Request(request)) => request.request_id,
        _ => return None,
    };
    let payload = packet.encode_to_vec();
    let started = Instant::now();

    socket.send_to(&payload, address).await.ok()?;

    let mut buffer = vec![0; 64 * 1024];
    let (size, source) = timeout(probe_timeout, socket.recv_from(&mut buffer))
        .await
        .ok()?
        .ok()?;
    if source != address {
        return None;
    }

    let response = Packet::decode(&buffer[..size]).ok()?;
    let session_id = match response {
        Packet {
            session_id,
            kind: Some(packet::Kind::Response(response)),
        } if session_id.len() == SESSION_ID_SIZE
            && response.request_id == request_id
            && response.code == StatusCode::Ok as i32
            && matches!(
                response.kind,
                Some(response::Kind::Session(response::Session {
                    challenge_req: Some(_),
                    ..
                }))
            ) =>
        {
            session_id
        }
        _ => return None,
    };
    let round_trip = started.elapsed();

    // Do not leave the remote peer waiting for the second handshake step.
    let disconnect = Packet::control(
        session_id.clone(),
        control::Disconnected {
            by: Some(control::disconnected::By::SessionId(session_id)),
        },
    );
    let _ = socket.send_to(&disconnect.encode_to_vec(), address).await;

    Some(round_trip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_first_session_handshake_response() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = responder.local_addr().unwrap();

        let responder_task = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            let (size, source) = responder.recv_from(&mut buffer).await.unwrap();
            let request = Packet::decode(&buffer[..size]).unwrap();
            let request_id = match request {
                Packet {
                    session_id,
                    kind: Some(packet::Kind::Request(request)),
                } if session_id.is_empty()
                    && matches!(request.kind, Some(request::Kind::Session(_))) =>
                {
                    request.request_id
                }
                packet => panic!("unexpected probe packet: {packet:?}"),
            };

            let response = Packet::response(
                request_id,
                vec![1; SESSION_ID_SIZE],
                StatusCode::Ok,
                response::Session {
                    challenge_req: Some(Default::default()),
                    ..Default::default()
                },
            );
            responder
                .send_to(&response.encode_to_vec(), source)
                .await
                .unwrap();
        });

        let result = probe(address, [7; 20], Duration::from_secs(1)).await;

        responder_task.await.unwrap();
        assert!(result.is_some());
    }
}
