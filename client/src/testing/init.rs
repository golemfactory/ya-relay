use anyhow::bail;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use url::Url;

use crate::client::Client;
use crate::config::{ClientBuilder, ClientConfig, FailFast};
use crate::session::network_view::{NetworkView, SessionLock, SessionPermit};
use crate::session::session_initializer::SessionInitializer;
use crate::session::SessionLayer;
use crate::testing::accessors::SessionLayerPrivate;

use crate::testing::mocks::MockHandler;
use ya_relay_core::testing::TestServerWrapper;
use ya_relay_core::NodeId;

pub async fn test_default_config(server: Url) -> anyhow::Result<ClientConfig> {
    ClientBuilder::from_url(server).build_config().await
}

impl SessionLayerPrivate for SessionLayer {
    fn get_protocol(&self) -> anyhow::Result<SessionInitializer> {
        let myself = self.clone();
        Ok(SessionLayer::get_protocol(&myself)?)
    }

    fn get_test_socket_addr(&self) -> anyhow::Result<SocketAddr> {
        let myself = self.clone();

        if let Some(addr) = myself.get_local_addr() {
            let port = addr.port();
            Ok(format!("127.0.0.1:{port}").parse()?)
        } else {
            bail!("Can't get local address.")
        }
    }
}

#[derive(Clone)]
pub struct MockSessionNetwork<'a, SERVER: TestServerWrapper<'a>> {
    pub server: SERVER,
    pub layers: Vec<SessionLayerWrapper>,
    pub clients: Vec<Client>,
    _phantom: PhantomData<&'a SERVER>,
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

impl<'a, SERVER: TestServerWrapper<'a>> MockSessionNetwork<'a, SERVER> {
    pub fn new(server: SERVER) -> anyhow::Result<MockSessionNetwork<'a, SERVER>> {
        Ok(MockSessionNetwork {
            server,
            layers: vec![],
            clients: vec![],
            _phantom: Default::default(),
        })
    }

    pub async fn new_layer(&mut self) -> anyhow::Result<SessionLayerWrapper> {
        let layer = spawn_session_layer(self.server.url()).await?;
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

    pub async fn hack_make_layer_ip_private(&'a self, wrapper: &SessionLayerWrapper) {
        self.hack_make_layer_ip_private_impl(&wrapper.layer).await
    }

    /// If Client will reconnect to relay server, he will have public IP again.
    async fn hack_make_layer_ip_private_impl(&'a self, layer: &SessionLayer) {
        log::info!(
            "Making Node's {} IP private on relay server",
            layer.config.node_id
        );
        self.server
            .remove_node_endpoints(layer.config.node_id)
            .await;
        layer.set_public_addr(None).await;
    }
}

pub async fn spawn_session_layer(url: Url) -> anyhow::Result<SessionLayerWrapper> {
    let config = Arc::new(test_default_config(url).await?);
    let mut layer = SessionLayer::new(config);
    let capturer = MockHandler::new(layer.clone());
    layer.spawn_with_dispatcher(capturer.clone()).await?;

    let node_id = layer.config.node_id;
    let addr = layer.get_test_socket_addr()?;
    let protocol = layer.get_protocol()?;
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
