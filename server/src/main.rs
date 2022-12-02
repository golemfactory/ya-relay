use ya_relay_server::config::Config;
use ya_relay_server::metrics::register_metrics;
use ya_relay_server::server::Server;

use chrono::Local;
use std::io::Write;
use structopt::StructOpt;

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info,ya_smoltcp=info".to_string()),
    );
    env_logger::Builder::new()
        .parse_default_env()
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:5} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.module_path().unwrap_or("<unnamed>"),
                record.args()
            )
        })
        .init();

    let args = Config::from_args();

    register_metrics(args.metrics_scrape_addr);

    let server = Server::bind_udp(args).await?;
    server.run().await
}
