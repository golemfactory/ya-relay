use clap::Parser;

use ya_relay_server::metrics::register_metrics;
use ya_relay_server::Config;

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info".to_string()),
    );
    env_logger::Builder::new()
        .parse_default_env()
        .format_timestamp_millis()
        .init();

    let args = Config::parse();

    register_metrics(args.metrics_scrape_addr);

    let server = ya_relay_server::run(&args).await?;
    log::info!("started");

    let _a = tokio::signal::ctrl_c().await;
    if let Some(state_dir) = &args.state_dir {
        server.save_state(state_dir)?;
    }
    Ok(())
}
