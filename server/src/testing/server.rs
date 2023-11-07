use crate::config::Config;

use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::net;
use std::rc::Rc;

use crate::udp_server::UdpServer;
use tokio::time::Duration;
use url::Url;
use ya_relay_core::testing::TestServerWrapper;
use crate::server::{IpCheckerConfig, ServerConfig, SessionHandlerConfig};
use crate::SessionManagerConfig;

#[derive(Clone)]
pub struct ServerWrapper {
    pub server: Rc<UdpServer>,
}

impl<'a> TestServerWrapper<'a> for ServerWrapper {
    fn url(&self) -> Url {
        let addr = self.server.bind_addr();
        format!("udp://{addr}").parse().unwrap()
    }

    fn remove_node_endpoints(&'a self, _node: ya_relay_core::NodeId) -> LocalBoxFuture<'a, ()> {
        async move { todo!() }.boxed_local()
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
        server: ServerConfig {
            address: (net::Ipv4Addr::LOCALHOST, 0).into(),
            workers: 1,
            tasks_per_worker: 1,
        },
        session_manager: SessionManagerConfig {
            session_cleaner_interval: Duration::from_secs(10),
            session_purge_timeout: Duration::from_secs(20),
        },
        session_handler: SessionHandlerConfig { difficulty: 1, salt: None },
        ip_check: IpCheckerConfig {
            timeout: Duration::from_millis(300),
            retry_cnt: 1,
            retry_after: Duration::from_millis(100),
        },
    }
}

impl Drop for ServerWrapper {
    fn drop(&mut self) {
        self.server.stop_internal();
        log::debug!("[TEST] Dropping ServerWrapper.");
    }
}
