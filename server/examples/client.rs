use env_logger;
use std::net::SocketAddr;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;

use bytes::BytesMut;
use tokio_util::codec::Encoder;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::*;
use ya_relay_proto::proto::*;

#[derive(StructOpt)]
#[structopt(about = "NET Client")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    address: String,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();
    let addr = args.address;

    log::info!("Sending to server listening on: {}", addr);

    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let mut socket = UdpSocket::bind(local_addr).await?;

    let mut codec = Codec::default();
    let mut buf = BytesMut::new();

    codec.encode(packet(), &mut buf)?;

    socket.send_to(&buf, addr).await?;
    Ok(())
}

fn packet() -> PacketKind {
    PacketKind::Packet(Packet {
        kind: Some(packet::Kind::Request(Request {
            session_id: Vec::new(),
            kind: Some(request::Kind::Session(request::Session {
                challenge_resp: vec![0u8; 2048 as usize],
                node_id: vec![],
                public_key: vec![],
            })),
        })),
    })
}
