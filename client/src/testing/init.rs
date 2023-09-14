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
use ya_relay_core::testing::AbstractServerWrapper;
use ya_relay_core::NodeId;

pub async fn test_default_config(server: Url) -> anyhow::Result<ClientConfig> {
    ClientBuilder::from_url(server).build_config().await
}

impl SessionLayerPrivate for SessionLayer {
    fn get_protocol(&self) -> LocalBoxFuture<anyhow::Result<SessionInitializer>> {
        let myself = self.clone();
        async move { Ok(myself.get_protocol().await?) }.boxed_local()
    }

    fn get_test_socket_addr(&self) -> LocalBoxFuture<anyhow::Result<SocketAddr>> {
        let myself = self.clone();
        async move {
            if let Some(addr) = myself.get_local_addr().await {
                let port = addr.port();
                Ok(format!("127.0.0.1:{port}").parse()?)
            } else {
                bail!("Can't get local address.")
            }
        }
        .boxed_local()
    }
}

#[derive(Clone)]
pub struct MockSessionNetwork<'a, SERVER: AbstractServerWrapper<'a>> {
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

impl<'a, SERVER: AbstractServerWrapper<'a>> MockSessionNetwork<'a, SERVER> {
    pub fn new(server: SERVER) -> anyhow::Result<MockSessionNetwork<'a, SERVER>> {
        Ok(MockSessionNetwork {
            server,
            layers: vec![],
            clients: vec![],
            _phantom: Default::default(),
        })
    }

    pub async fn new_layer(&'a mut self) -> anyhow::Result<SessionLayerWrapper> {
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
    pub async fn hack_make_ip_private(&'a self, client: &Client) {
        self.hack_make_layer_ip_private_impl(&client.transport.session_layer)
            .await
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

pub async fn spawn_session_layer<'a, SERVER: AbstractServerWrapper<'a>>(
    wrapper: &SERVER,
) -> anyhow::Result<SessionLayerWrapper> {
    let config = Arc::new(test_default_config(wrapper.url()).await?);
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
