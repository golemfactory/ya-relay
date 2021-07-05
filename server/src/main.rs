mod server;

use server::Server;

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::*;
use ya_relay_proto::proto::*;

use crate::server::parse_udp_url;
use bytes::BytesMut;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;
use tokio_util::codec::Decoder;

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
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

    log::info!("Server listening on: {}", addr);

    let sock = UdpSocket::bind(&addr).await?;
    let server = Server::new()?;

    let (mut input, _output) = sock.split();

    let mut codec = Codec::default();
    let mut buf = BytesMut::with_capacity(2048);
    buf.resize(2048, 0);

    loop {
        match input.recv_from(&mut buf).await {
            Ok((size, addr)) => {
                log::info!("Received {} bytes from {}", size, addr);
                match codec.decode(&mut buf) {
                    Ok(Some(packet)) => {
                        server.read().await.dispatch(packet).await?;
                    }
                    Ok(None) => log::warn!("Empty packet."),
                    Err(e) => log::error!("Error: {}", e),
                }
            }
            Err(e) => {
                log::error!("Error receiving data from socket. {}", e);
                return Ok(());
            }
        }
    }
}
