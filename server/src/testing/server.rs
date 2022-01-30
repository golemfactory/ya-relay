use crate::config::Config;
use chrono::Local;
use futures::future::{AbortHandle, Abortable};
use futures::FutureExt;
use std::io::Write;
use std::time::Duration;
use url::Url;

use crate::server::Server;

pub struct ServerWrapper {
    pub server: Server,
    handle: AbortHandle,
}

impl ServerWrapper {
    pub fn url(&self) -> Url {
        self.server.inner.url.clone()
    }
}

pub async fn init_test_server() -> anyhow::Result<ServerWrapper> {
    // Initialize logger for all tests. Thi function will be called multiple times,
    // so we `try_init`.
    let _ = env_logger::Builder::new()
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
        .try_init();

    let server = Server::bind_udp(Config {
        address: Url::parse("udp://127.0.0.1:0")?,
        ip_checker_port: 0,
        session_cleaner_interval: Duration::from_secs(60),
        session_timeout: 10,
    })
    .await?;

    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    tokio::task::spawn_local(Abortable::new(
        server.clone().run().map(|result| {
            if let Err(e) = result {
                log::error!("Server error: {}", e);
            }
        }),
        abort_registration,
    ));

    Ok(ServerWrapper {
        server,
        handle: abort_handle,
    })
}

impl Drop for ServerWrapper {
    fn drop(&mut self) {
        self.server.inner.socket.clone().close_channel();
        self.handle.abort();

        log::debug!("[TEST] Dropping ServerWrapper.");
    }
}
