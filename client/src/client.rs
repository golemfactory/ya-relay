use anyhow::{anyhow, bail};
use derive_more::From;
use futures::channel::mpsc;
use futures::{Future, FutureExt, SinkExt, TryFutureExt};
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use url::Url;

use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider, PublicKey};
use ya_relay_core::error::InternalError;
use ya_relay_core::utils::parse_udp_url;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::SlotId;

use crate::session_manager::SessionManager;

pub type ForwardSender = mpsc::Sender<Vec<u8>>;
pub type ForwardReceiver = tokio::sync::mpsc::UnboundedReceiver<Forwarded>;

const NEIGHBOURHOOD_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct ClientConfig {
    pub node_id: NodeId,
    pub node_pub_key: PublicKey,
    pub crypto: Rc<dyn CryptoProvider>,
    pub bind_url: Url,
    pub srv_addr: SocketAddr,
    pub auto_connect: bool,
}

pub struct ClientBuilder {
    bind_url: Option<Url>,
    srv_url: Url,
    crypto: Option<Rc<dyn CryptoProvider>>,
    auto_connect: bool,
}

#[derive(Clone)]
pub struct Client {
    config: Arc<ClientConfig>,

    state: Arc<RwLock<ClientState>>,
    pub sessions: SessionManager,
}

pub(crate) struct ClientState {
    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    public_addr: Option<SocketAddr>,
    bind_addr: Option<SocketAddr>,

    neighbours: Option<Neighbourhood>,
}

impl Client {
    fn new(config: ClientConfig) -> Self {
        let config = Arc::new(config);

        let sessions = SessionManager::new(config.clone());
        let state = Arc::new(RwLock::new(ClientState {
            public_addr: None,
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

    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .read()
            .await
            .bind_addr
            .ok_or_else(|| anyhow!("client not started"))
    }

    pub async fn public_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.public_addr
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
            let session = self.sessions.server_session().await?;
            let endpoints = session.register_endpoints(vec![]).await?;

            // If there is any (correct) endpoint on the list, that means we have public IP.
            match endpoints
                .into_iter()
                .find_map(|endpoint| endpoint.try_into().ok())
            {
                Some(addr) => self.state.write().await.public_addr = Some(addr),
                None => self.state.write().await.public_addr = None,
            }
        }

        log::debug!("[{}] started", self.node_id());
        Ok(())
    }

    pub async fn forward(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        log::trace!("Forward reliable {} to {}", self.config.node_id, node_id);

        // Establish session here, if it didn't existed.
        let session = self.sessions.optimal_session(node_id).await?;
        self.sessions.forward(session, node_id).await
    }

    pub async fn forward_unreliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        log::trace!("Forward unreliable {} to {}", self.config.node_id, node_id);

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

        let nodes = self
            .sessions
            .server_session()
            .await?
            .neighbours(count)
            .await?
            .nodes
            .into_iter()
            .filter_map(|n| NodeId::try_from(n.node_id.as_slice()).ok())
            .collect::<Vec<_>>();

        {
            let mut state = self.state.write().await;
            state.neighbours.replace(Neighbourhood {
                updated: Instant::now(),
                nodes: nodes.clone(),
            });
        }

        Ok(nodes)
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
                    log::trace!("Broadcasting message to {}", node_id);

                    match self.forward_unreliable(node_id).await {
                        Ok(mut forward) => {
                            if forward.send(data).await.is_err() {
                                bail!("Cannot broadcast to {}: channel closed", node_id);
                            }
                        }
                        Err(e) => {
                            bail!("Cannot broadcast to {}: channel error: {}", node_id, e);
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
}

impl ClientBuilder {
    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url: url,
            crypto: None,
            auto_connect: false,
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

    pub async fn build(self) -> anyhow::Result<Client> {
        let bind_url = self
            .bind_url
            .unwrap_or_else(|| Url::parse("udp://0.0.0.0:0").unwrap());
        let crypto = self
            .crypto
            .unwrap_or_else(|| Rc::new(FallbackCryptoProvider::default()));

        let default_id = crypto.default_id().await?;
        let default_pub_key = crypto.get(default_id).await?.public_key().await?;

        let mut client = Client::new(ClientConfig {
            node_id: default_id,
            node_pub_key: default_pub_key,
            crypto,
            bind_url,
            srv_addr: parse_udp_url(&self.srv_url)?.parse()?,
            auto_connect: self.auto_connect,
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
