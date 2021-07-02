use env_logger;
use std::net::SocketAddr;
use structopt::{clap, StructOpt};
use tokio::net::UdpSocket;

#[derive(StructOpt)]
#[structopt(about = "NET Client")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    address: String,
    #[structopt(short = "m", env)]
    message: String,
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Options::from_args();
    let addr = args.address;

    log::info!("Sending to server listening on: {}", addr);

    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let mut socket = UdpSocket::bind(local_addr).await?;

    log::info!("Sending message: {}", args.message);

    socket.send_to(args.message[..].as_bytes(), addr).await?;
    Ok(())
}
