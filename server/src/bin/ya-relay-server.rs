use std::convert::TryInto;
use ya_relay_server::config::Config;
use ya_relay_server::metrics::register_metrics;
use clap::Parser;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};
use ya_relay_core::challenge;
use ya_relay_core::challenge::ChallengeDigest;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::utils::parse_udp_url;
use ya_relay_proto::codec::datagram::Codec;
use ya_relay_proto::codec::{BytesMut, PacketKind};
use ya_relay_proto::proto::{
    control, packet, request, response, Control, Message, Packet, Request, Response, StatusCode,
};
use ya_relay_server::udp_server::{UdpServerBuilder, worker_fn};
use ya_relay_server::{SessionManager, SessionState};

fn is_session_start(packet: &PacketKind) -> Option<(u64, &request::Session)> {
    match packet {
        PacketKind::Packet(Packet {
            kind:
                Some(packet::Kind::Request(Request {
                    request_id,
                    kind: Some(request::Kind::Session(session)),
                })),
            ..
        }) => Some((*request_id, session)),
        _ => None,
    }
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info,smoltcp=info".to_string()),
    );
    env_logger::Builder::new()
        .parse_default_env()
        .format_timestamp_millis()
        .init();

    let args = Config::parse();

    register_metrics(args.metrics_scrape_addr);

    eprintln!("log level = {}", log::max_level());



    log::info!("started");
    tokio::time::sleep(Duration::from_secs(60)).await;
    server.stop();
    log::info!("stopping");
    //let server = Server::bind_udp(args).await?;
    // server.run().await
    futures::future::pending::<()>().await;
    Ok(())
}

