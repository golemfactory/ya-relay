use crate::config::Config;

use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::rc::Rc;
use std::{future, net};

use crate::server::{IpCheckerConfig, Server, ServerConfig, SessionHandlerConfig};
use crate::SessionManagerConfig;
use tokio::time::Duration;
use url::Url;
use ya_relay_core::testing::TestServerWrapper;

#[derive(Clone)]
pub struct ServerWrapper {
    pub server: Rc<Server>,
}

impl<'a> TestServerWrapper<'a> for ServerWrapper {
    fn url(&self) -> Url {
        let addr = self.server.bind_addr();
        format!("udp://{addr}").parse().unwrap()
    }

    fn remove_node_endpoints(&'a self, node: ya_relay_core::NodeId) -> LocalBoxFuture<'a, ()> {
        if let Some(session_ref) = self.server.session_manager.node_session(node) {
            session_ref.addr_status.lock().set_valid(false);
        }
        future::ready(()).boxed_local()
    }
}

pub async fn init_test_server() -> anyhow::Result<ServerWrapper> {
    init_test_server_with_config(test_default_config()).await
}

pub async fn init_test_server_with_config(config: Config) -> anyhow::Result<ServerWrapper> {
    let server = Rc::new(crate::run(&config).await?);

    Ok(ServerWrapper { server })
}

pub fn test_default_config() -> Config {
    Config {
        metrics_scrape_addr: (net::Ipv4Addr::LOCALHOST, 0).into(),
        state_dir: None,
        server: ServerConfig {
            address: (net::Ipv4Addr::LOCALHOST, 0).into(),
            workers: 1,
            tasks_per_worker: 1,
        },
        session_manager: SessionManagerConfig {
            session_cleaner_interval: Duration::from_secs(10),
            session_purge_timeout: Duration::from_secs(20),
        },
        session_handler: SessionHandlerConfig {
            difficulty: 1,
            salt: None,
        },
        ip_check: IpCheckerConfig {
            timeout: Duration::from_millis(300),
            retry_cnt: 1,
            retry_after: Duration::from_millis(100),
        },
    }
}

impl Drop for ServerWrapper {
    fn drop(&mut self) {
        self.server.stop();
        log::debug!("[TEST] Dropping ServerWrapper.");
    }
}
