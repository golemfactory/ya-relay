#![allow(dead_code)]
#![allow(unused)]

use anyhow::{anyhow, bail};
use derive_more::Display;
use futures::future::{AbortHandle, LocalBoxFuture};
use futures::{FutureExt, SinkExt, TryFutureExt};
use log::log;
use std::collections::{HashMap, VecDeque};
use std::convert::{Infallible, TryFrom, TryInto};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, Semaphore};

use crate::client::{ClientConfig, Forwarded};
use crate::ForwardReceiver;

use crate::_dispatch::{dispatch, Dispatcher, Handler};
use crate::_encryption::Encryption;
use crate::_error::{SessionError, SessionInitError, SessionResult};
use crate::_expire::track_sessions_expiration;
use crate::_routing_session::{DirectSession, NodeEntry, NodeRouting, RoutingSender};
use crate::_session::RawSession;
use crate::_session_guard::{GuardedSessions, SessionLock, SessionPermit};
use crate::_session_protocol::SessionProtocol;

use ya_relay_core::identity::Identity;
use ya_relay_core::session::{Endpoint, NodeInfo, SessionId, TransportType};
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_core::{challenge, NodeId};
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::control::ReverseConnection;
use ya_relay_proto::proto::{Forward, RequestId, SlotId, FORWARD_SLOT_ID};
use ya_relay_proto::{codec, proto};
use ya_relay_stack::Channel;

type ReqFingerprint = (Vec<u8>, u64);

#[derive(Copy, Clone, Display, PartialEq, Eq)]
pub enum ConnectionMethod {
    Direct,
    Reverse,
    Relay,
    // Here should be NAT punching type(s) defined in the future
}

/// Responsible for establishing/receiving connections from other Nodes.
/// Hides from upper layers the decisions, how to route packets to desired location
#[derive(Clone)]
pub struct SessionLayer {
    pub config: Arc<ClientConfig>,
    sink: Option<OutStream>,

    pub(crate) state: Arc<RwLock<SessionLayerState>>,

    pub(crate) guards: GuardedSessions,
    ingress_channel: Channel<Forwarded>,

    // TODO: Could be per `Session`.
    processed_requests: Arc<Mutex<VecDeque<ReqFingerprint>>>,
}

#[derive(Default)]
pub struct SessionLayerState {
    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    public_addr: Option<SocketAddr>,
    /// Equals to `None` when not listening
    pub(crate) bind_addr: Option<SocketAddr>,

    pub nodes: HashMap<NodeId, Arc<NodeRouting>>,
    pub p2p_sessions: HashMap<SocketAddr, Arc<DirectSession>>,

    pub(crate) init_protocol: Option<SessionProtocol>,

    // Collection of background tasks that must be stopped on shutdown.
    pub handles: Vec<AbortHandle>,
}

impl SessionLayer {
    pub fn new(config: Arc<ClientConfig>) -> SessionLayer {
        let state = SessionLayerState::default();

        SessionLayer {
            sink: None,
            config,
            state: Arc::new(RwLock::new(state)),
            guards: Default::default(),
            ingress_channel: Default::default(),
            processed_requests: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub async fn spawn(&mut self) -> anyhow::Result<SocketAddr> {
        let (stream, sink, bind_addr) = udp_bind(&self.config.bind_url).await?;

        self.sink = Some(sink.clone());

        let abort_dispatcher = spawn_local_abortable(dispatch(self.clone(), stream));
        let abort_expiration = spawn_local_abortable(track_sessions_expiration(self.clone()));

        {
            let mut state = self.state.write().await;

            state.bind_addr.replace(bind_addr);
            state.handles.push(abort_dispatcher);
            state.handles.push(abort_expiration);
            state.init_protocol = Some(SessionProtocol::new(self.clone(), sink));
        }

        Ok(bind_addr)
    }

    pub async fn get_public_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.public_addr
    }

    pub async fn get_local_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.bind_addr
    }

    pub async fn get_node_routing(&self, node_id: NodeId) -> Option<RoutingSender> {
        let state = self.state.read().await;
        state
            .nodes
            .get(&node_id)
            .cloned()
            .map(|routing| RoutingSender::from_node_routing(node_id, routing, self.clone()))
    }

    pub async fn find_session(&self, addr: SocketAddr) -> Option<Arc<DirectSession>> {
        let state = self.state.read().await;
        state.p2p_sessions.get(&addr).cloned()
    }

    /// Queries information about Node from relay server.
    /// TODO: Update `GuardedSessions` information.
    pub async fn query_node_info(&self, node_id: NodeId) -> anyhow::Result<NodeInfo> {
        let server_session = self
            .server_session()
            .await
            .map_err(|e| anyhow!("Failed to get relay server session: {e}"))?;
        let node = server_session
            .raw
            .find_node(node_id)
            .await
            .map_err(|e| anyhow!("Failed to find Node on relay server: {e}"))?;

        NodeInfo::try_from(node)
    }

    async fn filter_own_addresses(&self, endpoints: &[Endpoint]) -> Vec<SocketAddr> {
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
            .map(|e| e.address)
            .filter(|a| !own_addrs.iter().any(|o| o == a))
            .collect()
    }

    pub async fn abort_initializations(
        &self,
        session: Arc<RawSession>,
    ) -> Result<(), SessionError> {
        unimplemented!()
    }

    pub async fn disconnect_node(&self, node_id: NodeId) -> Result<(), SessionError> {
        unimplemented!()
    }

    /// Registers initialized session to be ready to use.
    pub(crate) async fn register_session(
        &self,
        addr: SocketAddr,
        id: SessionId,
        node_id: NodeId,
        identities: impl IntoIterator<Item = Identity>,
    ) -> anyhow::Result<Arc<DirectSession>> {
        log::trace!("Calling register_session {id} {addr} {node_id}");

        let session = RawSession::new(addr, id, self.out_stream()?);
        let direct = DirectSession::new(node_id, identities, session.clone())
            .map_err(|e| anyhow!("Registering session for node [{node_id}]: {e}"))?;

        let routing = NodeRouting::new(
            direct.owner.clone(),
            direct.clone(),
            Encryption {
                crypto: self.config.crypto.clone(),
            },
        );

        {
            let mut state = self.state.write().await;

            state.p2p_sessions.insert(session.remote, direct.clone());
            for id in &direct.owner.identities {
                state.nodes.insert(id.node_id, routing.clone());
            }

            log::trace!("Saved node session {id} [{node_id}] {addr}")
        }
        Ok(direct)
    }

    pub fn out_stream(&self) -> anyhow::Result<OutStream> {
        self.sink
            .clone()
            .ok_or_else(|| anyhow!("Network sink not initialized"))
    }

    pub fn receiver(&self) -> Option<ForwardReceiver> {
        self.ingress_channel.receiver()
    }

    async fn send_disconnect(&self, session_id: SessionId, addr: SocketAddr) -> anyhow::Result<()> {
        // Don't use temporary session, because we don't want to initialize session
        // with this address, nor receive the response.
        let session = RawSession::new(addr, session_id, self.out_stream()?);
        session.disconnect().await
    }

    /// Returns `RoutingSender` which can be used to send packets to desired Node.
    /// Creates session with Node if necessary. Function will choose the most optimal
    /// route to destination.
    pub async fn session(&self, node_id: NodeId) -> Result<RoutingSender, SessionError> {
        if let Some(routing) = self.get_node_routing(node_id).await {
            log::debug!("Resolving Node [{node_id}]. Returning already existing connection (route = {} ({})).", routing.route(), routing.session_type());

            return Ok(routing);
        }

        // Query relay server for Node information, we need to find out default id and aliases.
        // TODO: We could avoid querying the same information multiple times in case this function
        //       is called from multiple places at the same time.
        let info = self
            .query_node_info(node_id)
            .await
            .map_err(|e| SessionError::NotFound(format!("Error querying node {node_id}: {e}")))?;

        if info.identities.is_empty() {
            return Err(SessionError::Unexpected(
                "No default id from relay response for [{node_id}]".to_string(),
            ));
        }

        let remote_id = info.identities[0].node_id;

        if remote_id == self.config.node_id {
            return Err(SessionError::BadRequest(format!(
                "Remote id [{remote_id}] belongs to this node."
            )));
        }

        if node_id != remote_id {
            log::debug!("Resolving [{node_id}] as alias of [{remote_id}]");
        }

        // TODO: Should we filter addresses or reject attempt to connect?
        //       In previous implementation we were filtering, but I don't know the rationale.
        let addrs: Vec<SocketAddr> = self.filter_own_addresses(&info.endpoints).await;

        let mut waiter = match self.guards.lock_outgoing(remote_id, &addrs).await {
            SessionLock::Permit(permit) => {
                let myself = self.clone();
                let waiter = permit.guard.awaiting_notifier();

                // Caller of `SessionLayer:session` can drop function execution at any time, but
                // other threads can be waiting for initialization as well. That's why we initialize
                // session in other task and wait for finish.
                tokio::task::spawn_local(async move {
                    myself
                        .resolve(remote_id, &permit, &[])
                        .await
                        .map_err(|e| log::info!("Failed init session with {remote_id}. Error: {e}"))
                        .ok();
                });

                waiter
            }
            SessionLock::Wait(waiter) => waiter,
        };

        waiter
            .await_for_finish()
            .await
            .map_err(|e| SessionError::Generic(e.to_string()))?;

        // TODO: How should we react to non-existing session in this place?
        self.get_node_routing(node_id)
            .await
            .ok_or(SessionError::Internal(format!(
                "Session with [{remote_id}] closed immediately after establishing."
            )))
    }

    pub async fn server_session(&self) -> Result<Arc<DirectSession>, SessionError> {
        unimplemented!()
    }

    pub async fn close_session(&self, session: Arc<DirectSession>) -> Result<(), SessionError> {
        unimplemented!()
    }

    /// Resolves connection to target Node using the best method available.
    /// First tries to establish p2p session and uses relayed connection as a fallback.
    /// You can list (`dont_use` field) methods that shouldn't be attempted.
    /// This is necessary in case we react to `ReverseConnection` message, because otherwise,
    /// we could fall into infinite loop of `ReverseConnection` attempts.
    async fn resolve(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
        dont_use: &[ConnectionMethod],
    ) -> Result<Arc<DirectSession>, SessionError> {
        log::debug!("Resolving route to [{node_id}].");

        if !dont_use.contains(&ConnectionMethod::Direct) {
            log::debug!("Attempting to establish direct p2p connection with [{node_id}].");

            match self.try_direct_session(node_id, &permit).await {
                Ok(session) => return Ok(session),
                Err(e) => log::debug!("Can't establish direct p2p session with [{node_id}]. {e}"),
            }
        } else {
            log::debug!("Omitting attempt to establish direct p2p connection with [{node_id}].");
        }

        if !dont_use.contains(&ConnectionMethod::Reverse) {
            log::debug!("Attempting to establish reverse p2p connection with [{node_id}].");

            match self.try_reverse_connection(node_id, &permit).await {
                Ok(session) => return Ok(session),
                Err(e) => log::debug!("Can't establish reverse p2p session with [{node_id}]. {e}"),
            }
        } else {
            log::debug!("Omitting attempt to establish reverse p2p connection with [{node_id}].");
        }

        log::debug!("All attempts to establish direct session with node [{node_id}] failed");

        if !dont_use.contains(&ConnectionMethod::Relay) {
            log::info!("Attempting to use relay Server to forward packets to [{node_id}]");

            match self.try_relayed_connection(node_id, &permit).await {
                Ok(session) => return Ok(session),
                Err(e) => log::debug!("Can't use relayed connection with [{node_id}]. {e}"),
            }
        } else {
            log::debug!("Not using relayed connection with [{node_id}].");
        }

        Err(SessionError::Generic(
            "All attempts to establish session with node [{node_id}] failed".to_string(),
        ))
    }

    pub async fn try_direct_session(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> SessionResult<Arc<DirectSession>> {
        let protocol = self.get_protocol().await?;
        let addrs = permit.guard.public_addresses().await;
        if addrs.is_empty() {
            return Err(SessionError::BadRequest(
                "Node [{node_id}] has no public endpoints.".to_string(),
            ));
        }

        // Try to connect to remote Node's public endpoints.
        for addr in addrs {
            match protocol.init_p2p_session(addr, permit).await {
                Ok(session) => return Ok(session),
                // We can probably recover from these errors.
                Err(SessionInitError::Relay(_, e)) | Err(SessionInitError::P2P(_, e)) => match e {
                    SessionError::Internal(_)
                    | SessionError::Network(_)
                    | SessionError::Timeout(_)
                    | SessionError::Unexpected(_) => {
                        log::debug!(
                            "Failed to establish p2p session with node [{node_id}], using address: {addr}. Error: {e}"
                        )
                    }
                    // Rest errors means that there is no reason to try again.
                    // TODO: `SessionError::Protocol` error sometimes can be recovered from and sometimes
                    //       shouldn't. We should organize errors better.
                    e => return Err(e),
                },
            }
        }

        Err(SessionError::Generic(
            "All attempts to establish direct session with node [{node_id}] failed".to_string(),
        ))
    }

    async fn try_reverse_connection(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionError> {
        // We are trying to connect to Node without public IP. Send `ReverseConnection` message,
        // so Node will connect to us.
        if self.get_public_addr().await.is_some() {
            return Err(SessionError::BadRequest(
                "We don't have public endpoints.".to_string(),
            ));
        }

        log::info!(
            "Request reverse connection. me={}, remote={node_id}",
            self.config.node_id,
        );

        todo!()

        //
        // let mut awaiting = self.guarded.register_waiting_for_node(node_id).await?;
        //
        // let server_session = self.server_session().await?;
        // server_session.reverse_connection(node_id).await?;
        //
        // log::debug!("ReverseConnection requested with node [{node_id}]");
        //
        // // We don't want to wait for connection finish, because we need longer timeout for this.
        // // But if we won't receive any message from other Node, we would like to exit early,
        // // to try out different connection methods.
        // tokio::time::timeout(
        //     self.config.reverse_connection_tmp_timeout,
        //     awaiting.wait_for_first_message(),
        // )
        // .await?;
        //
        // // If we have first handshake message from other node, we can wait with
        // // longer timeout now, because we can hope, that this node is responsive.
        // let result = tokio::time::timeout(
        //     self.config.reverse_connection_real_timeout,
        //     awaiting.wait_for_connection(),
        // )
        // .await;
        //
        // match result {
        //     Ok(Ok(session)) => {
        //         log::info!("ReverseConnection - got session with node: [{node_id}]");
        //         Ok(session)
        //     }
        //     Ok(Err(e)) => {
        //         log::info!("ReverseConnection - failed to establish session with: [{node_id}]");
        //         Err(e.into())
        //     }
        //     Err(_) => {
        //         log::info!(
        //             "ReverseConnection - waiting for session timed out ({}). Node: [{node_id}]",
        //             humantime::format_duration(self.config.reverse_connection_real_timeout)
        //         );
        //         bail!("Not able to setup ReverseConnection within timeout with node: [{node_id}]")
        //     }
        // }
    }

    async fn try_relayed_connection(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionError> {
        // Currently only single server supported. In the future we could use multiple
        // relays or use other p2p Nodes to forward traffic.
        // We could even route traffic through many Nodes/Servers at the same time.
        let server = self.server_session().await?;
        let server_id = server.owner.default_id.node_id;
        let addr = server.raw.remote;

        // The only information about Node, that we don't have yet, is its SlotId.
        let node = server.raw.find_node(node_id).await.map_err(|e| {
            SessionError::NotFound(format!(
                "Failed to find Node [{node_id}] on relay server [{server_id}] ({addr}). {e}"
            ))
        })?;
        let slot: SlotId = node.slot;

        log::info!("Using relay server [{server_id}] ({addr}) to forward packets to [{node_id}] (slot {slot})");

        //server.forwards

        let ids = permit.guard.identities().await;
        let routing = NodeRouting::new(
            ids,
            server.clone(),
            Encryption {
                crypto: self.config.crypto.clone(),
            },
        );
        todo!()
    }

    pub async fn dispatch_session<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) -> anyhow::Result<()> {
        let protocol = self.get_protocol().await?;

        if session_id.is_empty() {
            // Empty `session_id` indicates attempt to initialize session.
            let remote_id = challenge::recover_default_node_id(&request)?;

            return match self.guards.lock_incoming(remote_id, &[from]).await {
                SessionLock::Permit(permit) => Ok(protocol
                    .new_session(request_id, from, &permit, request)
                    .await?),
                SessionLock::Wait(_) => {
                    // We didn't get lock, so probably other thread is in charge of initializing session.
                    // TODO: This could be situation, when both sides tried to initialize connection.
                    //       Maybe we should implement algorithm, which would decide, who has precedence.
                    Ok(())
                }
            };
        }

        let session_id = SessionId::try_from(session_id)?;
        Ok(protocol
            .existing_session(session_id, request_id, from, request)
            .await?)
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
            log::warn!("Unable to send Pong to {from}: {e}");
        }
    }

    pub async fn on_disconnected(
        &self,
        session_id: Vec<u8>,
        from: SocketAddr,
        by: By,
    ) -> anyhow::Result<()> {
        log::debug!("Got `Disconnected` from {from}");

        // SessionId should be valid, otherwise this is some unknown session
        // so we should be cautious, when processing it.
        let session_id = SessionId::try_from(session_id)?;

        if let Ok(node) = match by {
            By::Slot(id) => match self.find_session(from).await {
                Some(session) => session.remove_slot(id).await,
                None => Err(anyhow!("Session with {from} not found")),
            },
            By::NodeId(id) => NodeId::try_from(&id).map_err(anyhow::Error::from),
            // We will disconnect session responsible for sending message. Doesn't matter
            // what id was sent.
            By::SessionId(to_close) => {
                let to_close = SessionId::try_from(to_close)?;
                if to_close != session_id {
                    bail!("Session id mismatch. Sender: {session_id}, session to close: {to_close}. Might be exploit attempt..")
                }

                match self.find_session(from).await {
                    Some(session) => {
                        if session.raw.id != session_id {
                            bail!("Unexpected Session id: {session_id}")
                        }

                        self.close_session(session).await?;
                        return Ok(());
                    }
                    None => {
                        // If we didn't find established session, maybe we have temporary session
                        // during initialization.
                        if let Some(session) = self.dispatcher(from).await {
                            return Ok(self.abort_initializations(session).await?);
                        }
                        Err(anyhow!("Session with {from} not found"))
                    }
                }
            }
        } {
            log::info!("Node [{node}] disconnected from Relay. Stopping forwarding..");
            self.disconnect_node(node).await.ok();
        }
        Ok(())
    }

    pub async fn on_reverse_connection(
        &self,
        session_id: Vec<u8>,
        from: SocketAddr,
        message: ReverseConnection,
    ) -> anyhow::Result<()> {
        let node_id = NodeId::try_from(&message.node_id).map_err(|e| {
            anyhow!(
                "ReverseConnection with invalid NodeId: {:?}",
                message.node_id
            )
        })?;

        log::info!(
            "Got ReverseConnection message. node={node_id}, endpoints={:?}",
            message.endpoints
        );

        if message.endpoints.is_empty() {
            bail!("Got ReverseConnection with no endpoints to connect to.")
        }

        let permit = match self.guards.lock_outgoing(node_id, &[from]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(waiter) => return Ok(()),
        };

        // Don't try to use `ReverseConnection`, when handling `ReverseConnection`.
        self.resolve(node_id, &permit, &[ConnectionMethod::Reverse])
            .await
            .map_err(|e| {
                anyhow!("Failed to resolve ReverseConnection. node_id={node_id} error={e}")
            })?;

        log::trace!("ReverseConnection succeeded: {:?}", message);
        Ok(())
    }

    async fn send(&self, packet: impl Into<PacketKind>, addr: SocketAddr) -> anyhow::Result<()> {
        let mut stream = self.out_stream()?;
        Ok(stream.send((packet.into(), addr)).await?)
    }

    fn record_duplicate(&self, session_id: Vec<u8>, request_id: u64) {
        const REQ_DEDUPLICATE_BUF_SIZE: usize = 32;

        let mut processed_requests = self.processed_requests.lock().unwrap();
        processed_requests.push_back((session_id, request_id));
        if processed_requests.len() > REQ_DEDUPLICATE_BUF_SIZE {
            processed_requests.pop_front();
        }
    }

    fn is_request_duplicate(&self, session_id: &Vec<u8>, request_id: u64) -> bool {
        self.processed_requests
            .lock()
            .unwrap()
            .iter()
            .any(|(sess_id, req_id)| *req_id == request_id && sess_id == session_id)
    }

    async fn get_protocol(&self) -> Result<SessionProtocol, SessionError> {
        match self.state.read().await.init_protocol.clone() {
            None => {
                return Err(SessionError::Internal(
                    "`SessionProtocol` empty (not initialized?)".to_string(),
                ))
            }
            Some(protocol) => Ok(protocol),
        }
    }
}

impl Handler for SessionLayer {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<RawSession>>> {
        async move {
            if let Ok(protocol) = self.get_protocol().await {
                protocol.get_temporary_session(&from).await
            } else {
                None
            }
        }
        .boxed_local()
    }

    fn session(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<DirectSession>>> {
        let handler = self.clone();
        async move {
            let state = handler.state.read().await;
            state.p2p_sessions.get(&from).cloned()
        }
        .boxed_local()
    }

    fn on_control(
        self,
        session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        if let Some(kind) = control.kind {
            let fut = match kind {
                ya_relay_proto::proto::control::Kind::ReverseConnection(message) => {
                    let myself = self;
                    tokio::task::spawn_local(async move {
                        myself
                            .on_reverse_connection(session_id, from, message)
                            .await
                            .map_err(|e| log::warn!("Error handling `ReverseConnection`: {e}"))
                            .ok();
                    });
                    return None;
                }
                ya_relay_proto::proto::control::Kind::PauseForwarding(_) => async move {
                    match self.find_session(from).await {
                        Some(session) => {
                            log::debug!(
                                "Forwarding paused for session {} ({from})",
                                session.raw.id
                            );
                            session.raw.pause_forwarding().await;
                        }
                        None => {
                            log::warn!("Cannot pause forwarding: session with {from} not found")
                        }
                    }
                }
                .boxed_local(),
                ya_relay_proto::proto::control::Kind::ResumeForwarding(_) => async move {
                    match self.find_session(from).await {
                        Some(session) => {
                            log::debug!("Forwarding resumed for session {}", session.raw.id);
                            session.raw.resume_forwarding().await;
                        }
                        None => {
                            log::warn!("Cannot resume forwarding: session with {from} not found")
                        }
                    }
                }
                .boxed_local(),
                ya_relay_proto::proto::control::Kind::Disconnected(
                    proto::control::Disconnected { by: Some(by) },
                ) => {
                    let myself = self;
                    async move {
                        myself
                            .on_disconnected(session_id, from, by)
                            .await
                            .map_err(|e| log::debug!("Error handling `Disconnected`: {e}"))
                            .ok();
                    }
                    .boxed_local()
                }
                _ => {
                    log::debug!("Unhandled control packet: {kind:?}");
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
        log::trace!("Received request packet from {from}: {request:?}");

        let (request_id, kind) = match request {
            proto::Request {
                request_id,
                kind: Some(kind),
            } => (request_id, kind),
            _ => return None,
        };

        if self.is_request_duplicate(&session_id, request_id) {
            return None;
        }
        self.record_duplicate(session_id.clone(), request_id);

        let fut = match kind {
            proto::request::Kind::Ping(request) => {
                async move { self.on_ping(session_id, request_id, from, request).await }
                    .boxed_local()
            }
            proto::request::Kind::Session(request) => async move {
                self.dispatch_session(session_id, request_id, from, request)
                    .await
                    .map_err(|e| log::warn!("Handling `Session` request: {e}"))
                    .ok();
            }
            .boxed_local(),
            _ => return None,
        };

        Some(fut)
    }

    fn on_forward(
        self,
        forward: Forward,
        from: SocketAddr,
        session: Option<Arc<DirectSession>>,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        // TODO: We shouldn't handle different transport types, but only care to either return
        //       correct session or create one.
        //       We need to consider if we need to create session here at all.
        let reliable = forward.is_reliable();
        let encrypted = forward.is_encrypted();
        let slot = forward.slot;
        let channel = self.ingress_channel.clone();

        let myself = self;
        let fut = async move {
            log::trace!(
                "[{}] received forward packet ({} B) via {}",
                myself.config.node_id,
                forward.payload.len(),
                from
            );

            let session = match session {
                None => {
                    // In this case we can't establish session, because we don't have
                    // neither NodeId nor SlotId.
                    log::debug!(
                        "Forward packet from unknown address: {from}. Can't resolve. Sending Disconnected message.",
                    );
                    return myself.send_disconnect(SessionId::from(forward.session_id), from).await;
                },
                Some(session) => session,
            };

            let node = if slot == FORWARD_SLOT_ID {
                // Direct message from other Node.
                session.owner.default_id.node_id.clone()
            } else {
                // Messages forwarded through relay server or other relay Node.
                match { session.forwards.read().await.slots.get(&slot).cloned() } {
                    Some(node) => node.default_id,
                    None => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {slot}) through session [{from}]. Resolving.."
                        );

                        let session = myself.server_session().await?;
                        let node = session.raw.find_slot(slot).await?;
                        let ident = Identity::try_from(&node)?;

                        log::debug!("Attempting to establish connection to Node {} (slot {})", ident.node_id, node.slot);
                        let session = myself
                            .session(ident.node_id)
                            .await.map_err(|e| anyhow!("Failed to resolve node with slot {slot}. {e}"))?;

                        session.target()
                    }
                }
            };

            // Decryption

            let packet = Forwarded {
                transport: match reliable {
                    true => TransportType::Reliable,
                    false => TransportType::Unreliable,
                },
                node_id: node,
                payload: forward.payload,
            };

            channel.tx.send(packet).map_err(|e| anyhow!("SessionLayer can't pass packet to other layers: {e}"))?;
            anyhow::Result::<()>::Ok(())
        }
        .map_err(move |e| log::debug!("Forward from {from} failed: {e}"))
        .map(|_| ());

        Some(fut.boxed_local())
    }
}

mod testing {
    use crate::_session_layer::SessionLayer;
    use crate::_session_protocol::SessionProtocol;
    use crate::testing::accessors::SessionLayerPrivate;

    use anyhow::bail;
    use futures::future::LocalBoxFuture;
    use futures::{FutureExt, StreamExt};
    use std::net::SocketAddr;

    impl SessionLayerPrivate for SessionLayer {
        fn get_protocol(&self) -> LocalBoxFuture<anyhow::Result<SessionProtocol>> {
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
}
