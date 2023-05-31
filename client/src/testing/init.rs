use anyhow::bail;
use std::net::SocketAddr;
use std::sync::Arc;
use url::Url;

use crate::_config::{ClientBuilder, ClientConfig};
use crate::_session_guard::{GuardedSessions, SessionLock, SessionPermit};
use crate::_session_layer::SessionLayer;
use crate::_session_protocol::SessionProtocol;
use crate::testing::accessors::SessionLayerPrivate;

use ya_relay_core::NodeId;
use ya_relay_server::testing::server::{init_test_server, ServerWrapper};

pub async fn test_default_config(server: Url) -> anyhow::Result<ClientConfig> {
    ClientBuilder::from_url(server).build_config().await
}

#[derive(Clone)]
pub struct MockSessionNetwork {
    pub server: ServerWrapper,
    pub layers: Vec<SessionLayerWrapper>,
}

#[derive(Clone)]
pub struct SessionLayerWrapper {
    pub id: NodeId,
    pub addr: SocketAddr,

    pub layer: SessionLayer,
    pub protocol: SessionProtocol,
    pub guards: GuardedSessions,
}

impl MockSessionNetwork {
    pub async fn new() -> anyhow::Result<MockSessionNetwork> {
        Ok(MockSessionNetwork {
            server: init_test_server().await?,
            layers: vec![],
        })
    }

    pub async fn new_layer(&mut self) -> anyhow::Result<SessionLayerWrapper> {
        let layer = spawn_session_layer(&self.server).await?;
        self.layers.push(layer.clone());
        Ok(layer)
    }
}

pub async fn spawn_session_layer(wrapper: &ServerWrapper) -> anyhow::Result<SessionLayerWrapper> {
    let config = Arc::new(test_default_config(wrapper.server.inner.url.clone()).await?);
    let mut layer = SessionLayer::new(config);
    layer.spawn().await?;

    let node_id = layer.config.node_id;
    let addr = layer.get_test_socket_addr().await?;
    let protocol = layer.get_protocol().await?;
    let guards = layer.guards.clone();

    log::info!("Spawned `SessionLayer` for [{node_id}] ({addr})");
    Ok(SessionLayerWrapper {
        id: node_id,
        addr,
        layer,
        protocol,
        guards,
    })
}

impl SessionLayerWrapper {
    pub async fn start_session(&self, with: &SessionLayerWrapper) -> anyhow::Result<SessionPermit> {
        match self.guards.lock_outgoing(with.id, &[with.addr]).await {
            SessionLock::Permit(permit) => Ok(permit),
            SessionLock::Wait(_) => bail!("Expected initialization permit"),
        }
    }
}
