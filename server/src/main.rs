mod server;
pub(crate) mod session;

use crate::server::parse_udp_url;
use server::Server;

use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::MAX_PACKET_SIZE;

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

    let (mut input, output) = sock.split();
    let server = Server::new(output)?;

    let mut codec = Codec::default();
    let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE as usize);

    loop {
        buf.resize(MAX_PACKET_SIZE as usize, 0);

        match input.recv_from(&mut buf).await {
            Ok((size, addr)) => {
                log::info!("Received {} bytes from {}", size, addr);

                buf.truncate(size);
                match codec.decode(&mut buf) {
                    Ok(Some(packet)) => {
                        server
                            .dispatch(addr, packet)
                            .await
                            .map_err(|e| log::error!("Packet dispatch failed. {}", e))
                            .ok();
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
