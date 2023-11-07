use clap::Parser;

use ya_relay_server::metrics::register_metrics;
use ya_relay_server::Config;

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
    let _server = ya_relay_server::run(&args).await?;
    log::info!("started");
    futures::future::pending::<()>().await;
    Ok(())
}
