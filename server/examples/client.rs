use anyhow::{anyhow, bail};
use env_logger;
use std::net::SocketAddr;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use std::convert::TryFrom;
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
    address: url::Url,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();
    let addr = parse_udp_url(args.address);

    log::info!("Sending to server listening on: {}", addr);

    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let mut socket = UdpSocket::bind(local_addr).await?;

    init_session(&mut socket, &addr).await?;
    Ok(())
}

async fn init_session(socket: &mut UdpSocket, addr: &str) -> anyhow::Result<()> {
    let mut codec = Codec::default();
    let mut out_buf = BytesMut::new();
    let mut in_buf = BytesMut::new();
    in_buf.resize(MAX_PACKET_SIZE as usize, 0);

    codec.encode(init_packet(), &mut out_buf)?;
    let sent = socket.send_to(&out_buf, addr).await?;

    log::info!("Init session packet sent ({} bytes).", sent);

    let size = socket.recv(&mut in_buf).await?;
    in_buf.truncate(size);

    let packet = codec
        .decode(&mut in_buf)?
        .ok_or(anyhow!("Didn't receive challenge"))?;

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

    let mut out_buf = BytesMut::new();

    codec.encode(session_packet(session.vec()), &mut out_buf)?;
    let sent = socket.send_to(&out_buf, addr).await?;

    log::info!("Challenge response sent ({} bytes).", sent);

    Ok(())
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

fn session_packet(session_id: Vec<u8>) -> PacketKind {
    PacketKind::Packet(Packet {
        session_id,
        kind: Some(packet::Kind::Request(Request {
            kind: Some(request::Kind::Session(request::Session {
                challenge_resp: vec![0u8; 2048 as usize],
                node_id: vec![],
                public_key: vec![],
            })),
        })),
    })
}
