use anyhow::{anyhow, bail};
use futures::future::{join_all, AbortHandle};
use futures::{FutureExt, TryFutureExt};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::future::Future;
use std::iter::zip;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::Payload;

use crate::metrics::register_metrics;

pub use crate::config::{ClientBuilder, ClientConfig, FailFast};
pub use crate::error::SessionError;
pub use crate::raw_session::SessionDesc;
pub use crate::transport::transport_sender::{ForwardSender, GenericSender};
pub use crate::transport::{ForwardReceiver, TransportLayer};

use crate::metrics::ChannelMetrics;
pub use ya_relay_core::server_session::TransportType;
pub use ya_relay_stack::{SocketDesc, SocketState};

#[derive(Clone)]
pub struct Client {
    config: Arc<ClientConfig>,
    state: Arc<RwLock<ClientState>>,
    pub(crate) transport: TransportLayer,
}

pub(crate) struct ClientState {
    bind_addr: Option<SocketAddr>,
    neighbours: Option<Neighbourhood>,
    handles: Vec<AbortHandle>,
}

impl Client {
    pub(crate) fn new(config: ClientConfig) -> Self {
        let config = Arc::new(config);

        let transport = TransportLayer::new(config.clone());
        let state = Arc::new(RwLock::new(ClientState {
            bind_addr: None,
            neighbours: None,
            handles: vec![],
        }));

        Self {
            config,
            state,
            transport,
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.config.node_id
    }

    /// Real address on which Client is listening to incoming messages.
    /// If you passed address like `0.0.0.0:0` to `ClientBuilder::listen()`, than this function will
    /// return resolved version of this address.
    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .read()
            .await
            .bind_addr
            .ok_or_else(|| anyhow!("client not started"))
    }

    pub async fn public_addr(&self) -> Option<SocketAddr> {
        self.transport.session_layer.get_public_addr().await
    }

    pub async fn set_public_addr(&self, addr: Option<SocketAddr>) {
        self.transport.session_layer.set_public_addr(None).await;
    }

    pub async fn remote_id(&self, addr: &SocketAddr) -> Option<NodeId> {
        self.transport.session_layer.remote_id(addr).await
    }

    pub async fn sessions(&self) -> Vec<SessionDesc> {
        self.transport
            .session_layer
            .sessions()
            .await
            .into_iter()
            .filter_map(|s| {
                s.upgrade()
                    .map(|direct| SessionDesc::from(direct.raw.as_ref()))
            })
            .collect()
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<crate::model::Node> {
        let session = self.transport.session_layer.server_session().await?;
        let find_node = session.raw.find_node(node_id);

        match tokio::time::timeout(Duration::from_secs(5), find_node).await {
            Ok(result) => Ok(result?),
            Err(_) => bail!("Node [{}] lookup timed out", node_id),
        }
    }

    #[inline]
    pub fn sockets(&self) -> Vec<(SocketDesc, SocketState<crate::metrics::ChannelMetrics>)> {
        self.transport.virtual_tcp.sockets()
    }

    #[inline]
    pub async fn session_metrics(&self) -> HashMap<NodeId, ChannelMetrics> {
        let mut session_metrics = HashMap::new();

        let sessions = self.transport.session_layer.sessions().await;
        let sockets = self.transport.virtual_tcp.sockets();
        for session in sessions {
            let session = match session.upgrade() {
                None => continue,
                Some(session) => session,
            };
            let node_id = match self.remote_id(&session.raw.remote).await {
                Some(node_id) => node_id,
                None => continue,
            };

            let virt_node = match self.transport.virtual_tcp.resolve_node(node_id).await {
                Ok(virt_node) => virt_node,
                Err(_) => continue,
            };

            sockets
                .iter()
                .filter_map(|(desc, metrics)| {
                    desc.remote
                        .ip_endpoint()
                        .ok()
                        .filter(|endpoint| endpoint.addr == virt_node.address)
                        .map(|_| metrics.clone().inner_mut().clone())
                })
                .reduce(|acc, item| acc + item)
                .and_then(|metrics| session_metrics.insert(node_id, metrics));
        }
        session_metrics
    }

    #[inline]
    pub fn metrics(&self) -> ChannelMetrics {
        self.transport.virtual_tcp.metrics()
    }

    pub async fn forward_receiver(&self) -> Option<ForwardReceiver> {
        self.transport.forward_receiver()
    }

    pub(crate) async fn spawn(&mut self) -> anyhow::Result<()> {
        register_metrics();

        log::debug!("[{}] starting...", self.node_id());

        let bind_addr = self.transport.spawn().await?;

        {
            let mut state = self.state.write().await;
            state.bind_addr = Some(bind_addr);
        }

        if self.config.auto_connect {
            // We don't want to exit here, since yagna probably will be able to connect to relay
            // later, so it is very inconvenient for users to exit early.
            if let Err(e) = self.transport.session_layer.server_session().await {
                if self.config.auto_connect_fail_fast {
                    return Err(e.into());
                } else {
                    log::warn!("Auto-connecting to relay on startup failed: {e}");
                }
            }
        }

        // Measure ping from time to time
        let this = self.clone();
        let ping_handle = spawn_local_abortable(async move {
            loop {
                tokio::time::sleep(this.config.ping_measure_interval).await;
                this.ping_sessions().await;
            }
        });

        {
            let mut state = self.state.write().await;
            state.handles.push(ping_handle);
        }

        log::debug!("[{}] started", self.node_id());
        Ok(())
    }

    pub async fn forward_reliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        log::trace!(
            "Forward reliable from [{}] to [{}]",
            self.config.node_id,
            node_id
        );
        self.transport.forward_reliable(node_id).await
    }

    pub async fn forward_transfer(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        log::trace!(
            "Forward transfer channel from [{}] to [{}]",
            self.config.node_id,
            node_id
        );

        self.transport.forward_transfer(node_id).await
    }

    pub async fn forward_unreliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        log::trace!(
            "Forward unreliable from [{}] to [{}]",
            self.config.node_id,
            node_id
        );
        self.transport.forward_unreliable(node_id).await
    }

    pub async fn ping_sessions(&self) {
        let sessions = self.transport.session_layer.sessions().await;
        let ping_futures = sessions
            .iter()
            .filter_map(|session| {
                session
                    .upgrade()
                    .map(|session| async move { session.raw.ping().await.ok() })
            })
            .collect::<Vec<_>>();

        futures::future::join_all(ping_futures).await;
    }

    // Returns connected NodeIds. Single Node can have many identities, so the second
    // tuple element contains main NodeId (default Id).
    pub async fn connected_nodes(&self) -> Vec<(NodeId, Option<NodeId>)> {
        let ids = self.transport.session_layer.list_connected().await;
        let aliases = join_all(
            ids.iter()
                .map(|id| self.transport.session_layer.default_id(*id))
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter();
        zip(ids.into_iter(), aliases).collect()
    }

    pub async fn reconnect_server(&self) {
        if self.transport.session_layer.close_server_session().await {
            log::info!("Reconnecting to Hybrid NET relay server");
            let _ = self.transport.session_layer.server_session().await;
        }
    }

    pub async fn broadcast(&self, data: Vec<u8>, count: u32) -> anyhow::Result<()> {
        let node_ids = self
            .neighbours(count)
            .await
            .map_err(|e| anyhow!("Unable to query neighbors: {e}"))?;

        log::debug!("Broadcasting message to {} node(s)", node_ids.len());

        let broadcast_futures = node_ids
            .iter()
            .map(|node_id| {
                let data = data.clone();
                let node_id = *node_id;

                async move {
                    log::trace!("Broadcasting message to [{node_id}]");

                    match self.forward_unreliable(node_id).await {
                        Ok(mut forward) => {
                            if forward.send(data.into()).await.is_err() {
                                bail!("Cannot broadcast to {node_id}: channel closed");
                            }
                        }
                        Err(e) => {
                            bail!("Cannot broadcast to {node_id}: {e}");
                        }
                    };
                    anyhow::Result::<()>::Ok(())
                }
                .map_err(|e| log::debug!("Failed to broadcast: {e}"))
                .map(|_| ())
                .boxed_local()
            })
            .collect::<Vec<Pin<Box<dyn Future<Output = ()>>>>>();

        futures::future::join_all(broadcast_futures).await;
        Ok(())
    }

    pub async fn neighbours(&self, count: u32) -> anyhow::Result<Vec<NodeId>> {
        if let Some(neighbours) = {
            let state = self.state.read().await;
            state.neighbours.clone()
        } {
            if neighbours.nodes.len() as u32 >= count
                && neighbours.updated + self.config.neighbourhood_ttl > Instant::now()
            {
                return Ok(neighbours.nodes);
            }
        }

        log::debug!("Asking NET relay Server for neighborhood ({count}).");

        let neighbours = self
            .transport
            .session_layer
            .server_session()
            .await
            .map_err(|e| anyhow!("Error establishing session with relay: {e}"))?
            .raw
            .neighbours(count, false)
            .await?;

        let nodes = neighbours
            .nodes
            .into_iter()
            .filter_map(|n| {
                n.identities
                    .get(0)
                    .and_then(|ident| NodeId::try_from(&ident.node_id).ok())
            })
            .collect::<Vec<_>>();

        let prev_neighborhood = {
            let mut state = self.state.write().await;
            state.neighbours.replace(Neighbourhood {
                updated: Instant::now(),
                nodes: nodes.clone(),
            })
        };

        // Compare neighborhood, to see which Nodes could have disappeared.
        if let Some(prev_neighbors) = prev_neighborhood {
            tokio::task::spawn_local(
                self.clone()
                    .check_nodes_connection(prev_neighbors, nodes.clone())
                    .map_err(|e| log::debug!("Checking disappeared neighbors failed. {e}")),
            );
        }

        Ok(nodes)
    }

    pub async fn invalidate_neighbourhood_cache(&self) {
        self.state.write().await.neighbours = None;
    }

    async fn check_nodes_connection(
        self,
        prev_neighbors: Neighbourhood,
        new_neighbors: Vec<NodeId>,
    ) -> anyhow::Result<()> {
        let prev_neighbors: HashSet<_> = prev_neighbors.nodes.into_iter().collect();
        let new_neighbors: HashSet<_> = new_neighbors.into_iter().collect();

        let lost_neighbors = prev_neighbors
            .difference(&new_neighbors)
            .cloned()
            .collect::<Vec<NodeId>>();

        if lost_neighbors.is_empty() {
            return Ok(());
        }

        let server = self.transport.session_layer.server_session().await?;
        for neighbor in lost_neighbors {
            if self.transport.session_layer.is_p2p(neighbor).await {
                continue;
            }

            log::debug!(
                "Neighborhood changed. Checking state of Node [{}] on relay Server.",
                neighbor
            );

            // If we can't find node on relay, most probably it lost connection.
            // We remove this Node, otherwise we will have problems to connect to it later,
            // because we will have outdated entry in our registry.
            if server.raw.find_node(neighbor).await.is_err() {
                log::info!(
                    "Node [{neighbor}], which was earlier in our neighborhood, disconnected."
                );
                self.transport.session_layer.disconnect(neighbor).await.ok();
            }
        }

        Ok(())
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        log::info!("Shutting down Hybrid NET client.");

        let handles = {
            let mut state = self.state.write().await;
            std::mem::take(&mut state.handles)
        };

        for handle in handles {
            handle.abort();
        }

        self.transport.shutdown().await
    }
}

#[derive(Clone)]
pub(crate) struct Neighbourhood {
    updated: Instant,
    nodes: Vec<NodeId>,
}

#[derive(Clone, Debug)]
pub struct Forwarded {
    pub transport: TransportType,
    pub node_id: NodeId,
    pub payload: Payload,
}

#[cfg(test)]
mod tests {
    use crate::client::{ClientBuilder, FailFast};

    use std::time::Duration;

    use ya_relay_core::utils::to_udp_url;
    #[cfg(feature = "mock")]
    use ya_relay_server::testing::server::init_test_server;

    /// Client should be able to use the same port after it was shutdown.
    /// If it doesn't, it means that socket wasn't dropped correctly.
    #[cfg(feature = "mock")]
    #[serial_test::serial]
    async fn test_clean_shutdown() -> anyhow::Result<()> {
        let wrapper = init_test_server().await?;

        let mut client = ClientBuilder::from_url(wrapper.url())
            .connect(FailFast::Yes)
            .expire_session_after(Duration::from_secs(2))
            .build()
            .await?;

        println!("Shutting down Client");

        let addr2 = client.bind_addr().await?;
        let crypto = client.config.crypto.clone();

        client.shutdown().await.unwrap();

        println!("Waiting for session cleanup.");

        // Wait expiration timeout + ping timeout + 1s margin
        tokio::time::sleep(Duration::from_secs(6)).await;

        println!("Starting client");

        // Start client on the same port as previously.
        let mut _client = ClientBuilder::from_url(wrapper.url())
            .listen(to_udp_url(addr2).unwrap())
            .crypto(crypto)
            .connect(FailFast::Yes)
            .expire_session_after(Duration::from_secs(2))
            .build()
            .await?;

        Ok(())
    }
}
