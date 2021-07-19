use anyhow::{anyhow, bail};
use bytes::BytesMut;
use env_logger;
use std::convert::TryFrom;
use std::net::SocketAddr;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

use ya_client_model::NodeId;
use ya_net_server::{parse_udp_url, SessionId};
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::*;
use ya_relay_proto::proto::packet::Kind;
use ya_relay_proto::proto::*;

#[derive(StructOpt)]
#[structopt(about = "NET Client")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(subcommand)]
    pub commands: Commands,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub enum Commands {
    Init(Init),
    FindNode(FindNode),
    Ping(Ping),
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct Ping {
    #[structopt(short = "s", long, env)]
    session_id: String,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct Init {
    #[structopt(short = "n", long, env)]
    pub node_id: NodeId,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct FindNode {
    #[structopt(short = "n", long, env)]
    node_id: NodeId,
    #[structopt(short = "s", long, env)]
    session_id: String,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();
    let addr = parse_udp_url(args.address);

    log::info!("Sending to server listening on: {}", addr);

    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let mut socket = UdpSocket::bind(local_addr).await?;

    match args.commands {
        Commands::Init(Init { node_id }) => init_session(&mut socket, &addr, node_id).await?,
        Commands::FindNode(opts) => find_node(&mut socket, &addr, opts).await?,
        Commands::Ping(opts) => ping(&mut socket, &addr, opts).await?,
    };

    Ok(())
}

async fn ping(socket: &mut UdpSocket, addr: &str, opts: Ping) -> anyhow::Result<()> {
    let session_id = hex::decode(opts.session_id)
        .map_err(|e| anyhow!("Failed to convert SessionId to hex. {}", e))?;

    send_packet(socket, addr, ping_packet(session_id)).await?;

    log::info!("Ping sent to server");

    let packet = receive_packet(socket)
        .await
        .map_err(|e| anyhow!("Didn't receive Pong response. Error: {}", e))?;

    match packet {
        PacketKind::Packet(Packet {
            kind: Some(kind), ..
        }) => match kind {
            Kind::Response(Response {
                kind: Some(kind),
                code,
            }) => match kind {
                response::Kind::Pong(_) => {
                    log::info!("Got ping from server.")
                }
                _ => bail!("Invalid Response."),
            },
            _ => bail!("Invalid Response."),
        },
        _ => bail!("Invalid Response."),
    };

    Ok(())
}

async fn find_node(socket: &mut UdpSocket, addr: &str, opts: FindNode) -> anyhow::Result<()> {
    let session_id = hex::decode(opts.session_id)
        .map_err(|e| anyhow!("Failed to convert SessionId to hex. {}", e))?;

    send_packet(socket, addr, node_packet(session_id, opts.node_id.clone())).await?;

    log::info!("Querying node: [{}] info from server.", opts.node_id);

    let packet = receive_packet(socket)
        .await
        .map_err(|e| anyhow!("Didn't receive FindNode response. Error: {}", e))?;

    match packet {
        PacketKind::Packet(Packet {
            kind: Some(kind), ..
        }) => match kind {
            Kind::Response(Response {
                kind: Some(kind),
                code,
            }) => match kind {
                response::Kind::Node(node_info) => {
                    log::info!("Got info for node: [{}]", opts.node_id)
                }
                _ => bail!("Invalid Response."),
            },
            _ => bail!("Invalid Response."),
        },
        _ => bail!("Invalid Response."),
    }

    Ok(())
}

async fn init_session(socket: &mut UdpSocket, addr: &str, node_id: NodeId) -> anyhow::Result<()> {
    let sent = send_packet(socket, addr, init_packet()).await?;

    log::info!("Init session packet sent ({} bytes).", sent);

    let packet = receive_packet(socket)
        .await
        .map_err(|e| anyhow!("Didn't receive challenge. Error: {}", e))?;

    log::info!("Decoded packet received from server.");

    let session = SessionId::try_from(match packet {
        PacketKind::Packet(Packet { session_id, kind }) => match kind {
            Some(packet::Kind::Control(Control { kind })) => match kind {
                Some(control::Kind::Challenge(control::Challenge { challenge, .. })) => session_id,
                _ => bail!("Expected Challenge packet."),
            },
            _ => bail!("Invalid packet kind."),
        },
        _ => bail!("Expected Control packet with challenge."),
    })?;

    log::info!(
        "Decoded packet is correct Challenge. Session id: {}",
        session
    );

    let sent = send_packet(socket, addr, session_packet(session.vec(), node_id)).await?;

    log::info!("Challenge response sent ({} bytes).", sent);

    Ok(())
}

async fn send_packet(
    socket: &mut UdpSocket,
    addr: &str,
    packet: PacketKind,
) -> anyhow::Result<usize> {
    let mut codec = Codec::default();
    let mut out_buf = BytesMut::new();

    codec.encode(packet, &mut out_buf)?;
    Ok(socket.send_to(&out_buf, addr).await?)
}

async fn receive_packet(socket: &mut UdpSocket) -> anyhow::Result<PacketKind> {
    let mut codec = Codec::default();
    let mut in_buf = BytesMut::new();

    in_buf.resize(MAX_PACKET_SIZE as usize, 0);

    let size = socket.recv(&mut in_buf).await?;
    in_buf.truncate(size);

    Ok(codec
        .decode(&mut in_buf)?
        .ok_or(anyhow!("Failed to decode packet."))?)
}

fn init_packet() -> PacketKind {
    PacketKind::Packet(Packet {
        session_id: vec![],
        kind: Some(packet::Kind::Request(Request {
            kind: Some(request::Kind::Session(request::Session {
                challenge_resp: vec![],
                node_id: vec![],
                public_key: vec![],
            })),
        })),
    })
}

fn ping_packet(session_id: Vec<u8>) -> PacketKind {
    PacketKind::Packet(Packet {
        session_id,
        kind: Some(packet::Kind::Request(Request {
            kind: Some(request::Kind::Ping(request::Ping {})),
        })),
    })
}

fn session_packet(session_id: Vec<u8>, node_id: NodeId) -> PacketKind {
    PacketKind::Packet(Packet {
        session_id,
        kind: Some(packet::Kind::Request(Request {
            kind: Some(request::Kind::Session(request::Session {
                challenge_resp: vec![0u8; 2048 as usize],
                node_id: node_id.into_array().to_vec(),
                public_key: vec![],
            })),
        })),
    })
}

fn node_packet(session_id: Vec<u8>, node_id: NodeId) -> PacketKind {
    PacketKind::Packet(Packet {
        session_id,
        kind: Some(packet::Kind::Request(Request {
            kind: Some(request::Kind::Node(request::Node {
                node_id: node_id.into_array().to_vec(),
                public_key: true,
            })),
        })),
    })
}
