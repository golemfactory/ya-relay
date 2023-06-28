use anyhow::bail;
use std::net::SocketAddr;
use std::sync::Arc;
use url::Url;

use crate::_client::Client;
use crate::_config::{ClientBuilder, ClientConfig, FailFast};
use crate::_network_view::{NetworkView, SessionLock, SessionPermit};
use crate::_session_layer::SessionLayer;
use crate::_session_protocol::SessionInitializer;
use crate::testing::accessors::SessionLayerPrivate;

use crate::testing::mocks::MockHandler;
use ya_relay_core::NodeId;
use ya_relay_server::testing::server::{init_test_server, ServerWrapper};

pub async fn test_default_config(server: Url) -> anyhow::Result<ClientConfig> {
    ClientBuilder::from_url(server).build_config().await
}

#[derive(Clone)]
pub struct MockSessionNetwork {
    pub server: ServerWrapper,
    pub layers: Vec<SessionLayerWrapper>,
    pub clients: Vec<Client>,
}

#[derive(Clone)]
pub struct SessionLayerWrapper {
    pub id: NodeId,
    pub addr: SocketAddr,

    pub layer: SessionLayer,
    pub capturer: MockHandler,
    pub protocol: SessionInitializer,
    pub guards: NetworkView,
}

impl MockSessionNetwork {
    pub async fn new() -> anyhow::Result<MockSessionNetwork> {
        Ok(MockSessionNetwork {
            server: init_test_server().await?,
            layers: vec![],
            clients: vec![],
        })
    }

    pub async fn new_layer(&mut self) -> anyhow::Result<SessionLayerWrapper> {
        let layer = spawn_session_layer(&self.server).await?;
        self.layers.push(layer.clone());
        Ok(layer)
    }

    pub async fn new_client(&mut self) -> anyhow::Result<Client> {
        let client = ClientBuilder::from_url(self.server.url())
            .connect(FailFast::Yes)
            .build()
            .await?;
        self.clients.push(client.clone());
        Ok(client)
    }

    /// If Client will reconnect to relay server, he will have public IP again.
    pub async fn hack_make_ip_private(&self, client: &Client) {
        self.hack_make_layer_ip_private_impl(&client.transport.session_layer)
            .await
    }

    pub async fn hack_make_layer_ip_private(&self, wrapper: &SessionLayerWrapper) {
        self.hack_make_layer_ip_private_impl(&wrapper.layer).await
    }

    /// If Client will reconnect to relay server, he will have public IP again.
    async fn hack_make_layer_ip_private_impl(&self, layer: &SessionLayer) {
        log::info!(
            "Making Node's {} IP private on relay server",
            layer.config.node_id
        );

        let mut state = self.server.server.state.write().await;

        let mut info = state.nodes.get_by_node_id(layer.config.node_id).unwrap();
        state.nodes.remove_session(info.info.slot);

        // Server won't return any endpoints, so Client won't try to connect directly.
        info.info.endpoints = vec![];
        state.nodes.register(info);

        drop(state);
        layer.set_public_addr(None).await;
    }
}

pub async fn spawn_session_layer(wrapper: &ServerWrapper) -> anyhow::Result<SessionLayerWrapper> {
    let config = Arc::new(test_default_config(wrapper.server.inner.url.clone()).await?);
    let mut layer = SessionLayer::new(config);
    let capturer = MockHandler::new(layer.clone());
    layer.spawn_with_dispatcher(capturer.clone()).await?;

    let node_id = layer.config.node_id;
    let addr = layer.get_test_socket_addr().await?;
    let protocol = layer.get_protocol().await?;
    let guards = layer.registry.clone();

    log::info!("Spawned `SessionLayer` for [{node_id}] ({addr})");
    Ok(SessionLayerWrapper {
        id: node_id,
        addr,
        layer,
        capturer,
        protocol,
        guards,
    })
}

impl SessionLayerWrapper {
    pub async fn start_session(&self, with: &SessionLayerWrapper) -> anyhow::Result<SessionPermit> {
        match self
            .guards
            .lock_outgoing(with.id, &[with.addr], self.layer.clone())
            .await
        {
            SessionLock::Permit(permit) => Ok(permit),
            SessionLock::Wait(_) => bail!("Expected initialization permit"),
        }
    }
}
