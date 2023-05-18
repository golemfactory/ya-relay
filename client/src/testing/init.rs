use std::sync::Arc;
use std::time::Duration;
use url::Url;

use ya_relay_core::challenge::CHALLENGE_DIFFICULTY;
use ya_relay_core::utils::parse_udp_url;

use crate::client::ClientConfig;
use crate::ClientBuilder;
use crate::_session_layer::SessionLayer;

use ya_relay_server::testing::server::{init_test_server, ServerWrapper};

pub async fn test_default_config(server: Url) -> anyhow::Result<ClientConfig> {
    ClientBuilder::from_url(server).build_config().await
}

pub async fn spawn_session_layer(wrapper: &ServerWrapper) -> anyhow::Result<SessionLayer> {
    let config = Arc::new(test_default_config(wrapper.server.inner.url.clone()).await?);
    let mut layer = SessionLayer::new(config);
    layer.spawn().await?;
    Ok(layer)
}
