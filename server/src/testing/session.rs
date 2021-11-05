use anyhow::{anyhow, bail};
use std::convert::TryFrom;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::challenge;
use crate::challenge::{prepare_challenge_request, CHALLENGE_DIFFICULTY};
use crate::testing::client::{ForwardId, ForwardSender};
use crate::testing::Client;
use crate::SessionId;

use ya_client_model::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::SlotId;

const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

#[derive(Clone)]
pub struct Session {
    remote_addr: SocketAddr,
    client: Client,
    pub state: Arc<RwLock<Option<SessionState>>>,
}

pub struct SessionState {
    pub id: SessionId,
}

impl Session {
    pub fn new(remote_addr: SocketAddr, client: Client) -> Self {
        Self {
            client,
            remote_addr,
            state: Default::default(),
        }
    }

    pub async fn id(&self) -> anyhow::Result<SessionId> {
        let state = self.state.read().await;
        Ok(state.as_ref().ok_or_else(|| anyhow!("Not connected"))?.id)
    }

    pub async fn init(&self) -> anyhow::Result<SessionId> {
        self.client.init_server_session(self.remote_addr).await
    }

    pub async fn close(self) {
        self.client.remove_session(self.remote_addr).await;
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let session_id = self.id().await?;
        self.client
            .register_endpoints(self.remote_addr, session_id, endpoints)
            .await
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.id().await?;
        self.client
            .find_node(self.remote_addr, session_id, node_id)
            .await
    }

    pub async fn find_slot(&self, slot: SlotId) -> anyhow::Result<proto::response::Node> {
        let session_id = self.id().await?;
        self.client
            .find_slot(self.remote_addr, session_id, slot)
            .await
    }

    pub async fn neighbours(&self, count: u32) -> anyhow::Result<proto::response::Neighbours> {
        let session_id = self.id().await?;
        self.client
            .neighbours(self.remote_addr, session_id, count)
            .await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let session_id = self.id().await?;
        self.client.ping(self.remote_addr, session_id).await
    }

    pub async fn forward(&self, forward_id: impl Into<ForwardId>) -> anyhow::Result<ForwardSender> {
        let _ = self.id().await?;
        self.client.forward(self.remote_addr, forward_id).await
    }

    pub async fn forward_unreliable(
        &self,
        forward_id: impl Into<ForwardId>,
    ) -> anyhow::Result<ForwardSender> {
        let _ = self.id().await?;
        self.client
            .forward_unreliable(self.remote_addr, forward_id)
            .await
    }

    pub async fn broadcast(&self, data: Vec<u8>, count: u32) -> anyhow::Result<()> {
        let session_id = self.id().await?;
        self.client
            .broadcast(self.remote_addr, session_id, data, count)
            .await?;
        Ok(())
    }
}

impl Client {
    async fn init_session(&self, addr: SocketAddr, challenge: bool) -> anyhow::Result<SessionId> {
        let id = self.id();
        log::info!("[{}] initializing session with {}", id, addr);

        let (request, raw_challenge) = prepare_challenge_request();
        let request = match challenge {
            true => request,
            false => proto::request::Session::default(),
        };

        let response = self
            .request::<proto::response::Session>(
                request.into(),
                vec![],
                SESSION_REQUEST_TIMEOUT,
                addr,
            )
            .await?;

        let crypto = self.crypto().await?;
        let public_key = crypto.public_key().await?;

        let packet = response.packet.challenge_req.ok_or_else(|| {
            anyhow!(
                "Expected ChallengeRequest while initializing session with {}",
                addr
            )
        })?;

        let challenge_resp =
            challenge::solve(packet.challenge.as_slice(), packet.difficulty, crypto).await?;

        let session_id = SessionId::try_from(response.session_id.clone())?;
        let node_id = self.node_id().await;

        let packet = proto::request::Session {
            challenge_resp,
            challenge_req: None,
            node_id: node_id.into_array().to_vec(),
            public_key: public_key.bytes().to_vec(),
        };
        let response = self
            .request::<proto::response::Session>(
                packet.into(),
                session_id.to_vec(),
                SESSION_REQUEST_TIMEOUT,
                addr,
            )
            .await?;

        if session_id != &response.session_id[..] {
            log::error!(
                "[{}] init session id mismatch: {} vs {:?} (response)",
                id,
                session_id,
                response.session_id
            )
        }

        if challenge {
            // Validate the challenge
            if !challenge::verify(
                &raw_challenge,
                CHALLENGE_DIFFICULTY,
                &response.packet.challenge_resp,
                response.packet.public_key.as_slice(),
            )
            .map_err(|e| anyhow!("Invalid challenge: {}", e))?
            {
                bail!("Challenge verification failed.")
            }
        }

        {
            let mut state = self.state.write().await;
            let session = state
                .sessions
                .entry(addr)
                .or_insert_with(|| Session::new(addr, self.clone()));
            let mut state = session.state.write().await;
            state.replace(SessionState { id: session_id });
        }

        log::info!("[{}] session initialized with {}", id, addr);
        Ok(session_id)
    }

    async fn init_p2p_session(&self, addr: SocketAddr) -> anyhow::Result<SessionId> {
        self.init_session(addr, true).await
    }

    async fn init_server_session(&self, addr: SocketAddr) -> anyhow::Result<SessionId> {
        self.init_session(addr, false).await
    }

    async fn remove_session(&self, addr: SocketAddr) {
        let mut state = self.state.write().await;
        state.responses.remove(&addr);

        if let Some(slots) = state.slots.remove(&addr) {
            for slot in slots {
                if let Some(ip) = state.virt_ips.remove(&(slot, addr)) {
                    state.virt_nodes.remove(&ip);
                }
            }
        }

        let removed = state.sessions.remove(&addr);
        drop(state);

        if let Some(session) = removed {
            let mut session_state = session.state.write().await;
            *session_state = None;
        }
    }

    pub(crate) async fn try_direct_session(
        &self,
        packet: &proto::response::Node,
    ) -> anyhow::Result<SocketAddr> {
        let node_id = NodeId::from(&packet.node_id[..]);
        if packet.endpoints.is_empty() {
            bail!(
                "Node {} has no public endpoints. Not establishing p2p session",
                node_id
            )
        }

        for endpoint in packet.endpoints.iter() {
            let ip = match IpAddr::from_str(&endpoint.address) {
                Ok(ip) => ip,
                Err(_) => continue,
            };

            let addr = SocketAddr::new(ip, endpoint.port as u16);
            self.init_p2p_session(addr)
                .await
                .map_err(|e| {
                    log::debug!(
                        "Failed to establish p2p session with node {}, address: {}. Error: {}",
                        node_id,
                        addr,
                        e
                    )
                })
                .ok();
        }

        bail!(
            "All attempts to establish direct session with node: {} failed",
            node_id
        )
    }

    pub(crate) async fn register_endpoints(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        let id = self.id();
        log::info!("[{}] registering endpoints", id);

        let response = self
            .request::<proto::response::Register>(
                proto::request::Register { endpoints }.into(),
                session_id.to_vec(),
                REGISTER_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        log::info!("[{}] registration finished", id);

        Ok(response.endpoints)
    }

    pub async fn server_session(&self) -> anyhow::Result<Session> {
        let addr = { self.state.read().await.config.srv_addr };
        Ok(self.session(addr).await?)
    }

    pub async fn session(&self, addr: SocketAddr) -> anyhow::Result<Session> {
        let session = {
            let mut state = self.state.write().await;
            if let Some(session) = state.sessions.get(&addr) {
                return Ok(session.clone());
            }
            state
                .sessions
                .entry(addr)
                .or_insert_with(|| Session::new(addr, self.clone()))
                .clone()
        };
        session.init().await?;
        Ok(session)
    }
}
