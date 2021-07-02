use std::sync::{Arc, RwLock};
use url::Url;

pub const DEFAULT_NET_PORT: u16 = 7464;

pub struct Server {}

impl Server {
    pub fn new() -> anyhow::Result<Arc<RwLock<Server>>> {
        Ok(Arc::new(RwLock::new(Server {})))
    }
}

pub fn parse_udp_url(url: Url) -> String {
    let host = url.host_str().expect("Needs host for NET URL");
    let port = url.port().unwrap_or(DEFAULT_NET_PORT);

    format!("{}:{}", host, port)
}
