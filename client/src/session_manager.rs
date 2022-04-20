use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable, LocalBoxFuture};
use futures::{FutureExt, SinkExt, TryFutureExt};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::net::SocketAddr;
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use ya_relay_core::challenge::{self, ChallengeDigest, CHALLENGE_DIFFICULTY};
use ya_relay_core::crypto::{CryptoProvider, PublicKey};
use ya_relay_core::error::Error;
use ya_relay_core::identity::Identity;
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_core::NodeId;
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::{Forward, RequestId, StatusCode};
use ya_relay_stack::{Channel, Connection};

use crate::client::{ClientConfig, ForwardSender, Forwarded};
use crate::dispatch::{dispatch, Dispatcher, Handler};
use crate::expire::track_sessions_expiration;
use crate::registry::{NodeEntry, NodesRegistry};
use crate::session::{Session, SessionError, SessionResult};
use crate::session_start::{SessionInitialization, StartingSessions};
use crate::virtual_layer::TcpLayer;

const SESSION_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);
const CHALLENGE_REQUEST_TIMEOUT: Duration = Duration::from_millis(8000);
const PAUSE_FWD_DELAY: i64 = 5;
const REVERSE_CONNECTION_TMP_TIMEOUT: u64 = 3;
const REVERSE_CONNECTION_REAL_TIMEOUT: i64 = 13;

/// This is the only layer, that should know real IP addresses and ports
/// of other peers. Other layers should use higher level abstractions
/// like `NodeId` or `SlotId`.
#[derive(Clone)]
pub struct SessionManager {
    sink: Option<OutStream>,
    pub config: Arc<ClientConfig>,

    pub virtual_tcp: TcpLayer,
    pub registry: NodesRegistry,

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
    forward_paused_till: Arc<RwLock<Option<DateTime<Utc>>>>,

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
            registry: NodesRegistry::new(),
            guarded: Default::default(),
            state: Arc::new(RwLock::new(SessionManagerState {
                public_addr: None,
                bind_addr: None,
                sessions: Default::default(),
                nodes_addr: Default::default(),
                nodes_identity: Default::default(),
                forward_reliable: Default::default(),
                forward_unreliable: Default::default(),
                forward_paused_till: Arc::new(RwLock::new(Default::default())),
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

        let (abort_dispatcher, abort_dispatcher_reg) = AbortHandle::new_pair();
        let (abort_expiration, abort_expiration_reg) = AbortHandle::new_pair();

        tokio::task::spawn_local(Abortable::new(
            dispatch(self.clone(), stream),
            abort_dispatcher_reg,
        ));
        tokio::task::spawn_local(Abortable::new(
            track_sessions_expiration(self.clone()),
            abort_expiration_reg,
        ));

        {
            let mut state = self.state.write().await;

            state.bind_addr.replace(bind_addr);
            state.handles.push(abort_dispatcher);
            state.handles.push(abort_expiration);
            state.starting_sessions = Some(StartingSessions::new(self.clone(), sink));
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

    async fn init_session_impl(
        &self,
        addr: SocketAddr,
        challenge: bool,
        remote_id: Option<NodeId>,
    ) -> SessionResult<Arc<Session>> {
        let this_id = self.config.node_id;

        let tmp_session = match self.temporary_session(addr).await? {
            SessionInitialization::Starting(tmp) => tmp,
            SessionInitialization::Finished(session) => return Ok(session),
            SessionInitialization::Failure(err) => return Err(err),
        };

        log::debug!("[{}] initializing session with {}", this_id, addr);

        let (request, raw_challenge) = challenge::prepare_challenge_request();
        let request = challenge.then(|| request).unwrap_or_default();
        let response = tmp_session
            .request::<proto::response::Session>(request.into(), vec![], SESSION_REQUEST_TIMEOUT)
            .await?;

        let session_id = SessionId::try_from(response.session_id.clone())?;
        let challenge_req = response.packet.challenge_req.ok_or_else(|| {
            anyhow!(
                "Expected ChallengeRequest while initializing session with {}",
                addr
            )
        })?;
        let challenge_handle = self.solve_challenge(challenge_req).await;

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
                CHALLENGE_REQUEST_TIMEOUT,
            )
            .await?;

        if session_id != &response.session_id[..] {
            let _ = tmp_session.disconnect().await;
            return Err(SessionError::Drop(format!(
                "[{}] init session id mismatch: {} vs {:?} (response)",
                this_id, session_id, response.session_id,
            )));
        }

        let (remote_id, identities) = match {
            if challenge {
                // Validate the challenge
                challenge::recover_identities_from_challenge::<ChallengeDigest>(
                    &raw_challenge,
                    CHALLENGE_DIFFICULTY,
                    response.packet.challenge_resp,
                    remote_id,
                )
            } else {
                Ok((Default::default(), Default::default()))
            }
        } {
            Ok(tuple) => tuple,
            Err(err) => {
                let _ = tmp_session.disconnect().await;
                return Err(SessionError::Drop(format!(
                    "{} while initializing session with {}",
                    err, addr
                )));
            }
        };

        let session = self
            .add_session(addr, session_id, remote_id, identities)
            .await?;

        log::trace!(
            "[{}] session {} established with address: {}",
            this_id,
            session_id,
            addr
        );
        Ok(session)
    }

    async fn init_session(
        &self,
        addr: SocketAddr,
        challenge: bool,
        remote_id: Option<NodeId>,
    ) -> SessionResult<Arc<Session>> {
        match self.init_session_impl(addr, challenge, remote_id).await {
            Ok(session) => Ok(session),
            Err(err) => {
                // Clear state and notify other futures waiting for this session to be initialized.
                if let Some(mut s) = self.state.read().await.starting_sessions.clone() {
                    s.remove_temporary_session(&addr, Err(err.clone()))
                }
                Err(err)
            }
        }
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
        let session = {
            let mut state = self.state.write().await;
            state.nodes_addr.insert(addr, node_id);
            state.p2p_sessions.entry(node_id).or_insert(session).clone()
        };

        log::info!(
            "Established P2P session {} with node [{}] ({})",
            session.id,
            node_id,
            addr
        );

        Ok(session)
    }

    async fn init_server_session(&self, addr: SocketAddr) -> anyhow::Result<Arc<Session>> {
        log::info!("Initializing session with NET relay server at: {}", addr);

        let session = self.init_session(addr, false, None).await?;

        self.state
            .write()
            .await
            .sessions
            .insert(addr, session.clone());

        log::info!(
            "Established session {} with NET relay server ({})",
            session.id,
            addr
        );

        Ok(session)
    }

    async fn temporary_session(&self, addr: SocketAddr) -> anyhow::Result<SessionInitialization> {
        if let Some(mut starting_sessions) = self.state.write().await.starting_sessions.clone() {
            Ok(starting_sessions.temporary_session(addr).await)
        } else {
            bail!("StartingSessions struct not initialized.")
        }
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

        // Incoming sessions are always p2p sessions, so we use slot=0.
        self.registry.add_node(node_id, session.clone(), 0).await;
        {
            let mut state = self.state.write().await;

            state.add_session(addr, session.clone());
            state.add_identities(node_id, identities);

            state.nodes_addr.insert(addr, node_id);

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
        log::trace!(
            "Removing Node [{}] information. Stopping communication..",
            node_id
        );

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
        let server_session = self.server_session().await?;
        let node = server_session.find_node(node_id).await?;
        let ident = Identity::try_from(&node)?;

        // Find node on server. p2p session will be established, if possible. Otherwise
        // communication will be forwarder through relay server.
        Ok(self
            .resolve(ident.node_id.as_ref(), &node.endpoints, node.slot)
            .await?
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
        let (conn, tx, rx) = {
            match {
                let state = self.state.read().await;
                state.forward_reliable.get(&node_id).cloned()
            } {
                Some(tx) => return Ok(tx),
                None => {
                    let conn = self.virtual_tcp.connect(node.clone()).await?;
                    let (tx, rx) = mpsc::channel(1);
                    {
                        let mut state = self.state.write().await;
                        state.forward_reliable.insert(node_id, tx.clone());
                    }
                    (conn, tx, rx)
                }
            }
        };

        tokio::task::spawn_local(self.clone().forward_reliable_handler(conn, node, rx));
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
        let paused_check = { self.state.read().await.forward_paused_till.clone() };
        let session = node.session.clone();

        while let Some(payload) = self
            .virtual_tcp
            .get_next_fwd_payload(&mut rx, paused_check.clone())
            .await
        {
            log::trace!(
                "Forwarding message to {} through {} (session id: {})",
                node.id,
                session.remote,
                session.id
            );

            if let Err(err) = self.virtual_tcp.send(payload, connection).await {
                log::trace!(
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

        self.remove_node(node.id).await;
        rx.close();

        log::trace!(
            "[{}] forward: disconnected from: {}",
            node.id,
            session.remote
        );
    }

    async fn forward_unreliable_handler(
        self,
        session: Arc<Session>,
        node: NodeEntry,
        mut rx: mpsc::Receiver<Vec<u8>>,
    ) {
        let paused_check = { self.state.read().await.forward_paused_till.clone() };

        while let Some(payload) = self
            .virtual_tcp
            .get_next_fwd_payload(&mut rx, paused_check.clone())
            .await
        {
            log::trace!(
                "Forwarding message (U) to {} through {} (session id: {})",
                node.id,
                session.remote,
                session.id
            );

            let forward = Forward::unreliable(session.id, node.slot, payload);
            if let Err(error) = session.send(forward).await {
                log::trace!(
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

        self.remove_node(node.id).await;
        rx.close();

        log::trace!(
            "[{}] forward (U): disconnected from: {}",
            node.id,
            session.remote
        );
    }

    async fn resolve(
        &self,
        node_id: &[u8],
        endpoints: &[proto::Endpoint],
        slot: u32,
    ) -> anyhow::Result<NodeEntry> {
        let node_id: NodeId = node_id.try_into()?;
        if node_id == self.config.node_id {
            bail!("remote id belongs to this node");
        }

        // Check whether we are already connected to a node with this id
        if let Ok(entry) = self.registry.resolve_node(node_id).await {
            return Ok(entry);
        }

        let own_addrs: Vec<_> = {
            let state = self.state.read().await;
            vec![state.bind_addr, state.public_addr]
                .into_iter()
                .flatten()
                .collect()
        };
        let addrs: Vec<SocketAddr> = endpoints
            .iter()
            .cloned()
            .filter_map(|e| e.try_into().ok())
            .filter(|a| !own_addrs.iter().any(|o| o == a))
            .collect();

        // Check whether we are already connected to a node with any of the addresses
        self.assert_not_connected(&addrs).await?;

        let target = self.guarded.get_or_create(node_id, &addrs).await;
        let lock = target.lock.clone();
        let _guard = lock.write().await;

        let this = self.clone();
        self.try_resolve(node_id, addrs, slot)
            .then(move |result| async move {
                let mut guarded = this.guarded.state.write().await;
                guarded.remove(&target);
                result
            })
            .await
    }

    async fn try_resolve(
        &self,
        node_id: NodeId,
        addrs: Vec<SocketAddr>,
        slot: u32,
    ) -> anyhow::Result<NodeEntry> {
        // If node has public IP, we can establish direct session with him
        // instead of forwarding messages through relay.
        let (session, resolved_slot) = match self.try_direct_session(node_id, addrs).await {
            // If we send packets directly, slot will be always 0.
            Ok(session) => (session, 0),
            // Fatal error, cease further communication
            Err(SessionError::Drop(e)) => {
                log::warn!("{}", e);
                return Err(SessionError::Drop(e).into());
            }
            // Recoverable error, continue
            Err(SessionError::Retry(e)) => {
                log::info!("{}", e);
                (self.server_session().await?, slot)
            }
        };

        if resolved_slot != 0 {
            log::info!(
                "Using relay Server to forward packets to [{}] (slot {})",
                node_id,
                resolved_slot
            );
        }

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
    ) -> SessionResult<Arc<Session>> {
        if addrs.is_empty() {
            // We are trying to connect to Node without public IP. Trying to send
            // ReverseConnection message, so Node will try to connect to us.
            if self.get_public_addr().await.is_some() {
                return Ok(self.try_reverse_connection(node_id).await?);
            }

            return Err(anyhow!(
                "Node [{}] has no public endpoints. Not establishing p2p session",
                node_id
            )
            .into());
        }

        for addr in addrs {
            match self.init_p2p_session(addr, node_id).await {
                Err(SessionError::Retry(err)) => {
                    log::debug!(
                        "Failed to establish p2p session with node [{}], address: {}. Error: {}",
                        node_id,
                        addr,
                        err
                    )
                }
                result => return result,
            }
        }

        Err(anyhow!(
            "All attempts to establish direct session with node [{}] failed",
            node_id
        )
        .into())
    }

    async fn try_reverse_connection(&self, node_id: NodeId) -> anyhow::Result<Arc<Session>> {
        log::debug!(
            "Request reverse connection. me={}, remote={}",
            self.config.node_id,
            node_id
        );

        let mut start_registration;
        {
            let starting_session = { self.state.read().await.starting_sessions.clone() }
                .ok_or_else(|| anyhow!("Starting sessions not loaded yet"))?;

            start_registration = starting_session.register_waiting_for_node(node_id);

            let server_session = self.server_session().await?;
            server_session.reverse_connection(node_id).await?;
            log::trace!("ReverseConnection - Requested with node {}", &node_id);

            tokio::time::timeout(
                Duration::from_secs(REVERSE_CONNECTION_TMP_TIMEOUT),
                start_registration.rx()?,
            )
            .await??;
        }

        let deadline = Utc::now().add(chrono::Duration::seconds(REVERSE_CONNECTION_REAL_TIMEOUT));
        loop {
            if let Ok(node) = self.registry.resolve_node(node_id).await {
                log::debug!("ReverseConnection - Got session with node. {}", &node_id);
                return Ok(node.session);
            }
            if deadline <= Utc::now() {
                log::debug!("ReverseConnection - Direct session timed out. {}", &node_id);
                break;
            }
            log::trace!(
                "ReverseConnection - no session yet, waiting... {}",
                &node_id
            );
            tokio::time::delay_for(tokio::time::Duration::from_millis(100)).await;
        }

        bail!(
            "Not able to setup ReverseConnection within timeout with node {}",
            node_id
        )
    }

    pub async fn server_session(&self) -> anyhow::Result<Arc<Session>> {
        if let Some(session) = self.get_server_session().await {
            return Ok(session);
        }

        let session = self.init_server_session(self.config.srv_addr).await?;
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

    async fn assert_not_connected(&self, addrs: &[SocketAddr]) -> anyhow::Result<()> {
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

    pub(crate) async fn error_response(
        &self,
        req_id: u64,
        id: Vec<u8>,
        addr: &SocketAddr,
        error: Error,
    ) {
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

    async fn send(&self, packet: impl Into<PacketKind>, addr: SocketAddr) -> anyhow::Result<()> {
        let mut stream = self.out_stream()?;
        Ok(stream.send((packet.into(), addr)).await?)
    }

    pub async fn solve_challenge<'a>(
        &self,
        request: proto::ChallengeRequest,
    ) -> LocalBoxFuture<'a, anyhow::Result<proto::ChallengeResponse>> {
        let mut crypto_vec = match self.config.crypto.get(self.config.node_id).await {
            Ok(crypto) => vec![crypto],
            Err(e) => return Box::pin(futures::future::err(e)),
        };
        let aliases = match self.config.crypto.aliases().await {
            Ok(aliases) => aliases,
            Err(e) => return Box::pin(futures::future::err(e)),
        };
        for alias in aliases {
            match self.config.crypto.get(alias).await {
                Ok(crypto) => crypto_vec.push(crypto),
                _ => log::debug!("Unable to retrieve Crypto instance for id [{}]", alias),
            }
        }

        // Compute challenge in different thread to avoid blocking runtime.
        // Note: computing starts here, not after awaiting.
        challenge::solve::<ChallengeDigest, _>(request.challenge, request.difficulty, crypto_vec)
            .boxed_local()
    }
}

impl Handler for SessionManager {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        let handler = self.clone();
        async move {
            let state = handler.state.read().await;

            // We get either dispatcher for already existing Session from self,
            // or temporary session, that is during creation.
            state
                .sessions
                .get(&from)
                .map(|session| session.dispatcher())
                .or_else(|| {
                    state
                        .starting_sessions
                        .clone()
                        .and_then(|entity| entity.dispatcher(from))
                })
        }
        .boxed_local()
    }

    fn on_control(
        &self,
        _session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!("Received control packet from {}: {:?}", from, control);

        if let Some(kind) = control.kind {
            match kind {
                ya_relay_proto::proto::control::Kind::ReverseConnection(message) => {
                    let myself = self.clone();
                    tokio::task::spawn_local(async move {
                        log::info!(
                            "Got ReverseConnection message. node={:?}, endpoints={:?}",
                            message.node_id,
                            message.endpoints
                        );

                        if message.endpoints.is_empty() {
                            log::warn!("Got ReverseConnection with no endpoints to connect to.");
                            return;
                        }

                        match myself
                            .resolve(&message.node_id, &message.endpoints, 0)
                            .await
                        {
                            Err(e) => log::warn!(
                                "Failed to resolve reverse connection. node_id={:?} error={}",
                                message.node_id,
                                e
                            ),
                            Ok(_) => log::trace!("ReverseConnection succeeded: {:?}", message),
                        };
                    });
                }
                ya_relay_proto::proto::control::Kind::PauseForwarding(message) => {
                    return async move {
                        log::trace!("Got Pause! {:?}", message);
                        let lock = {
                            let state = self.state.read().await;
                            state.forward_paused_till.clone()
                        };
                        lock.write()
                            .await
                            .replace(Utc::now() + chrono::Duration::seconds(PAUSE_FWD_DELAY));
                    }
                    .boxed_local();
                }
                ya_relay_proto::proto::control::Kind::ResumeForwarding(message) => {
                    return async move {
                        log::trace!("Got Resume! {:?}", message);
                        let lock = {
                            let state = self.state.read().await;
                            state.forward_paused_till.clone()
                        };
                        lock.write().await.take();
                    }
                    .boxed_local();
                }
                ya_relay_proto::proto::control::Kind::Disconnected(
                    proto::control::Disconnected { by: Some(by) },
                ) => {
                    return async move {
                        log::trace!("Got Disconnected! {:?}", by);

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
                    .boxed_local();
                }
                _ => {
                    log::trace!("Un-handled control packet: {:?}", kind)
                }
            }
        }
        Box::pin(futures::future::ready(()))
    }

    fn on_request(
        &self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::trace!("Received request packet from {}: {:?}", from, request);

        let (request_id, kind) = match request {
            proto::Request {
                request_id,
                kind: Some(kind),
            } => (request_id, kind),
            _ => return Box::pin(futures::future::ready(())),
        };

        match kind {
            proto::request::Kind::Ping(request) => {
                Box::pin(self.on_ping(session_id, request_id, from, request))
            }
            proto::request::Kind::Session(request) => {
                Box::pin(self.dispatch_session(session_id, request_id, from, request))
            }
            _ => Box::pin(futures::future::ready(())),
        }
    }

    fn on_forward(&self, forward: proto::Forward, from: SocketAddr) -> LocalBoxFuture<()> {
        let myself = self.clone();
        let fut = async move {
            log::trace!(
                "[{}] received forward packet ({} B) via {}",
                myself.config.node_id,
                forward.payload.len(),
                from
            );

            let node = if forward.slot == 0 {
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
                match myself.registry.resolve_slot(forward.slot).await {
                    Ok(node) => node,
                    Err(_) => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {}). Resolving..",
                            forward.slot
                        );

                        let session = myself.server_session().await?;
                        let node = session.find_slot(forward.slot).await?;
                        let ident = Identity::try_from(&node)?;

                        log::debug!("Establishing connection to Node {} (slot {})", ident.node_id, node.slot);
                        myself
                            .resolve(ident.node_id.as_ref(), &node.endpoints, node.slot)
                            .await?
                    }
                }
            };

            if forward.is_reliable() {
                myself.virtual_tcp.receive(node, forward.payload).await;
            } else {
                myself.dispatch_unreliable(node.id, forward).await;
            }
            anyhow::Result::<()>::Ok(())
        }
        .map_err(|e| log::error!("On forward error: {}", e))
        .map(|_| ());

        tokio::task::spawn_local(fut);
        Box::pin(futures::future::ready(()))
    }
}

impl SessionManagerState {
    /// External function should acquire lock.
    pub fn add_session(&mut self, addr: SocketAddr, session: Arc<Session>) {
        if let Some(mut s) = self.starting_sessions.clone() {
            s.remove_temporary_session(&session.remote, Ok(session.clone()))
        }

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

#[derive(Clone, Default)]
struct GuardedSessions {
    state: Arc<RwLock<GuardedSessionsState>>,
}

impl GuardedSessions {
    async fn get_or_create<'a>(&self, node_id: NodeId, addrs: &[SocketAddr]) -> SessionTarget {
        if let Some(target) = {
            let state = self.state.read().await;
            state.find(node_id, addrs)
        } {
            return target;
        }

        let target = SessionTarget::new(node_id, addrs.to_vec());
        let mut state = self.state.write().await;
        state.add(target.clone());

        target
    }
}

#[derive(Default)]
struct GuardedSessionsState {
    by_node_id: HashMap<NodeId, SessionTarget>,
    by_addr: HashMap<SocketAddr, SessionTarget>,
}

impl GuardedSessionsState {
    fn find(&self, node_id: NodeId, addrs: &[SocketAddr]) -> Option<SessionTarget> {
        self.by_node_id
            .get(&node_id)
            .or_else(|| addrs.iter().filter_map(|a| self.by_addr.get(a)).next())
            .cloned()
    }

    fn add(&mut self, target: SessionTarget) {
        for addr in target.addresses.iter() {
            self.by_addr.insert(*addr, target.clone());
        }
        self.by_node_id.insert(target.node_id, target);
    }

    fn remove(&mut self, target: &SessionTarget) {
        for addr in target.addresses.iter() {
            self.by_addr.remove(addr);
        }
        self.by_node_id.remove(&target.node_id);
    }
}

#[derive(Clone)]
struct SessionTarget {
    node_id: NodeId,
    addresses: Arc<Vec<SocketAddr>>,
    lock: Arc<RwLock<()>>,
}

impl SessionTarget {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>) -> Self {
        Self {
            node_id,
            addresses: Arc::new(addresses),
            lock: Default::default(),
        }
    }
}
