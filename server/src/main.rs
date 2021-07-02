mod server;

use server::Server;

use crate::server::parse_udp_url;
use actix_service::fn_service;
use anyhow::Result;
use bytes::BytesMut;
use std::future::Future;
use std::io::Error;
use std::net::SocketAddr;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;

#[derive(StructOpt)]
#[structopt(about = "NET Server")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    address: url::Url,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    let args = Options::from_args();
    let addr = parse_udp_url(args.address);

    log::info!("Server listening on: {}", addr);

    let sock = UdpSocket::bind(&addr).await?;
    let server = Server::new()?;

    let (mut input, output) = sock.split();
    let mut buf = BytesMut::new();

    loop {
        match input.recv_from(&mut buf).await {
            Ok((size, addr)) => {
                log::info!("Received {} bytes from {}", size, addr);
                log::info!("Message:\n{:?}", buf);
            }
            Err(e) => {
                log::error!("Error receiving data from socket. {}", e);
                return Ok(());
            }
        }
    }
}
