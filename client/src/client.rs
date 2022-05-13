use std::collections::HashSet;
use std::convert::TryFrom;
use std::iter::zip;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use derive_more::From;
use futures::channel::mpsc;
use futures::future::join_all;
use futures::{Future, FutureExt, SinkExt, TryFutureExt};
use tokio::sync::RwLock;
use url::Url;

use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider, PublicKey};
use ya_relay_core::error::InternalError;
use ya_relay_core::identity::Identity;
use ya_relay_core::udp_stream::resolve_max_payload_size;
use ya_relay_core::utils::parse_udp_url;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Forward, SlotId, MAX_TAG_SIZE};
use ya_relay_stack::{ChannelMetrics, NetworkConfig, SocketDesc, SocketState};

use crate::session::SessionDesc;
use crate::session_manager::SessionManager;

pub type ForwardSender = mpsc::Sender<Vec<u8>>;
pub type ForwardReceiver = tokio::sync::mpsc::UnboundedReceiver<Forwarded>;

pub const PCAP_FILE_ENV_VAR: &str = "YA_NET_PCAP_FILE";
const NEIGHBOURHOOD_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct ClientConfig {
    pub node_id: NodeId,
    pub node_pub_key: PublicKey,
    pub crypto: Rc<dyn CryptoProvider>,
    pub bind_url: Url,
    pub srv_addr: SocketAddr,
    pub auto_connect: bool,
    pub session_expiration: Duration,
    pub tcp_config: NetworkConfig,
    pub pcap_path: Option<PathBuf>,
    pub ping_measure_interval: Duration,

    pub session_request_timeout: Duration,
    pub challenge_request_timeout: Duration,

    pub reverse_connection_tmp_timeout: Duration,
    pub reverse_connection_real_timeout: Duration,
    pub incoming_session_timeout: Duration,
}

pub struct ClientBuilder {
    bind_url: Option<Url>,
    srv_url: Url,
    crypto: Option<Rc<dyn CryptoProvider>>,
    auto_connect: bool,
    session_expiration: Option<Duration>,
    vtcp_buffer_size: Option<usize>,
}

#[derive(Clone)]
pub struct Client {
    pub config: Arc<ClientConfig>,

    state: Arc<RwLock<ClientState>>,
    pub sessions: SessionManager,
}

pub(crate) struct ClientState {
    bind_addr: Option<SocketAddr>,
    neighbours: Option<Neighbourhood>,
}

impl Client {
    fn new(config: ClientConfig) -> Self {
        let config = Arc::new(config);

        let sessions = SessionManager::new(config.clone());
        let state = Arc::new(RwLock::new(ClientState {
            bind_addr: None,
            neighbours: None,
        }));

        Self {
            config,
            state,
            sessions,
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
        self.sessions.get_public_addr().await
    }

    pub async fn remote_id(&self, addr: &SocketAddr) -> Option<NodeId> {
        self.sessions.remote_id(addr).await
    }

    pub async fn sessions(&self) -> Vec<SessionDesc> {
        self.sessions
            .sessions()
            .await
            .into_iter()
            .map(|s| SessionDesc::from(s.as_ref()))
            .collect()
    }

    #[inline]
    pub fn sockets(&self) -> Vec<(SocketDesc, SocketState<ChannelMetrics>)> {
        self.sessions.virtual_tcp.sockets()
    }

    #[inline]
    pub fn metrics(&self) -> ChannelMetrics {
        self.sessions.virtual_tcp.metrics()
    }

    pub async fn forward_receiver(&self) -> Option<ForwardReceiver> {
        self.sessions.virtual_tcp.receiver().await
    }

    async fn spawn(&mut self) -> anyhow::Result<()> {
        log::debug!("[{}] starting...", self.node_id());

        let bind_addr = self.sessions.spawn().await?;

        {
            let mut state = self.state.write().await;
            state.bind_addr = Some(bind_addr);
        }

        if self.config.auto_connect {
            self.sessions.server_session().await?;
        }

        // Measure ping from time to time
        let this = self.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::time::delay_for(this.config.ping_measure_interval).await;
                this.ping_sessions().await;
            }
        });

        log::debug!("[{}] started", self.node_id());
        Ok(())
    }

    pub async fn forward(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        let node_id = match self.sessions.alias(&node_id).await {
            Some(target_id) => {
                log::trace!("Resolved id [{}] as an alias of [{}]", node_id, target_id);
                target_id
            }
            None => node_id,
        };

        log::trace!(
            "Forward reliable from [{}] to [{}]",
            self.config.node_id,
            node_id
        );

        // Establish session here, if it didn't existed.
        let session = self.sessions.optimal_session(node_id).await?;
        self.sessions.forward(session, node_id).await
    }

    pub async fn forward_unreliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        let node_id = match self.sessions.alias(&node_id).await {
            Some(target_id) => {
                log::trace!("Resolved id [{}] as an alias of [{}]", node_id, target_id);
                target_id
            }
            None => node_id,
        };

        log::trace!(
            "Forward unreliable from [{}] to [{}]",
            self.config.node_id,
            node_id
        );

        // Establish session here, if it didn't existed.
        let session = self.sessions.optimal_session(node_id).await?;
        self.sessions.forward_unreliable(session, node_id).await
    }

    pub async fn neighbours(&self, count: u32) -> anyhow::Result<Vec<NodeId>> {
        if let Some(neighbours) = {
            let state = self.state.read().await;
            state.neighbours.clone()
        } {
            if neighbours.nodes.len() as u32 >= count
                && neighbours.updated + NEIGHBOURHOOD_TTL > Instant::now()
            {
                return Ok(neighbours.nodes);
            }
        }

        log::debug!("Asking NET relay Server for neighborhood ({}).", count);

        let neighbours = self
            .sessions
            .server_session()
            .await?
            .neighbours(count)
            .await?;

        let nodes = neighbours
            .nodes
            .into_iter()
            .filter_map(|n| Identity::try_from(&n).map(|ident| ident.node_id).ok())
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
                    .map_err(|e| log::debug!("Checking disappeared neighbors failed. {}", e)),
            );
        }

        Ok(nodes)
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

        let server = self.sessions.server_session().await?;
        for neighbor in lost_neighbors {
            if self.sessions.is_p2p(&neighbor).await {
                continue;
            }

            log::debug!(
                "Neighborhood changed. Checking state of Node [{}] on relay Server.",
                neighbor
            );

            // If we can't find node on relay, most probably it lost connection.
            // We remove this Node, otherwise we will have problems to connect to it later,
            // because we will have outdated entry in our registry.
            if server.find_node(neighbor).await.is_err() {
                log::info!(
                    "Node [{}], which was earlier in our neighborhood, disconnected.",
                    neighbor
                );
                self.sessions.remove_node(neighbor).await;
            }
        }

        Ok(())
    }

    pub async fn broadcast(&self, data: Vec<u8>, count: u32) -> anyhow::Result<()> {
        let node_ids = self.neighbours(count).await?;

        log::debug!("Broadcasting message to {} node(s)", node_ids.len());

        let broadcast_futures = node_ids
            .iter()
            .map(|node_id| {
                let data = data.clone();
                let node_id = *node_id;

                async move {
                    log::trace!("Broadcasting message to [{}]", node_id);

                    match self.forward_unreliable(node_id).await {
                        Ok(mut forward) => {
                            if forward.send(data).await.is_err() {
                                bail!("Cannot broadcast to {}: channel closed", node_id);
                            }
                        }
                        Err(e) => {
                            bail!("Cannot broadcast to {}: {}", node_id, e);
                        }
                    };
                    anyhow::Result::<()>::Ok(())
                }
                .map_err(|e| log::debug!("Failed to broadcast: {}", e))
                .map(|_| ())
                .boxed_local()
            })
            .collect::<Vec<Pin<Box<dyn Future<Output = ()>>>>>();

        futures::future::join_all(broadcast_futures).await;
        Ok(())
    }

    pub async fn ping_sessions(&self) {
        let sessions = self.sessions.sessions().await;
        let ping_futures = sessions
            .iter()
            .map(|session| async move { session.ping().await.ok() })
            .collect::<Vec<_>>();

        futures::future::join_all(ping_futures).await;
    }

    // Returns connected NodeIds. Single Node can have many identities, so the second
    // tuple element contains main NodeId (default Id).
    pub async fn connected_nodes(&self) -> Vec<(NodeId, Option<NodeId>)> {
        let ids = self.sessions.list_identities().await;
        let aliases = join_all(
            ids.iter()
                .map(|id| self.sessions.alias(id))
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter();
        zip(ids.into_iter(), aliases).collect()
    }

    pub async fn reconnect_server(&self) {
        if self.sessions.drop_server_session().await {
            log::info!("Reconnecting to Hybrid NET relay server");
            let _ = self.sessions.server_session().await;
        }
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        log::info!("Shutting down Hybrid NET client.");

        self.sessions.shutdown().await
    }
}

impl ClientBuilder {
    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url: url,
            crypto: None,
            auto_connect: false,
            session_expiration: None,
            vtcp_buffer_size: None,
        }
    }

    pub fn crypto(mut self, provider: impl CryptoProvider + 'static) -> ClientBuilder {
        self.crypto = Some(Rc::new(provider));
        self
    }

    pub fn connect(mut self) -> ClientBuilder {
        self.auto_connect = true;
        self
    }

    pub fn listen(mut self, url: Url) -> ClientBuilder {
        self.bind_url = Some(url);
        self
    }

    pub fn expire_session_after(mut self, expiration: Duration) -> Self {
        self.session_expiration = Some(expiration);
        self
    }

    pub fn virtual_tcp_buffer_size_multiplier(mut self, multiplier: usize) -> Self {
        self.vtcp_buffer_size = Some(multiplier);
        self
    }

    pub async fn build(self) -> anyhow::Result<Client> {
        let bind_url = self
            .bind_url
            .unwrap_or_else(|| Url::parse("udp://0.0.0.0:0").unwrap());
        let crypto = self
            .crypto
            .unwrap_or_else(|| Rc::new(FallbackCryptoProvider::default()));

        let default_id = crypto.default_id().await?;
        let default_pub_key = crypto.get(default_id).await?.public_key().await?;
        let mtu = resolve_max_payload_size().await? - MAX_TAG_SIZE - Forward::header_size();
        let pcap_path = std::env::var(PCAP_FILE_ENV_VAR).ok().map(PathBuf::from);

        let mut client = Client::new(ClientConfig {
            node_id: default_id,
            node_pub_key: default_pub_key,
            crypto,
            bind_url,
            srv_addr: parse_udp_url(&self.srv_url)?.parse()?,
            auto_connect: self.auto_connect,
            session_expiration: self
                .session_expiration
                .unwrap_or_else(|| Duration::from_secs(25)),
            tcp_config: NetworkConfig {
                max_transmission_unit: mtu,
                buffer_size_multiplier: self.vtcp_buffer_size.unwrap_or(4),
            },
            pcap_path,
            ping_measure_interval: Duration::from_secs(300),
            session_request_timeout: Duration::from_millis(3000),
            challenge_request_timeout: Duration::from_millis(8000),
            reverse_connection_tmp_timeout: Duration::from_secs(3),
            reverse_connection_real_timeout: Duration::from_secs(13),
            incoming_session_timeout: Duration::from_secs(16),
        });

        client.spawn().await?;

        Ok(client)
    }
}

impl ClientConfig {
    pub async fn public_key(&self) -> Result<PublicKey, InternalError> {
        let crypto = self
            .crypto
            .get(self.node_id)
            .await
            .map_err(|e| InternalError::Generic(e.to_string()))?;
        crypto
            .public_key()
            .await
            .map_err(|e| InternalError::Generic(e.to_string()))
    }
}

#[derive(Clone)]
pub(crate) struct Neighbourhood {
    updated: Instant,
    nodes: Vec<NodeId>,
}

#[derive(Clone, Debug)]
pub struct Forwarded {
    pub reliable: bool,
    pub node_id: NodeId,
    pub payload: Vec<u8>,
}

#[derive(From, Clone)]
pub enum ForwardId {
    SlotId(SlotId),
    NodeId(NodeId),
}
