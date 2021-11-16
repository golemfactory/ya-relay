use anyhow::{anyhow, bail};
use chrono::Utc;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

use crate::client::{ForwardId, ForwardSender};
use crate::Client;

use ya_relay_core::error::{
    BadRequest, Error, InternalError, NotFound, ServerResult, Unauthorized,
};

use ya_client_model::NodeId;
use ya_relay_core::challenge::{
    self, prepare_challenge_request, prepare_challenge_response, CHALLENGE_DIFFICULTY,
};
use ya_relay_core::session::SessionId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::{Endpoint, Protocol, RequestId, SlotId, StatusCode};

const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

#[derive(Clone)]
pub struct Session {
    pub remote_addr: SocketAddr,
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
        self.client
            .init_server_session(self.remote_addr)
            .await?
            .id()
            .await
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
    async fn init_session(&self, addr: SocketAddr, challenge: bool) -> anyhow::Result<Session> {
        let id = self.id();
        let node_id = self.node_id().await;

        log::info!("[{}] initializing session with {}", id, addr);

        let (request, raw_challenge) = prepare_challenge_request();
        let request = match challenge {
            true => request,
            false => proto::request::Session::default(),
        };

        let request = proto::request::Session {
            node_id: node_id.into_array().to_vec(),
            ..request
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

        let session = self.add_session(addr, session_id).await;

        log::info!("[{}] session initialized with address: {}", id, addr);
        Ok(session)
    }

    async fn init_p2p_session(&self, addr: SocketAddr, node_id: NodeId) -> anyhow::Result<Session> {
        let session = self.init_session(addr, true).await?;
        {
            Ok(self
                .state
                .write()
                .await
                .p2p_sessions
                .entry(node_id)
                .or_insert(session)
                .clone())
        }
    }

    async fn init_server_session(&self, addr: SocketAddr) -> anyhow::Result<Session> {
        self.init_session(addr, false).await
    }

    pub(crate) async fn add_session(&self, addr: SocketAddr, session_id: SessionId) -> Session {
        let mut state = self.state.write().await;
        let session = state
            .sessions
            .entry(addr)
            .or_insert_with(|| Session::new(addr, self.clone()));

        let mut state = session.state.write().await;
        state.replace(SessionState { id: session_id });

        session.clone()
    }

    async fn remove_session(&self, addr: SocketAddr) {
        let mut state = self.state.write().await;
        state.responses.remove(&addr);

        if let Some(slots) = state.slots.remove(&addr) {
            for slot in slots {
                if let Some(ip) = state.virt_ips.remove(&(slot, addr)) {
                    let node = state.virt_nodes.remove(&ip);
                    if let Some(node) = node {
                        state.p2p_sessions.remove(&node.id);
                    }
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

    pub async fn optimal_session(&self, node_id: NodeId) -> anyhow::Result<Session> {
        if let Some(session) = self.state.read().await.p2p_sessions.get(&node_id).cloned() {
            return Ok(session);
        }

        // Find node on server. p2p session will be established, if possible.
        let server = self.server_session().await?;
        server.find_node(node_id).await?;

        // If p2p session was established, we can find it, otherwise
        // communication will be forwarder through relay server.
        Ok(self
            .state
            .read()
            .await
            .p2p_sessions
            .get(&node_id)
            .cloned()
            .unwrap_or(server))
    }

    pub(crate) async fn try_direct_session(
        &self,
        packet: &proto::response::Node,
    ) -> anyhow::Result<Session> {
        let node_id = NodeId::from(&packet.node_id[..]);
        if packet.endpoints.is_empty() {
            bail!(
                "Node {} has no public endpoints. Not establishing p2p session",
                node_id
            )
        }

        for endpoint in packet.endpoints.iter() {
            let addr = match endpoint.clone().try_into() {
                Ok(addr) => addr,
                Err(_) => continue,
            };

            match self.init_p2p_session(addr, node_id).await {
                Ok(session) => return Ok(session),
                Err(e) => {
                    log::debug!(
                        "Failed to establish p2p session with node {}, address: {}. Error: {}",
                        node_id,
                        addr,
                        e
                    )
                }
            }
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

    pub async fn dispatch_session<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) {
        if let Some(session) = { self.state.read().await.starting_sessions.clone() } {
            session
                .dispatch_session(session_id, request_id, from, request)
                .await;
        };
    }

    pub async fn dispatch_get_node<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Node,
    ) {
        let config = { self.state.read().await.config.clone() };
        // TODO:
        // if params.node_id.len() != 20 {
        //     return Err(BadRequest::InvalidNodeId.into());
        // }

        let our_node_id = config.node_id;
        let request_node_id = NodeId::from(&request.node_id[..]);

        // At this point we respond only to requests about our node.
        if our_node_id != request_node_id {
            self.error_response(
                request_id,
                session_id.to_vec(),
                &from,
                BadRequest::InvalidNodeId.into(),
            )
            .await;
            return;
        }

        let response = self
            .our_node_info(request_id, session_id, request.public_key)
            .await;

        self.send(response, from)
            .await
            .map_err(|_| log::warn!("Failed to send response to GetNode from: {}.", from))
            .ok();
    }

    pub async fn dispatch_get_slot<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Slot,
    ) {
        // if request.slot != 0 {
        //     self.error_response(
        //         request_id,
        //         session_id.to_vec(),
        //         &from,
        //         NotFound::NodeBySlot(request.slot).into(),
        //     )
        //     .await;
        //     return;
        // }

        let response = self
            .our_node_info(request_id, session_id, request.public_key)
            .await;

        self.send(response, from)
            .await
            .map_err(|_| log::warn!("Failed to send response to GetSlot from: {}.", from))
            .ok();
    }

    pub async fn our_node_info(
        &self,
        request_id: RequestId,
        session_id: Vec<u8>,
        public_key: bool,
    ) -> proto::Packet {
        let config = { self.state.read().await.config.clone() };
        let public_ip = self.public_addr().await;

        let packet = proto::response::Node {
            node_id: config.node_id.into_array().to_vec(),
            public_key: match public_key {
                true => config.node_pub_key.bytes().to_vec(),
                false => vec![],
            },
            endpoints: public_ip.map_or(vec![], |ip| {
                vec![Endpoint {
                    protocol: Protocol::Udp as i32,
                    address: ip.ip().to_string(),
                    port: ip.port() as u32,
                }]
            }),
            seen_ts: Utc::now().timestamp() as u32,
            slot: 0,
        };

        proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        )
    }

    async fn error_response(&self, req_id: u64, id: Vec<u8>, addr: &SocketAddr, error: Error) {
        let status_code = match error {
            Error::Undefined(_) => StatusCode::Undefined,
            Error::BadRequest(_) => StatusCode::BadRequest,
            Error::Unauthorized(_) => StatusCode::Unauthorized,
            Error::NotFound(_) => StatusCode::NotFound,
            Error::Timeout(_) => StatusCode::Timeout,
            Error::Conflict(_) => StatusCode::Conflict,
            Error::PayloadTooLarge(_) => StatusCode::PayloadTooLarge,
            Error::TooManyRequests(_) => StatusCode::TooManyRequests,
            Error::Internal(_) => StatusCode::ServerError,
            Error::GatewayTimeout(_) => StatusCode::GatewayTimeout,
        };

        self.send(proto::Packet::error(req_id, id, status_code), *addr)
            .await
            .map_err(|e| log::error!("Failed to send error response. {}.", e))
            .ok();
    }
}

type Sessions = Arc<Mutex<HashMap<SessionId, mpsc::Sender<(RequestId, proto::request::Session)>>>>;

#[derive(Clone)]
pub(crate) struct StartingSessions {
    sessions: Sessions,
    client: Client,
}

impl StartingSessions {
    pub fn new(client: Client) -> StartingSessions {
        StartingSessions {
            sessions: Arc::new(Mutex::new(Default::default())),
            client,
        }
    }

    pub async fn dispatch_session(
        self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) {
        // This will be new session.
        match session_id.is_empty() {
            true => self.clone().new_session(request_id, from, request).await,
            false => self
                .existing_session(session_id, request_id, from, request)
                .await
                .map_err(|e| e.into()),
        }
        .map_err(|e| log::warn!("{}", e))
        .ok();
    }

    pub async fn new_session(
        self,
        request_id: RequestId,
        with: SocketAddr,
        request: proto::request::Session,
    ) -> anyhow::Result<()> {
        if request.node_id.len() != 20 {
            return Err(BadRequest::InvalidNodeId.into());
        }

        let session_id = SessionId::generate();
        let node_id = NodeId::from(&request.node_id[..]);
        let (sender, receiver) = mpsc::channel(1);

        log::info!(
            "Node {} ({}) tries to establish p2p session.",
            node_id,
            with
        );

        {
            self.sessions
                .lock()
                .unwrap()
                .entry(session_id)
                .or_insert(sender);
        }

        // TODO: Add timeout for session initialization.
        tokio::task::spawn_local(async move {
            if let Err(e) = self
                .clone()
                .init_session_handler(with, request_id, session_id, request, receiver)
                .await
            {
                log::warn!(
                    "Error initializing session {} with node {}. Error: {}",
                    session_id,
                    node_id,
                    e
                );

                // Establishing session failed.
                self.client
                    .error_response(request_id, session_id.to_vec(), &with, e)
                    .await;
                self.cleanup_initialization(&session_id).await;
            }
        });
        Ok(())
    }

    async fn existing_session(
        &self,
        id: Vec<u8>,
        request_id: RequestId,
        _from: SocketAddr,
        request: proto::request::Session,
    ) -> ServerResult<()> {
        let id = SessionId::try_from(id.clone()).map_err(|_| Unauthorized::InvalidSessionId(id))?;
        let mut sender = {
            match { self.sessions.lock().unwrap().get(&id).cloned() } {
                Some(sender) => sender.clone(),
                None => return Err(Unauthorized::SessionNotFound(id).into()),
            }
        };

        Ok(sender
            .send((request_id, request))
            .await
            .map_err(|_| InternalError::Send)?)
    }

    async fn init_session_handler(
        self,
        with: SocketAddr,
        request_id: RequestId,
        session_id: SessionId,
        request: proto::request::Session,
        mut rc: mpsc::Receiver<(RequestId, proto::request::Session)>,
    ) -> ServerResult<()> {
        if request.node_id.len() != 20 {
            return Err(BadRequest::InvalidNodeId.into());
        }

        let node_id = NodeId::from(&request.node_id[..]);

        let (packet, raw_challenge) = prepare_challenge_response();
        let challenge = proto::Packet::response(
            request_id,
            session_id.to_vec(),
            proto::StatusCode::Ok,
            packet,
        );

        self.client
            .send(challenge, with)
            .await
            .map_err(|_| InternalError::Send)?;

        log::info!("Challenge sent to Node: {}, address: {}", node_id, with);

        if let Some((request_id, session)) = rc.next().await {
            log::info!("Got challenge from node: {}, address: {}", node_id, with);

            // Validate the challenge
            if !challenge::verify(
                &raw_challenge,
                CHALLENGE_DIFFICULTY,
                &session.challenge_resp,
                session.public_key.as_slice(),
            )
            .map_err(|e| BadRequest::InvalidChallenge(e.to_string()))?
            {
                return Err(Unauthorized::InvalidChallenge.into());
            }

            log::info!(
                "Challenge from Node: {}, address: {} verified.",
                node_id,
                with
            );

            let crypto = self
                .client
                .crypto()
                .await
                .map_err(|e| InternalError::Generic(e.to_string()))?;
            let public_key = crypto
                .public_key()
                .await
                .map_err(|e| InternalError::Generic(e.to_string()))?;

            let challenge_resp = match request.challenge_req {
                Some(request) => {
                    challenge::solve(request.challenge.as_slice(), request.difficulty, crypto)
                        .await
                        .map_err(|e| InternalError::Generic(e.to_string()))?
                }
                None => vec![],
            };

            let packet = proto::response::Session {
                challenge_resp,
                challenge_req: None,
                public_key: public_key.bytes().to_vec(),
            };

            self.client
                .send(
                    proto::Packet::response(
                        request_id,
                        session_id.to_vec(),
                        proto::StatusCode::Ok,
                        packet,
                    ),
                    with,
                )
                .await
                .map_err(|_| InternalError::Send)?;

            self.client.add_session(with, session_id).await;
        }

        Ok(())
    }

    async fn cleanup_initialization(&self, session_id: &SessionId) {
        self.sessions.lock().unwrap().remove(session_id);
    }
}
