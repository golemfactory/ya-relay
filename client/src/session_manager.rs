use anyhow::{anyhow, bail};
use futures::channel::mpsc;
use futures::future::{AbortHandle, LocalBoxFuture};
use futures::{FutureExt, SinkExt, TryFutureExt};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::RwLock;

use ya_relay_core::challenge::{self, ChallengeDigest, RawChallenge, CHALLENGE_DIFFICULTY};
use ya_relay_core::crypto::{Crypto, CryptoProvider, PublicKey};
use ya_relay_core::identity::Identity;
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_core::NodeId;
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::{Forward, RequestId, SlotId, FORWARD_SLOT_ID};
use ya_relay_stack::{Channel, Connection};

use crate::client::{ClientConfig, ForwardSender, Forwarded};
use crate::dispatch::{dispatch, Dispatcher, Handler};
use crate::expire::track_sessions_expiration;
use crate::registry::{NodeEntry, NodesRegistry};
use crate::session::{Session, SessionError, SessionResult};
use crate::session_guard::GuardedSessions;
use crate::session_start::StartingSessions;
use crate::virtual_layer::TcpLayer;

/// This is the only layer, that should know real IP addresses and ports
/// of other peers. Other layers should use higher level abstractions
/// like `NodeId` or `SlotId`.
#[derive(Clone)]
pub struct SessionManager {
    sink: Option<OutStream>,
    pub config: Arc<ClientConfig>,

    pub virtual_tcp: TcpLayer,
    pub registry: NodesRegistry,

    virtual_tcp_fast_lane: Rc<RefCell<HashSet<(SocketAddr, SlotId)>>>,
    guarded: GuardedSessions,
    state: Arc<RwLock<SessionManagerState>>,
}

pub struct SessionManagerState {
    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    public_addr: Option<SocketAddr>,
    /// Equals to `None` when not listening
    bind_addr: Option<SocketAddr>,

    pub(crate) sessions: HashMap<SocketAddr, Arc<Session>>,
    pub(crate) nodes_addr: HashMap<SocketAddr, NodeId>,
    pub(crate) nodes_identity: HashMap<NodeId, (NodeId, PublicKey)>,

    forward_reliable: HashMap<NodeId, ForwardSender>,
    forward_unreliable: HashMap<NodeId, ForwardSender>,

    p2p_sessions: HashMap<NodeId, Arc<Session>>,
    ingress_channel: Channel<Forwarded>,

    starting_sessions: Option<StartingSessions>,

    // Collection of background tasks that must be stopped on shutdown.
    handles: Vec<AbortHandle>,
}

impl SessionManager {
    pub fn new(config: Arc<ClientConfig>) -> SessionManager {
        let ingress = Channel::<Forwarded>::default();
        SessionManager {
            sink: None,
            config: config.clone(),
            virtual_tcp: TcpLayer::new(config, ingress.clone()),
            virtual_tcp_fast_lane: Default::default(),
            registry: NodesRegistry::default(),
            guarded: Default::default(),
            state: Arc::new(RwLock::new(SessionManagerState {
                public_addr: None,
                bind_addr: None,
                sessions: Default::default(),
                nodes_addr: Default::default(),
                nodes_identity: Default::default(),
                forward_reliable: Default::default(),
                forward_unreliable: Default::default(),
                p2p_sessions: Default::default(),
                ingress_channel: ingress,
                starting_sessions: None,
                handles: vec![],
            })),
        }
    }

    pub async fn spawn(&mut self) -> anyhow::Result<SocketAddr> {
        let (stream, sink, bind_addr) = udp_bind(&self.config.bind_url).await?;

        self.sink = Some(sink.clone());

        self.virtual_tcp.spawn(self.config.node_id).await?;

        let abort_dispatcher = spawn_local_abortable(dispatch(self.clone(), stream));
        let abort_expiration = spawn_local_abortable(track_sessions_expiration(self.clone()));

        {
            let mut state = self.state.write().await;

            state.bind_addr.replace(bind_addr);
            state.handles.push(abort_dispatcher);
            state.handles.push(abort_expiration);
            state.starting_sessions = Some(StartingSessions::new(
                self.clone(),
                self.guarded.clone(),
                sink,
            ));
        }

        Ok(bind_addr)
    }

    pub async fn get_public_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.public_addr
    }

    pub async fn set_public_addr(&self, addr: Option<SocketAddr>) {
        self.state.write().await.public_addr = addr;
    }

    pub async fn remote_id(&self, addr: &SocketAddr) -> Option<NodeId> {
        let state = self.state.read().await;
        state.nodes_addr.get(addr).copied()
    }

    pub async fn alias(&self, node_id: &NodeId) -> Option<NodeId> {
        let state = self.state.read().await;
        state.nodes_identity.get(node_id).map(|(id, _)| *id)
    }

    pub async fn is_p2p(&self, node_id: &NodeId) -> bool {
        self.state.read().await.p2p_sessions.contains_key(node_id)
    }

    pub async fn list_identities(&self) -> Vec<NodeId> {
        // This list stores only aliases. We need to add default ids.
        let unique_nodes = self.registry.list_nodes().await;

        let state = self.state.read().await;
        let mut ids = state.nodes_identity.keys().cloned().collect::<Vec<_>>();

        ids.extend(unique_nodes.into_iter());
        ids
    }

    async fn init_session(
        &self,
        addr: SocketAddr,
        challenge: bool,
        node_id: Option<NodeId>,
    ) -> SessionResult<Arc<Session>> {
        let this_id = self.config.node_id;

        let tmp_session = self.temporary_session(addr).await?;

        log::debug!("[{}] initializing session with {}", this_id, addr);

        let (request, raw_challenge) = self.prepare_challenge_request(challenge).await?;
        let response = tmp_session
            .request::<proto::response::Session>(
                request.into(),
                vec![],
                self.config.session_request_timeout,
            )
            .await?;

        // Default NodeId in case of relay server.
        // TODO: consider giving relay server proper NodeId.
        self.guarded
            .notify_first_message(node_id.unwrap_or_default())
            .await;

        let session_id = SessionId::try_from(response.session_id.clone())?;
        let challenge_req = response.packet.challenge_req.ok_or_else(|| {
            anyhow!(
                "Expected ChallengeRequest while initializing session with {}",
                addr
            )
        })?;
        let challenge_handle = self.solve_challenge(challenge_req).await;

        log::trace!(
            "Solving challenge while establishing session with: {}",
            addr
        );

        // with the current ECDSA scheme the public key
        // can be recovered from challenge signature
        let packet = proto::request::Session {
            challenge_resp: Some(challenge_handle.await?),
            ..Default::default()
        };
        let response = tmp_session
            .request::<proto::response::Session>(
                packet.into(),
                session_id.to_vec(),
                self.config.challenge_request_timeout,
            )
            .await?;

        log::trace!("Challenge sent to: {}", addr);

        if session_id != &response.session_id[..] {
            let _ = tmp_session.disconnect().await;
            return Err(SessionError::Drop(format!(
                "[{}] init session id mismatch: {} vs {:?} (response)",
                this_id, session_id, response.session_id,
            )));
        }

        log::trace!("Validating challenge from: {}", addr);

        let (remote_id, identities) = match {
            if challenge {
                // Validate the challenge
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    CHALLENGE_DIFFICULTY,
                    response.packet.challenge_resp,
                    node_id,
                )
            } else {
                Ok(Default::default())
            }
        } {
            Ok(tuple) => tuple,
            Err(err) => {
                let _ = tmp_session.disconnect().await;
                return Err(SessionError::Drop(format!(
                    "{err} while initializing session with {addr}"
                )));
            }
        };

        if let Some(expected_id) = node_id {
            // Validate remote NodeId
            if remote_id != expected_id && !identities.iter().any(|i| i.node_id == expected_id) {
                let _ = tmp_session.disconnect().await;
                return Err(SessionError::Drop(format!(
                    "remote node id mismatch: {expected_id} vs {remote_id} (response)",
                )));
            }
        }

        let session = self
            .add_session(addr, session_id, remote_id, identities)
            .await?;

        session
            .send(proto::Packet::control(
                session.id.to_vec(),
                ya_relay_proto::proto::control::ResumeForwarding::default(),
            ))
            .await?;

        log::trace!(
            "[{}] session {} established with address: {}",
            this_id,
            session_id,
            addr
        );

        // Send ping to measure response time.
        let session_cp = session.clone();
        tokio::task::spawn_local(async move {
            session_cp.ping().await.ok();
        });

        Ok(session)
    }

    async fn init_p2p_session(
        &self,
        addr: SocketAddr,
        node_id: NodeId,
    ) -> SessionResult<Arc<Session>> {
        log::info!(
            "Initializing p2p session with Node: [{}], address: {}",
            node_id,
            addr
        );

        let session = self.init_session(addr, true, Some(node_id)).await?;
        {
            let mut state = self.state.write().await;
            state.nodes_addr.insert(addr, node_id);
            state.p2p_sessions.insert(node_id, session.clone());
        };

        log::info!(
            "Established P2P session {} with node [{}] ({})",
            session.id,
            node_id,
            addr
        );

        Ok(session)
    }

    async fn init_server_session(&self, addr: SocketAddr) -> SessionResult<Arc<Session>> {
        log::info!("Initializing session with NET relay server at: {}", addr);

        let session = self
            .init_session(addr, false, None)
            .await
            .map_err(|e| anyhow!("Failed to init relay server session: {e}"))?;

        {
            let mut state = self.state.write().await;
            state.sessions.insert(addr, session.clone());
        }

        log::info!(
            "Established session {} with NET relay server ({addr})",
            session.id,
        );

        Ok(session)
    }

    async fn temporary_session(&self, addr: SocketAddr) -> anyhow::Result<Arc<Session>> {
        Ok(self
            .guarded
            .temporary_session(addr, self.out_stream()?)
            .await)
    }

    pub async fn sessions(&self) -> Vec<Arc<Session>> {
        let state = self.state.read().await;
        state
            .sessions
            .iter()
            .map(|(_, session)| session.clone())
            .collect()
    }

    pub async fn find_session(&self, addr: SocketAddr) -> Option<Arc<Session>> {
        let state = self.state.read().await;
        state.sessions.get(&addr).cloned()
    }

    pub(crate) async fn add_session(
        &self,
        addr: SocketAddr,
        id: SessionId,
        node_id: NodeId,
        identities: impl IntoIterator<Item = Identity>,
    ) -> anyhow::Result<Arc<Session>> {
        log::trace!("add_session {} {} {}", id, addr, node_id);

        let session = Session::new(addr, id, self.out_stream()?);
        let mut state = self.state.write().await;

        state.add_session(addr, session.clone());
        state.add_identities(node_id, identities);

        Ok(session)
    }

    /// Function adds session initialized by other peer.
    /// Note: We can't use `self.add_session` internally due to synchronization.
    pub async fn add_incoming_session(
        &self,
        addr: SocketAddr,
        id: SessionId,
        node_id: NodeId,
        identities: impl IntoIterator<Item = Identity>,
    ) -> anyhow::Result<Arc<Session>> {
        let session = Session::new(addr, id, self.out_stream()?);

        // Incoming sessions are always p2p sessions, so we use slot FORWARD_SLOT_ID.
        self.registry
            .add_node(node_id, session.clone(), FORWARD_SLOT_ID)
            .await;
        {
            let mut state = self.state.write().await;

            state.add_session(addr, session.clone());
            state.add_identities(node_id, identities);

            state.nodes_addr.insert(addr, node_id);
            state.p2p_sessions.insert(node_id, session.clone());

            log::trace!(
                "[{}] Saved node session {} [{}] {}",
                self.config.node_id,
                session.id,
                node_id,
                addr
            )
        }
        Ok(session)
    }

    pub async fn remove_node(&self, node_id: NodeId) {
        log::debug!(
            "Removing Node [{}] information. Stopping communication..",
            node_id
        );

        self.virtual_tcp_fast_lane.borrow_mut().clear();
        self.registry.remove_node(node_id).await.ok();
        self.virtual_tcp.remove_node(node_id).await.ok();

        {
            let mut state = self.state.write().await;
            let tx_r = state.forward_reliable.remove(&node_id);
            let tx_u = state.forward_unreliable.remove(&node_id);

            if let Some(session) = state.p2p_sessions.remove(&node_id) {
                state.nodes_addr.remove(&session.remote);
            }

            drop(state);
            if let Some(mut tx) = tx_r {
                tx.close().await.ok();
            }
            if let Some(mut tx) = tx_u {
                tx.close().await.ok();
            }
        }
    }

    pub async fn close_session(&self, session: Arc<Session>) -> anyhow::Result<()> {
        log::info!("Closing session {} ({})", session.id, session.remote);

        if let Some(node_id) = {
            let mut state = self.state.write().await;
            state.sessions.remove(&session.remote);
            state.nodes_addr.remove(&session.remote)
        } {
            // We are closing p2p session with Node.
            // TODO: If we will support forwarding through other Nodes, than
            //       relay server, we must handle this here.
            self.remove_node(node_id).await;
        } else {
            // We are removing relay server session and all Nodes that are using
            // it to forward messages.
            futures::future::join_all(
                self.registry
                    .nodes_using_session(session.clone())
                    .await
                    .into_iter()
                    .map(|id| self.remove_node(id)),
            )
            .await;
        }

        session.close().await?;
        Ok(())
    }

    pub async fn optimal_session(&self, node_id: NodeId) -> anyhow::Result<Arc<Session>> {
        // Maybe we already have optimal session resolved.
        if let Ok(node) = self.registry.resolve_node(node_id).await {
            return Ok(node.session);
        }

        // get node info
        let server_session = self
            .server_session()
            .await
            .map_err(|e| anyhow!("Failed to get relay server session: {e}"))?;
        let node = server_session
            .find_node(node_id)
            .await
            .map_err(|e| anyhow!("Failed to find Node on relay server: {e}"))?;
        let ident = Identity::try_from(&node)
            .map_err(|e| anyhow!("Unable to get identity from Node info: {e}"))?;

        // Find node on server. p2p session will be established, if possible. Otherwise
        // communication will be forwarder through relay server.
        Ok(self
            .resolve(ident.node_id.as_ref(), &node.endpoints, node.slot)
            .await
            .map_err(|e| anyhow!("Failed to resolve session: {e}"))?
            .session)
    }

    pub fn out_stream(&self) -> anyhow::Result<OutStream> {
        self.sink
            .clone()
            .ok_or_else(|| anyhow!("Network sink not initialized"))
    }

    pub async fn forward(
        &self,
        _session: Arc<Session>,
        node_id: NodeId,
    ) -> anyhow::Result<ForwardSender> {
        // TODO: Use `_session` parameter. We can allow using other session, than default.

        let node = self.registry.resolve_node(node_id).await?;
        let tx = {
            match {
                let state = self.state.read().await;
                state.forward_reliable.get(&node_id).cloned()
            } {
                Some(tx) => tx,
                None => {
                    self.virtual_tcp_fast_lane.borrow_mut().clear();

                    let conn_lock = node.conn_lock.clone();
                    let (_guard, was_locked) = match conn_lock.try_write() {
                        Ok(guard) => (guard, false),
                        Err(_) => (conn_lock.write().await, true),
                    };

                    if was_locked {
                        if let Some(tx) = {
                            let state = self.state.read().await;
                            state.forward_reliable.get(&node_id).cloned()
                        } {
                            return Ok(tx);
                        }
                    }

                    let conn = self.virtual_tcp.connect(node.clone()).await?;
                    let (tx, rx) = mpsc::channel(1);
                    tokio::task::spawn_local(self.clone().forward_reliable_handler(conn, node, rx));

                    let mut state = self.state.write().await;
                    state.forward_reliable.insert(node_id, tx.clone());

                    tx
                }
            }
        };

        Ok(tx)
    }

    pub async fn forward_unreliable(
        &self,
        session: Arc<Session>,
        node_id: NodeId,
    ) -> anyhow::Result<ForwardSender> {
        let node = self.registry.resolve_node(node_id).await?;

        let (tx, rx) = {
            let mut state = self.state.write().await;
            match state.forward_unreliable.get(&node_id) {
                Some(tx) => return Ok(tx.clone()),
                None => {
                    let (tx, rx) = mpsc::channel(1);
                    state.forward_unreliable.insert(node_id, tx.clone());
                    (tx, rx)
                }
            }
        };

        tokio::task::spawn_local(self.clone().forward_unreliable_handler(session, node, rx));
        Ok(tx)
    }

    async fn forward_reliable_handler(
        self,
        connection: Connection,
        node: NodeEntry,
        mut rx: mpsc::Receiver<Vec<u8>>,
    ) {
        let pause = node.session.forward_pause.clone();
        let session = node.session.clone();

        while let Some(payload) = self.virtual_tcp.get_next_fwd_payload(&mut rx, &pause).await {
            log::trace!(
                "Forwarding message to {} through {} (session id: {})",
                node.id,
                session.remote,
                session.id
            );

            if let Err(err) = self.virtual_tcp.send(payload, connection).await {
                log::debug!(
                    "[{}] forward to {} through {} (session id: {}) failed: {}",
                    self.config.node_id,
                    node.id,
                    session.remote,
                    session.id,
                    err
                );
                break;
            }
        }

        log::debug!(
            "[{}] forward: disconnected from: {}",
            node.id,
            session.remote
        );

        rx.close();
        let _ = self.close_session(node.session.clone()).await;
    }

    async fn forward_unreliable_handler(
        self,
        session: Arc<Session>,
        node: NodeEntry,
        mut rx: mpsc::Receiver<Vec<u8>>,
    ) {
        let pause = node.session.forward_pause.clone();

        while let Some(payload) = self.virtual_tcp.get_next_fwd_payload(&mut rx, &pause).await {
            log::trace!(
                "Forwarding message (U) to {} through {} (session id: {})",
                node.id,
                session.remote,
                session.id
            );

            let forward = Forward::unreliable(session.id, node.slot, payload);
            if let Err(error) = session.send(forward).await {
                log::debug!(
                    "[{}] forward (U) to {} through {} (session id: {}) failed: {}",
                    self.config.node_id,
                    node.id,
                    session.remote,
                    session.id,
                    error
                );
                break;
            }
        }

        log::debug!(
            "[{}] forward (U): disconnected from: {}",
            node.id,
            session.remote
        );

        rx.close();
        self.remove_node(node.id).await;
    }

    async fn filter_own_addresses(&self, endpoints: &[proto::Endpoint]) -> Vec<SocketAddr> {
        let own_addrs: Vec<_> = {
            let state = self.state.read().await;
            vec![state.bind_addr, state.public_addr]
                .into_iter()
                .flatten()
                .collect()
        };
        endpoints
            .iter()
            .cloned()
            .filter_map(|e| e.try_into().ok())
            .filter(|a| !own_addrs.iter().any(|o| o == a))
            .collect()
    }

    async fn resolve(
        &self,
        node_id: &[u8],
        endpoints: &[proto::Endpoint],
        slot: u32,
    ) -> anyhow::Result<NodeEntry> {
        let node_id: NodeId = node_id.try_into()?;
        if node_id == self.config.node_id {
            bail!("Remote id belongs to this node.");
        }

        log::debug!("Resolving [{node_id}], slot = {slot}");

        let addrs: Vec<SocketAddr> = self.filter_own_addresses(endpoints).await;

        let lock = self.guarded.guard_initialization(node_id, &addrs).await;
        let _guard = lock.write().await;

        // Check whether we are already connected to a node with this id
        if let Ok(entry) = self.registry.resolve_node(node_id).await {
            let session_type = match entry.is_p2p() {
                true => "p2p",
                false => "relay",
            };
            log::debug!("Resolving Node [{node_id}]. Returning already existing connection (session = {} ({session_type}), slot = {}).", entry.session.id, entry.slot);
            return Ok(entry);
        }

        let this = self.clone();
        Ok(self
            .try_resolve(node_id, addrs.clone(), slot)
            .then(move |result| async move {
                this.guarded
                    .stop_guarding(node_id, result.clone().map(|entry| entry.session))
                    .await;
                result
            })
            .await?)
    }

    async fn try_resolve(
        &self,
        node_id: NodeId,
        addrs: Vec<SocketAddr>,
        slot: u32,
    ) -> SessionResult<NodeEntry> {
        // If requested slot == FORWARD_SLOT_ID, than we are responding to ReverseConnection,
        // so we shouldn't try to send it ourselves.
        let try_reverse = slot != FORWARD_SLOT_ID;

        // If node has public IP, we can establish direct session with him
        // instead of forwarding messages through relay.
        let (session, resolved_slot) = match self
            .try_direct_session(node_id, addrs, try_reverse)
            .await
        {
            // If we send packets directly, we use FORWARD_SLOT_ID.
            Ok(session) => (session, FORWARD_SLOT_ID),
            // Fatal error, cease further communication
            Err(SessionError::Drop(e)) => {
                log::warn!("{}", e);
                return Err(SessionError::Drop(e));
            }
            // Recoverable error, continue
            Err(SessionError::Retry(e)) => {
                log::info!("{}", e);

                // In this case we don't have slot id, so we can't establish relayed connection.
                // It happens mostly if we are trying to establish connection in response to ReverseConnection.
                if !try_reverse {
                    return Err(anyhow!("Failed to establish p2p connection with [{node_id}] and relay Server won't be used, since slot = {FORWARD_SLOT_ID}.").into());
                }

                (self.server_session().await?, slot)
            }
        };

        if resolved_slot != FORWARD_SLOT_ID {
            log::info!(
                "Using relay Server to forward packets to [{node_id}] (slot {resolved_slot})"
            );
        }

        log::debug!(
            "Adding node [{node_id}] session ({} - {}), resolved slot = {resolved_slot}, requested slot = {slot}",
            session.id,
            session.remote
        );

        let node = self
            .registry
            .add_node(node_id, session, resolved_slot)
            .await;
        self.registry.add_slot_for_node(node_id, slot).await;

        Ok(node)
    }

    pub async fn try_direct_session(
        &self,
        node_id: NodeId,
        addrs: Vec<SocketAddr>,
        try_reverse_connection: bool,
    ) -> SessionResult<Arc<Session>> {
        if addrs.is_empty() {
            log::debug!("Node [{node_id}] has no public endpoints.");
        }

        // Try to connect to remote Node's public endpoints.
        for addr in addrs {
            match self.init_p2p_session(addr, node_id).await {
                Err(SessionError::Retry(err)) => {
                    log::debug!(
                        "Failed to establish p2p session with node [{node_id}], address: {addr}. Error: {err}"
                    )
                }
                result => return result,
            }
        }

        // We are trying to connect to Node without public IP. Trying to send
        // ReverseConnection message, so Node will try to connect to us.
        if try_reverse_connection && self.get_public_addr().await.is_some() {
            match self.try_reverse_connection(node_id).await {
                Err(e) => log::info!("Reverse connection with [{node_id}] failed: {e}"),
                Ok(session) => return Ok(session),
            }
        }

        Err(anyhow!("All attempts to establish direct session with node [{node_id}] failed").into())
    }

    async fn try_reverse_connection(&self, node_id: NodeId) -> anyhow::Result<Arc<Session>> {
        log::info!(
            "Request reverse connection. me={}, remote={node_id}",
            self.config.node_id,
        );

        let mut awaiting = self.guarded.register_waiting_for_node(node_id).await?;

        let server_session = self.server_session().await?;
        server_session.reverse_connection(node_id).await?;

        log::debug!("ReverseConnection requested with node [{node_id}]");

        // We don't want to wait for connection finish, because we need longer timeout for this.
        // But if we won't receive any message from other Node, we would like to exit early,
        // to try out different connection methods.
        tokio::time::timeout(
            self.config.reverse_connection_tmp_timeout,
            awaiting.wait_for_first_message(),
        )
        .await?;

        // If we have first handshake message from other node, we can wait with
        // longer timeout now, because we can have hope, that this node is responsive.
        let result = tokio::time::timeout(
            self.config.reverse_connection_real_timeout,
            awaiting.wait_for_connection(),
        )
        .await;

        match result {
            Ok(Ok(session)) => {
                log::info!("ReverseConnection - got session with node: [{node_id}]");
                Ok(session)
            }
            Ok(Err(e)) => {
                log::info!("ReverseConnection - failed to establish session with: [{node_id}]");
                Err(e.into())
            }
            Err(_) => {
                log::info!(
                    "ReverseConnection - waiting for session timed out ({}). Node: [{node_id}]",
                    humantime::format_duration(self.config.reverse_connection_real_timeout)
                );
                bail!("Not able to setup ReverseConnection within timeout with node: [{node_id}]")
            }
        }
    }

    pub async fn server_session(&self) -> anyhow::Result<Arc<Session>> {
        // A little bit dirty hack, that we give default NodeId (0x00) for relay server.
        let remote_id = NodeId::default();
        let lock = self
            .guarded
            .guard_initialization(remote_id, &[self.config.srv_addr])
            .await;

        let this = self.clone();
        let session = {
            let _guard = lock.write().await;

            if let Some(session) = self.get_server_session().await {
                return Ok(session);
            }

            self.init_server_session(self.config.srv_addr)
                .then(move |result| async move {
                    this.guarded.stop_guarding(remote_id, result.clone()).await;
                    result
                })
                .await?
        };

        let manager = self.clone();
        session
            .dispatcher
            .handle_error(proto::StatusCode::Unauthorized as i32, true, move || {
                let manager = manager.clone();
                async move {
                    manager.drop_server_session().await;
                    let _ = manager.server_session().await;
                }
                .boxed_local()
            });

        let fast_lane = self.virtual_tcp_fast_lane.clone();
        session.on_drop(move || {
            fast_lane.borrow_mut().clear();
        });

        let endpoints = session.register_endpoints(vec![]).await?;

        // If there is any (correct) endpoint on the list, that means we have public IP.
        if let Some(addr) = endpoints
            .into_iter()
            .find_map(|endpoint| endpoint.try_into().ok())
        {
            self.set_public_addr(Some(addr)).await;
        }

        Ok(session)
    }

    pub(crate) async fn drop_server_session(&self) -> bool {
        if let Some(session) = self.get_server_session().await {
            let _ = self.close_session(session).await;
            return true;
        }
        false
    }

    async fn get_server_session(&self) -> Option<Arc<Session>> {
        let state = self.state.read().await;
        state.sessions.get(&self.config.srv_addr).cloned()
    }

    pub(crate) async fn get_session(&self, addr: SocketAddr) -> Option<Arc<Session>> {
        let state = self.state.read().await;
        state.sessions.get(&addr).cloned()
    }

    pub(crate) async fn assert_not_connected(&self, addrs: &[SocketAddr]) -> anyhow::Result<()> {
        let state = self.state.read().await;

        // Fail if we're already connected to the remote address
        for addr in addrs.iter() {
            if state.nodes_addr.contains_key(addr) {
                anyhow::bail!("already connected to {}", addr);
            }
        }

        Ok(())
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let (starting, abort_handles) = {
            let mut state = self.state.write().await;
            let starting = state.starting_sessions.take();
            let handles = std::mem::take(&mut state.handles);
            (starting, handles)
        };

        for abort_handle in abort_handles {
            abort_handle.abort();
        }

        if let Some(starting) = starting {
            starting.shutdown().await;
        }

        // Close sessions simultaneously, otherwise shutdown could last too long.
        let sessions = self.sessions().await;
        futures::future::join_all(sessions.into_iter().map(|session| {
            self.close_session(session.clone()).map_err(move |err| {
                log::warn!(
                    "Failed to close session {} ({}). {}",
                    session.id,
                    session.remote,
                    err,
                )
            })
        }))
        .await;

        self.virtual_tcp.shutdown().await;
        if let Some(mut out_stream) = self.sink.take() {
            if let Err(e) = out_stream.close().await {
                log::warn!("Error closing socket (output stream). {}", e);
            }
        }

        self.guarded.shutdown().await;
        Ok(())
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

    pub async fn on_ping(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        _request: proto::request::Ping,
    ) {
        let packet = proto::Packet::response(
            request_id,
            session_id,
            proto::StatusCode::Ok,
            proto::response::Pong {},
        );

        if let Err(e) = self.send(packet, from).await {
            log::warn!("Unable to send Pong to {}: {}", from, e);
        }
    }

    pub async fn dispatch_unreliable(&self, node_id: NodeId, forward: Forward) {
        let tx = {
            let state = self.state.read().await;
            state.ingress_channel.tx.clone()
        };

        let payload = Forwarded {
            reliable: false,
            node_id,
            payload: forward.payload.into_vec(),
        };

        if tx.send(payload).is_err() {
            log::trace!(
                "[{}] ingress router: ingress handler closed for node {}",
                self.config.node_id,
                node_id
            );
        }
    }

    async fn send_disconnect(&self, session_id: SessionId, addr: SocketAddr) -> anyhow::Result<()> {
        // Don't use temporary session, because we don't want to initialize session
        // with this address, nor receive the response.
        let session = Session::new(addr, session_id, self.out_stream()?);
        session.disconnect().await
    }

    async fn send(&self, packet: impl Into<PacketKind>, addr: SocketAddr) -> anyhow::Result<()> {
        let mut stream = self.out_stream()?;
        Ok(stream.send((packet.into(), addr)).await?)
    }

    pub async fn solve_challenge<'a>(
        &self,
        request: proto::ChallengeRequest,
    ) -> LocalBoxFuture<'a, anyhow::Result<proto::ChallengeResponse>> {
        let crypto_vec = match self.list_crypto().await {
            Ok(crypto) => crypto,
            Err(e) => return Box::pin(futures::future::err(e)),
        };

        // Compute challenge in different thread to avoid blocking runtime.
        // Note: computing starts here, not after awaiting.
        challenge::solve::<ChallengeDigest, _>(request.challenge, request.difficulty, crypto_vec)
            .boxed_local()
    }

    pub async fn prepare_challenge_request(
        &self,
        challenge: bool,
    ) -> SessionResult<(proto::request::Session, RawChallenge)> {
        let (mut request, raw_challenge) = challenge::prepare_challenge_request();

        let crypto = self
            .list_crypto()
            .await
            .map_err(|e| SessionError::Drop(format!("Failed to query identities: {}", e)))?;

        let mut identities = vec![];
        for id in crypto {
            let pub_key = id.public_key().await?;
            identities.push(proto::Identity {
                node_id: pub_key.address().to_vec(),
                public_key: pub_key.bytes().to_vec(),
            })
        }

        request.identities = identities;

        if !challenge {
            request.challenge_req = None;
        }

        Ok((request, raw_challenge))
    }

    pub async fn list_crypto(&self) -> anyhow::Result<Vec<Rc<dyn Crypto>>> {
        let mut crypto_vec = match self.config.crypto.get(self.config.node_id).await {
            Ok(crypto) => vec![crypto],
            Err(e) => bail!(e),
        };
        let aliases = match self.config.crypto.aliases().await {
            Ok(aliases) => aliases,
            Err(e) => bail!(e),
        };
        for alias in aliases {
            match self.config.crypto.get(alias).await {
                Ok(crypto) => crypto_vec.push(crypto),
                _ => log::debug!("Unable to retrieve Crypto instance for id [{}]", alias),
            }
        }
        Ok(crypto_vec)
    }
}

impl Handler for SessionManager {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        let handler = self.clone();
        async move {
            let session = {
                let state = handler.state.read().await;
                state.sessions.get(&from).cloned()
            };

            // We get either dispatcher for already existing Session from self,
            // or temporary session, that is during creation.
            match session {
                Some(session) => Some(session.dispatcher()),
                None => self
                    .guarded
                    .get_temporary_session(&from)
                    .await
                    .map(|entity| entity.dispatcher()),
            }
        }
        .boxed_local()
    }

    fn on_control(
        self,
        _session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        if let Some(kind) = control.kind {
            let fut = match kind {
                ya_relay_proto::proto::control::Kind::ReverseConnection(message) => {
                    let myself = self;
                    tokio::task::spawn_local(async move {
                        let node_id = NodeId::try_from(&message.node_id)
                            .ok()
                            .map(|id| id.to_string())
                            .unwrap_or(format!("{:?}", message.node_id));

                        log::info!(
                            "Got ReverseConnection message. node={node_id}, endpoints={:?}",
                            message.endpoints
                        );

                        if message.endpoints.is_empty() {
                            log::warn!("Got ReverseConnection with no endpoints to connect to.");
                            return;
                        }

                        match myself
                            .resolve(&message.node_id, &message.endpoints, FORWARD_SLOT_ID)
                            .await
                        {
                            Err(e) => log::warn!(
                                "Failed to resolve reverse connection. node_id={node_id} error={e}"
                            ),
                            Ok(_) => log::trace!("ReverseConnection succeeded: {:?}", message),
                        };
                    });
                    return None;
                }
                ya_relay_proto::proto::control::Kind::PauseForwarding(_) => async move {
                    match self.find_session(from).await {
                        Some(session) => {
                            log::debug!("Forwarding paused for session {}", session.id);
                            session.pause_forwarding().await;
                        }
                        None => {
                            log::warn!("Cannot pause forwarding: session with {} not found", from)
                        }
                    }
                }
                .boxed_local(),
                ya_relay_proto::proto::control::Kind::ResumeForwarding(_) => async move {
                    match self.find_session(from).await {
                        Some(session) => {
                            log::debug!("Forwarding resumed for session {}", session.id);
                            session.resume_forwarding().await;
                        }
                        None => {
                            log::warn!("Cannot resume forwarding: session with {} not found", from)
                        }
                    }
                }
                .boxed_local(),
                ya_relay_proto::proto::control::Kind::Disconnected(
                    proto::control::Disconnected { by: Some(by) },
                ) => {
                    async move {
                        log::debug!("Got Disconnected from {}", from);

                        if let Ok(node) = match by {
                            By::Slot(id) => self.registry.resolve_slot(id).await,
                            By::NodeId(id) if NodeId::try_from(&id).is_ok() => {
                                self.registry
                                    .resolve_node(NodeId::try_from(&id).unwrap())
                                    .await
                            }
                            // We will disconnect session responsible for sending message. Doesn't matter
                            // what id was sent.
                            By::SessionId(_session) => {
                                if let Some(session) = self.find_session(from).await {
                                    self.close_session(session).await.ok();
                                }
                                return;
                            }
                            _ => return,
                        } {
                            log::info!(
                                "Node [{}] disconnected from Relay. Stopping forwarding..",
                                node.id
                            );
                            self.remove_node(node.id).await;
                        }
                    }
                    .boxed_local()
                }
                _ => {
                    log::debug!("Unhandled control packet: {:?}", kind);
                    return None;
                }
            };
            return Some(fut);
        }
        None
    }

    fn on_request(
        self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        log::trace!("Received request packet from {}: {:?}", from, request);

        let (request_id, kind) = match request {
            proto::Request {
                request_id,
                kind: Some(kind),
            } => (request_id, kind),
            _ => return None,
        };

        let fut = match kind {
            proto::request::Kind::Ping(request) => {
                async move { self.on_ping(session_id, request_id, from, request).await }
                    .boxed_local()
            }
            proto::request::Kind::Session(request) => async move {
                self.dispatch_session(session_id, request_id, from, request)
                    .await
            }
            .boxed_local(),
            _ => return None,
        };

        Some(fut)
    }

    fn on_forward(self, forward: Forward, from: SocketAddr) -> Option<LocalBoxFuture<'static, ()>> {
        let reliable = forward.is_reliable();
        let slot = forward.slot;

        if reliable && {
            let fast_lane = self.virtual_tcp_fast_lane.borrow();
            fast_lane.contains(&(from, slot))
        } {
            self.virtual_tcp.inject(forward.payload);
            return None;
        }

        let myself = self;
        let fut = async move {
            log::trace!(
                "[{}] received forward packet ({} B) via {}",
                myself.config.node_id,
                forward.payload.len(),
                from
            );

            let node = if slot == FORWARD_SLOT_ID {
                // Direct message from other Node.
                match { myself.state.read().await.nodes_addr.get(&from).cloned() } {
                    Some(id) => myself.registry.resolve_node(id).await?,
                    None => {
                        // In this case we can't establish session, because we don't have
                        // neither NodeId nor SlotId.
                        log::warn!(
                            "Forward packet from unknown address: {}. Can't resolve. Sending Disconnected message.",
                            from
                        );
                        return myself.send_disconnect(SessionId::from(forward.session_id), from).await;
                    }
                }
            } else {
                // Messages forwarded through relay server or other relay Node.
                match myself.registry.resolve_slot(slot).await {
                    Ok(node) => node,
                    Err(_) => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {}). Resolving..",
                            slot
                        );

                        let session = myself.server_session().await?;
                        let node = session.find_slot(slot).await?;
                        let ident = Identity::try_from(&node)?;

                        log::debug!("Attempting to establish connection to Node {} (slot {})", ident.node_id, node.slot);
                        myself
                            .resolve(ident.node_id.as_ref(), &node.endpoints, node.slot)
                            .await?
                    }
                }
            };

            if reliable {
                myself.virtual_tcp.receive(node, forward.payload).await;
                myself.virtual_tcp_fast_lane.borrow_mut().insert((from, slot));
            } else {
                myself.dispatch_unreliable(node.id, forward).await;
            }

            anyhow::Result::<()>::Ok(())
        }
        .map_err(|e| log::error!("On forward error: {}", e))
        .map(|_| ());

        tokio::task::spawn_local(fut);
        None
    }
}

impl SessionManagerState {
    /// External function should acquire lock.
    pub fn add_session(&mut self, addr: SocketAddr, session: Arc<Session>) {
        self.sessions.entry(addr).or_insert(session);
    }

    pub fn add_identities(
        &mut self,
        node_id: NodeId,
        identities: impl IntoIterator<Item = Identity>,
    ) {
        identities
            .into_iter()
            .filter(|ident| ident.node_id != node_id)
            .for_each(|ident| {
                self.nodes_identity
                    .insert(ident.node_id, (node_id, ident.public_key));
            })
    }
}
