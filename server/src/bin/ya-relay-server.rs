use clap::Parser;
use ya_relay_server::Config;

#[cfg(any(feature = "metrics", feature = "ui"))]
use ya_relay_server::metrics::register_metrics;

#[cfg(feature = "ui")]
mod ui {
    use actix_web::dev::Payload;
    use actix_web::{
        delete, get, http, post, web, FromRequest, HttpRequest, HttpResponse, Responder, Scope,
    };
    use futures::prelude::*;
    use metrics_exporter_prometheus::PrometheusHandle;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use ya_relay_core::server_session::SessionId;
    use ya_relay_core::NodeId;
    use ya_relay_server::{
        AddrStatus, NodeSelector, Selector, ServerControl, SessionManager, SessionRef,
        SessionWeakRef,
    };

    #[derive(Serialize)]
    struct Status {
        sessions: usize,
        nodes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        auth: Option<bool>,
    }

    #[get("/status")]
    async fn status(
        sm: web::Data<Arc<SessionManager>>,
        auth: Option<Authorization>,
    ) -> impl Responder {
        let sessions = sm.num_sessions();
        let nodes = sm.num_nodes();
        let auth = auth.map(|_| true);

        web::Json(Status {
            sessions,
            nodes,
            auth,
        })
    }

    #[derive(Deserialize)]
    struct SessionsQuery {
        prefix: String,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct SessionInfo {
        #[serde(skip_serializing_if = "String::is_empty")]
        session_id: String,
        peer: SocketAddr,
        seen: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        supported_encryptions: Vec<String>,
        addr_status: String,
    }

    impl SessionInfo {
        fn from_ref(session_ref: Option<SessionRef>) -> Option<Self> {
            session_ref.map(|session_ref| SessionInfo {
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
        }

        fn from_ref_secure(session_ref: SessionWeakRef) -> Option<Self> {
            match Self::from_ref(session_ref.upgrade()) {
                Some(mut s) => {
                    s.session_id = Default::default();
                    s.supported_encryptions = Default::default();
                    Some(s)
                }
                None => None,
            }
        }
    }

    #[get("/nodes/{prefix}")]
    async fn nodes_list_prefix(
        sm: web::Data<Arc<SessionManager>>,
        query: web::Path<SessionsQuery>,
        auth: Option<Authorization>,
    ) -> Result<impl Responder, actix_web::Error> {
        let selector: NodeSelector = query
            .prefix
            .parse()
            .map_err(actix_web::error::ErrorBadRequest)?;

        let nodes: HashMap<NodeId, Vec<Option<SessionInfo>>> = sm
            .nodes_for(selector, 50)
            .into_iter()
            .map(|(node_id, sessions)| {
                (
                    node_id,
                    if auth.is_some() {
                        sessions
                            .into_iter()
                            .map(|s| SessionInfo::from_ref(s.upgrade()))
                            .collect()
                    } else {
                        sessions
                            .into_iter()
                            .map(SessionInfo::from_ref_secure)
                            .collect()
                    },
                )
            })
            .collect();

        Ok(web::Json(nodes))
    }

    #[get("/sessions/{id}")]
    async fn session_get(
        sm: web::Data<Arc<SessionManager>>,
        query: web::Path<(String,)>,
        _auth: Authorization,
    ) -> impl Responder {
        let session_id =
            SessionId::try_from(query.0.as_str()).map_err(actix_web::error::ErrorBadRequest)?;
        let session_info = SessionInfo::from_ref(sm.session(&session_id));
        Ok::<_, actix_web::Error>(web::Json(session_info))
    }

    #[delete("/sessions/{id}")]
    async fn session_remove(
        server_control: web::Data<ServerControl>,
        query: web::Path<(String,)>,
        _auth: Authorization,
    ) -> impl Responder {
        let session_id =
            SessionId::try_from(query.0.as_str()).map_err(actix_web::error::ErrorBadRequest)?;
        server_control
            .disconnect(session_id, false)
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok::<_, actix_web::Error>(HttpResponse::NoContent().finish())
    }

    #[post("/sessions/{id}/check")]
    async fn session_check_ip(
        server_control: web::Data<ServerControl>,
        query: web::Path<(String,)>,
        _auth: Authorization,
    ) -> impl Responder {
        let session_id =
            SessionId::try_from(query.0.as_str()).map_err(actix_web::error::ErrorBadRequest)?;
        let r = server_control
            .check_ip(session_id)
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok::<_, actix_web::Error>(web::Json(r))
    }

    struct Authorization {}

    impl FromRequest for Authorization {
        type Error = actix_web::Error;
        type Future = future::Ready<Result<Self, Self::Error>>;

        fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
            if req.connection_info().realip_remote_addr() == Some("127.0.0.1") {
                future::ready(Ok(Authorization {}))
            } else {
                future::ready(Err(actix_web::error::ErrorForbidden("denied")))
            }
        }
    }

    include!(concat!(env!("OUT_DIR"), "/ui.rs"));

    pub fn start_web_server(
        server: &ya_relay_server::Server,
        metrics_scrape_addr: SocketAddr,
        handle: PrometheusHandle,
    ) -> anyhow::Result<()> {
        let sessions = web::Data::new(server.sessions());
        let control = web::Data::new(server.control());

        let web_server = actix_web::HttpServer::new(move || {
            use actix_web::*;

            let handle2 = handle.clone();
            let handle = handle.clone();

            App::new()
                .app_data(sessions.clone())
                .app_data(control.clone())
                .service(nodes_list_prefix)
                .service(session_get)
                .service(session_remove)
                .service(session_check_ip)
                .service(status)
                .route(
                    "/metrics",
                    web::get().to(move || future::ready(handle.render())),
                )
                .route("/", web::get().to(move || future::ready(handle2.render())))
                .service(scope())
        })
        .workers(1)
        .worker_max_blocking_threads(1)
        .disable_signals()
        .bind(metrics_scrape_addr)?
        .run();

        actix_rt::spawn(web_server);
        Ok(())
    }
}

#[cfg(all(not(feature = "ui"), feature = "metrics"))]
mod ui {
    use futures::future;
    use metrics_exporter_prometheus::PrometheusHandle;
    use std::net::SocketAddr;

    pub fn start_web_server(
        _server: &ya_relay_server::Server,
        metrics_scrape_addr: SocketAddr,
        handle: PrometheusHandle,
    ) -> anyhow::Result<()> {
        let web_server = actix_web::HttpServer::new(move || {
            use actix_web::*;

            let handle = handle.clone();

            App::new().route(
                "/metrics",
                web::get().to(move || future::ready(handle.render())),
            )
        })
        .workers(1)
        .worker_max_blocking_threads(1)
        .disable_signals()
        .bind(metrics_scrape_addr)?
        .run();

        actix_rt::spawn(web_server);
        Ok(())
    }
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

    #[cfg(any(feature = "metrics", feature = "ui"))]
    let handle = register_metrics();

    let server = ya_relay_server::run(&args).await?;

    #[cfg(any(feature = "metrics", feature = "ui"))]
    ui::start_web_server(&server, args.metrics_scrape_addr, handle)?;

    log::info!("started");

    let _a = tokio::signal::ctrl_c().await;
    if let Some(state_dir) = &args.state_dir {
        log::info!("saving state to {state_dir:?}");
        server.save_state(state_dir)?;
    }
    Ok(())
}
