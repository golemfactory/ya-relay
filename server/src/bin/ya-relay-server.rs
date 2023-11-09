use clap::Parser;
use std::collections::HashMap;
use std::future;
use std::net::SocketAddr;
use std::sync::Arc;

use actix_web::{get, web, Responder};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_server::metrics::register_metrics;
use ya_relay_server::{AddrStatus, Config, Selector, SessionManager};

#[get("/sessions")]
async fn sessions_list(sm: web::Data<Arc<SessionManager>>) -> impl Responder {
    format!("sessions: {}", sm.num_sessions())
}

#[derive(Deserialize)]
struct SessionsQuery {
    prefix: String,
}
#[get("/nodes/{prefix}")]
async fn nodes_list_prefix(
    sm: web::Data<Arc<SessionManager>>,
    query: web::Path<SessionsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct SessionInfo {
        session_id: String,
        peer: SocketAddr,
        seen: String,
        supported_encryptions: Vec<String>,
        addr_status: String,
    }

    let selector: Selector = query
        .prefix
        .parse()
        .map_err(actix_web::error::ErrorBadRequest)?;
    let nodes: HashMap<NodeId, Vec<Option<SessionInfo>>> = sm
        .nodes_for(selector, 50)
        .into_iter()
        .map(|(node_id, sessions)| {
            (
                node_id,
                sessions
                    .into_iter()
                    .map(|session_ref| {
                        session_ref.upgrade().map(|session_ref| SessionInfo {
                            session_id: session_ref.session_id.to_string(),
                            peer: session_ref.peer,
                            seen: format!("{:?}", session_ref.ts.age()),
                            supported_encryptions: session_ref.supported_encryptions.clone(),
                            addr_status: match &*session_ref.addr_status.lock() {
                                AddrStatus::Unknown => "Unknown".to_owned(),
                                AddrStatus::Pending(ts) => format!("pending({:?})", ts.elapsed()),
                                AddrStatus::Invalid(ts) => format!("invalid({:?})", ts.elapsed()),
                                AddrStatus::Valid(ts) => format!("valid({:?})", ts.elapsed()),
                            },
                        })
                    })
                    .collect(),
            )
        })
        .collect();
    Ok(web::Json(nodes))
}

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

    let handle = register_metrics();

    let server = ya_relay_server::run(&args).await?;

    let sessions = web::Data::new(server.sessions());

    let web_server = actix_web::HttpServer::new(move || {
        use actix_web::*;

        let handle = handle.clone();

        App::new()
            .app_data(sessions.clone())
            .service(nodes_list_prefix)
            .service(sessions_list)
            .route("/", web::get().to(move || future::ready(handle.render())))
    })
    .workers(1)
    .worker_max_blocking_threads(1)
    .disable_signals()
    .bind(args.metrics_scrape_addr)?
    .run();

    actix_rt::spawn(web_server);

    log::info!("started");

    let _a = tokio::signal::ctrl_c().await;
    if let Some(state_dir) = &args.state_dir {
        log::info!("saving state to {state_dir:?}");
        server.save_state(state_dir)?;
    }
    Ok(())
}
