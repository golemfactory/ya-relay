use anyhow::{anyhow, bail};
use futures::future::{join_all, AbortHandle};
use futures::{FutureExt, TryFutureExt};
use parking_lot::Mutex;
use rand::prelude::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::future::Future;
use std::iter::zip;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_proto::proto::Payload;

use crate::metrics::register_metrics;

pub use crate::config::{ClientBuilder, ClientConfig, FailFast};
pub use crate::error::SessionError;
pub use crate::model::{SessionDesc, SocketDesc, SocketState};
pub use crate::transport::transport_sender::{ForwardSender, GenericSender};
pub use crate::transport::{ForwardReceiver, TransportLayer};

use crate::direct_session::DirectSession;
use crate::metrics::ChannelMetrics;
use crate::model::NodeId;
pub use ya_relay_core::server_session::TransportType;

/// A Hybrid NET client that handles connections, sessions and relay operations.
/// It provides high-level methods for networking tasks such as forwarding
/// data, managing session metrics, pinging sessions, and broadcasting data.
/// Supports unreliable transport using Udp directly or reliable transport using embedded Tcp
/// protocol on top of Udp.
#[derive(Clone)]
pub struct Client {
    config: Arc<ClientConfig>,
    state: Arc<Mutex<ClientState>>,
    pub(crate) transport: TransportLayer,
}

/// The state of a Hybrid NET client, containing the address it is bound to,
/// its current neighbours and its abort handles.
pub(crate) struct ClientState {
    bind_addr: Option<SocketAddr>,
    neighbours: Option<Neighbourhood>,
    handles: Vec<AbortHandle>,
}

impl Client {
    pub(crate) fn new(config: ClientConfig) -> Self {
        let config = Arc::new(config);

        let transport = TransportLayer::new(config.clone());
        let state = Arc::new(Mutex::new(ClientState {
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

    /// Returns the unique identifier (`NodeId`) of the client.
    pub fn node_id(&self) -> NodeId {
        self.config.node_id
    }

    /// Real address on which Client is listening to incoming messages.
    /// If you passed address like `0.0.0.0:0` to `ClientBuilder::listen()`, than this function will
    /// return resolved version of this address.
    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .lock()
            .bind_addr
            .ok_or_else(|| anyhow!("client not started"))
    }

    /// Returns the public address (`SocketAddr`) of the client, as seen by
    /// the rest of the network. This address is usually set during the
    /// initialization process and can be used by other nodes to establish
    /// connections with this client.
    ///
    /// # Returns
    ///
    /// * `SocketAddr`: The public address of the client. This can be either an IPv4 or an IPv6 address.
    ///
    pub async fn public_addr(&self) -> Option<SocketAddr> {
        self.transport.session_layer.get_public_addr().await
    }

    /// Sets the public address (`SocketAddr`) of the client, as seen by
    /// the rest of the network. This address is used by other nodes to
    /// establish connections with this client. This function should be
    /// used with care as changing the public address can affect ongoing
    /// connections and overall network functionality.
    ///
    /// # Arguments
    ///
    /// * `addr: Option<SocketAddr>` - The new public address to be set for the client.
    ///     This can be either an IPv4 or an IPv6 address or None if node has no
    ///     public address.
    ///
    pub async fn set_public_addr(&self, addr: Option<SocketAddr>) {
        self.transport.session_layer.set_public_addr(None).await;
    }

    #[doc(hidden)]
    pub async fn remote_id(&self, addr: &SocketAddr) -> Option<NodeId> {
        self.transport.session_layer.remote_id(addr).await
    }

    /// Returns a vector of descriptions for all currently active sessions.
    /// A session represents an active connection with a remote node.
    /// Each description (`SessionDesc`) contains information about the session,
    /// such as the remote node's address, session type, and status.
    ///
    /// This method is asynchronous and it should be awaited to get the result.
    ///
    /// # Returns
    ///
    /// * `Vec<SessionDesc>`: A vector of session descriptions. If there are no active sessions,
    /// the vector will be empty.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ya_relay_client::ClientBuilder;
    ///
    /// #[actix_rt::main]
    /// async fn main() {
    ///     let client = ClientBuilder::from_url("udp://52.48.158.112:7477".parse().unwrap()).build().await.unwrap();
    ///     let sessions = client.sessions().await;
    ///     for session in sessions {
    ///         println!("Session: {:?}", session);
    ///     }
    /// }
    /// ```
    ///
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

    /// Returns information about a node given its identifier (`NodeId`).
    /// This method uses the underlying network protocol to find the node
    /// and collect information about it.
    ///
    /// # Arguments
    ///
    /// * `node_id: NodeId` - The identifier of the node to find.
    ///
    /// # Returns
    ///
    /// * `Option<NodeInfo>`: An option containing the `NodeInfo` if found,
    ///     or `None` if the node was not found.
    ///
    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<crate::model::Node> {
        let session = self.transport.session_layer.server_session().await?;
        session.raw.find_node(node_id).await
    }

    /// Returns a vector of all currently opened sockets.
    /// Each socket (`SocketInfo`) includes information such as its local and remote addresses,
    /// and the current state of the socket.
    pub fn sockets(&self) -> Vec<(SocketDesc, SocketState<crate::metrics::ChannelMetrics>)> {
        self.transport.virtual_tcp.sockets()
    }

    /// Returns a set of metrics for all currently active sessions.
    /// Each metric (`SessionMetric`) includes information about the session,
    /// such as the amount of data transferred, the duration of the session,
    /// and other relevant statistics.
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
            let mut g = self.state.lock();
            g.bind_addr = Some(bind_addr);
            drop(g);
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
            let mut g = self.state.lock();
            g.handles.push(ping_handle);
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

    /// TODO: Remove this.
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

    /// Disconnects from provided Node and all secondary identities.
    /// If we had p2p session with Node, it will be closed.
    pub async fn disconnect(&self, node_id: NodeId) -> Result<(), SessionError> {
        self.transport.session_layer.disconnect(node_id).await
    }

    pub async fn is_p2p(&self, node_id: NodeId) -> bool {
        self.transport.session_layer.is_p2p(node_id).await
    }

    pub async fn default_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.transport.session_layer.default_id(node_id).await
    }

    /// Broadcasts a byte array to a certain number of neighbours in the network.
    /// This method sends the same byte array to the specified number of neighbour nodes.
    ///
    /// # Arguments
    ///
    /// * `data: Vec<u8>` - The data to be broadcasted
    /// * `count: u32` - The number of neighbour nodes to broadcast the data to.
    ///
    /// # Returns
    ///
    /// * `anyhow::Result<()>`: A Result object indicating the success or failure of the broadcast operation.
    ///
    pub async fn broadcast(&self, data: Vec<u8>, count: u32) -> anyhow::Result<()> {
        let zero: NodeId = Default::default();

        let mut node_ids: HashSet<NodeId> = {
            let mut sessions = self.transport.session_layer.sessions().await;
            sessions.shuffle(&mut thread_rng());

            sessions
                .into_iter()
                .filter_map(|session| session.upgrade())
                .map(|session| session.owner.default_id)
                .filter(|&node_id| node_id != zero)
                .take(count as usize)
                .collect()
        };

        if node_ids.len() < count as usize {
            log::debug!("Querying for {} node(s)", count as usize - node_ids.len());
            let next_node_ids = self
                .neighbours(count)
                .await
                .map_err(|e| anyhow!("Unable to query neighbors: {e}"))?;

            for node_id in next_node_ids {
                node_ids.insert(node_id);
                if node_ids.len() >= count as usize {
                    break;
                }
            }
        }

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

    /// Retrieves a certain number of neighbour nodes in the network.
    /// This method returns a vector of NodeId objects representing neighbour nodes.
    ///
    /// # Arguments
    ///
    /// * `count: u32` - The number of neighbour nodes to be returned.
    ///
    /// # Returns
    ///
    /// * `anyhow::Result<Vec<NodeId>>`: A Result object containing a vector of neighbour NodeIds or an error.
    ///
    pub async fn neighbours(&self, count: u32) -> anyhow::Result<Vec<NodeId>> {
        if let Some(neighbours) = { self.state.lock().neighbours.clone() } {
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
            .neighbours(count, true)
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
            let mut g = self.state.lock();
            g.neighbours.replace(Neighbourhood {
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
        self.state.lock().neighbours = None;
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
            let mut g = self.state.lock();
            std::mem::take(&mut g.handles)
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
