#![allow(dead_code)]
#![allow(unused)]

use anyhow::{anyhow, bail};
use futures::future::{AbortHandle, LocalBoxFuture};
use futures::{FutureExt, SinkExt, TryFutureExt};
use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, Semaphore};

use crate::client::{ClientConfig, Forwarded};
use crate::ForwardReceiver;

use crate::_dispatch::{dispatch, Dispatcher, Handler};
use crate::_encryption::Encryption;
use crate::_error::{SessionError, SessionResult};
use crate::_routing_session::{DirectSession, NodeEntry, NodeRouting, RoutingSender};
use crate::_session::RawSession;
use crate::_session_guard::GuardedSessions;

use crate::_expire::track_sessions_expiration;
use crate::_session_protocol::SessionProtocol;
use ya_relay_core::identity::Identity;
use ya_relay_core::session::{NodeInfo, SessionId, TransportType};
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_core::NodeId;
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::{Forward, RequestId, SlotId, FORWARD_SLOT_ID};
use ya_relay_proto::{codec, proto};
use ya_relay_stack::Channel;

type ReqFingerprint = (Vec<u8>, u64);

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
            state.init_protocol = Some(SessionProtocol::new(
                self.clone(),
                self.guards.clone(),
                sink,
            ));
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

    /// Returns `RoutingSender` which can be used to send packets to desired Node.
    /// Creates session with Node if necessary. Function will choose the most optimal
    /// route to destination.
    pub async fn session(&self, node_id: NodeId) -> Result<RoutingSender, SessionError> {
        if let Some(routing) = self.get_node_routing(node_id).await {
            return Ok(routing);
        }

        // Query relay server for Node information, we need to find out default id and aliases.
        // TODO: We could avoid querying the same information multiple times in case this function
        //       is called from multiple places at the same time.
        let info = self
            .query_node_info(node_id)
            .await
            .map_err(|e| SessionError::NotFound(format!("Error querying node {node_id}: {e}")))?;

        // Acquire SessionGuard to protect from duplicated initializations.
        // If session initialization is already in progress - wait for finish.
        // If we are the only one requesting session, call initialization functions.

        unimplemented!()
    }

    pub async fn server_session(&self) -> Result<DirectSession, SessionError> {
        unimplemented!()
    }

    pub async fn close_session(&self, session: Arc<DirectSession>) -> Result<(), SessionError> {
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

    // TODO: API should be different. NodeId should be enough to resolve connection.
    async fn resolve(&self, node_id: NodeId, slot: SlotId) -> anyhow::Result<Arc<DirectSession>> {
        unimplemented!()
        // if node_id == self.config.node_id {
        //     bail!("Remote id belongs to this node.");
        // }
        //
        // log::debug!("Resolving [{node_id}], slot = {slot}");
        //
        // let addrs: Vec<SocketAddr> = self.filter_own_addresses(endpoints).await;
        //
        // let lock = self.guarded.guard_initialization(node_id, &addrs).await;
        // let _guard = lock.write().await;
        //
        // // Check whether we are already connected to a node with this id
        // if let Ok(entry) = self.get_node(node_id).await {
        //     let session_type = match entry.is_p2p() {
        //         true => "p2p",
        //         false => "relay",
        //     };
        //     log::debug!("Resolving Node [{node_id}]. Returning already existing connection (session = {} ({session_type}), slot = {}).", entry.session.id, entry.slot);
        //     return Ok(entry);
        // }
        //
        // let this = self.clone();
        // Ok(self
        //     .try_resolve(node_id, addrs.clone(), slot)
        //     .then(move |result| async move {
        //         this.guarded
        //             .stop_guarding(node_id, result.clone().map(|entry| entry.session))
        //             .await;
        //         result
        //     })
        //     .await?)
    }

    async fn try_resolve(
        &self,
        node_id: NodeId,
        addrs: Vec<SocketAddr>,
        slot: SlotId,
    ) -> SessionResult<Arc<DirectSession>> {
        unimplemented!()
        // // If requested slot == FORWARD_SLOT_ID, than we are responding to ReverseConnection,
        // // so we shouldn't try to send it ourselves.
        // let try_reverse = slot != FORWARD_SLOT_ID;
        //
        // // If node has public IP, we can establish direct session with him
        // // instead of forwarding messages through relay.
        // let (session, resolved_slot) = match self
        //     .try_direct_session(node_id, addrs, try_reverse)
        //     .await
        // {
        //     // If we send packets directly, we use FORWARD_SLOT_ID.
        //     Ok(session) => (session, FORWARD_SLOT_ID),
        //     // Fatal error, cease further communication
        //     Err(SessionError::Drop(e)) => {
        //         log::warn!("{}", e);
        //         return Err(SessionError::Drop(e));
        //     }
        //     // Recoverable error, continue
        //     Err(SessionError::Retry(e)) => {
        //         log::info!("{}", e);
        //
        //         // In this case we don't have slot id, so we can't establish relayed connection.
        //         // It happens mostly if we are trying to establish connection in response to ReverseConnection.
        //         if !try_reverse {
        //             return Err(anyhow!("Failed to establish p2p connection with [{node_id}] and relay Server won't be used, since slot = {FORWARD_SLOT_ID}.").into());
        //         }
        //
        //         (self.server_session().await?, slot)
        //     }
        // };
        //
        // if resolved_slot != FORWARD_SLOT_ID {
        //     log::info!(
        //         "Using relay Server to forward packets to [{node_id}] (slot {resolved_slot})"
        //     );
        // }
        //
        // log::debug!(
        //     "Adding node [{node_id}] session ({} - {}), resolved slot = {resolved_slot}, requested slot = {slot}",
        //     session.raw.id,
        //     session.raw.remote
        // );
        //
        // Ok(node)
    }

    pub async fn try_direct_session(
        &self,
        node_id: NodeId,
        addrs: Vec<SocketAddr>,
        try_reverse_connection: bool,
    ) -> SessionResult<Arc<DirectSession>> {
        todo!()
        // if addrs.is_empty() {
        //     log::debug!("Node [{node_id}] has no public endpoints.");
        // }
        //
        // // Try to connect to remote Node's public endpoints.
        // for addr in addrs {
        //     match self.init_p2p_session(addr, node_id).await {
        //         Err(SessionError::Retry(err)) => {
        //             log::debug!(
        //                 "Failed to establish p2p session with node [{node_id}], address: {addr}. Error: {err}"
        //             )
        //         }
        //         result => return result,
        //     }
        // }
        //
        // // We are trying to connect to Node without public IP. Trying to send
        // // ReverseConnection message, so Node will try to connect to us.
        // if try_reverse_connection && self.get_public_addr().await.is_some() {
        //     match self.try_reverse_connection(node_id).await {
        //         Err(e) => log::info!("Reverse connection with [{node_id}] failed: {e}"),
        //         Ok(session) => return Ok(session),
        //     }
        // }
        //
        // Err(anyhow!("All attempts to establish direct session with node [{node_id}] failed").into())
    }

    async fn try_reverse_connection(&self, node_id: NodeId) -> anyhow::Result<Arc<DirectSession>> {
        todo!()
        // log::info!(
        //     "Request reverse connection. me={}, remote={node_id}",
        //     self.config.node_id,
        // );
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
        // // longer timeout now, because we can have hope, that this node is responsive.
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

    pub async fn dispatch_session<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) {
        if let Some(protocol) = { self.state.read().await.init_protocol.clone() } {
            protocol
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
            log::warn!("Unable to send Pong to {from}: {e}");
        }
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
}

impl Handler for SessionLayer {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        async move {
            self.guards
                .get_temporary_session(&from)
                .await
                .map(|entity| entity.dispatcher())
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
        _session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        if let Some(kind) = control.kind {
            let fut = match kind {
                ya_relay_proto::proto::control::Kind::ReverseConnection(message) => {
                    let myself = self;
                    tokio::task::spawn_local(async move {
                        let node_id = match NodeId::try_from(&message.node_id) {
                            Ok(node_id) => node_id,
                            Err(_) => {
                                log::warn!(
                                    "ReverseConnection with invalid NodeId: {:?}",
                                    message.node_id
                                );
                                return;
                            }
                        };

                        log::info!(
                            "Got ReverseConnection message. node={node_id}, endpoints={:?}",
                            message.endpoints
                        );

                        if message.endpoints.is_empty() {
                            log::warn!("Got ReverseConnection with no endpoints to connect to.");
                            return;
                        }

                        match myself.resolve(node_id, FORWARD_SLOT_ID).await {
                            Err(e) => log::warn!(
                                "Failed to resolve ReverseConnection. node_id={node_id} error={e}"
                            ),
                            Ok(_) => log::trace!("ReverseConnection succeeded: {:?}", message),
                        };
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
                    async move {
                        log::debug!("Got Disconnected from {from}");

                        if let Ok(node) = match by {
                            By::Slot(id) => match self.find_session(from).await {
                                Some(session) => session.remove_slot(id).await,
                                None => Err(anyhow!("Session with {from} not found")),
                            },
                            By::NodeId(id) => NodeId::try_from(&id).map_err(anyhow::Error::from),
                            // We will disconnect session responsible for sending message. Doesn't matter
                            // what id was sent.
                            By::SessionId(_session) => match self.find_session(from).await {
                                Some(session) => {
                                    self.close_session(session).await.ok();
                                    return;
                                }
                                None => Err(anyhow!("Session with {from} not found")),
                            },
                        } {
                            log::info!(
                                "Node [{node}] disconnected from Relay. Stopping forwarding.."
                            );
                            self.disconnect_node(node).await.ok();
                        }
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
                session.owner.clone().into()
            } else {
                // Messages forwarded through relay server or other relay Node.
                match { session.forwards.read().await.slots.get(&slot).cloned() } {
                    Some(node) => node,
                    None => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {slot}). Resolving.."
                        );

                        let session = myself.server_session().await?;
                        let node = session.raw.find_slot(slot).await?;
                        let ident = Identity::try_from(&node)?;

                        log::debug!("Attempting to establish connection to Node {} (slot {})", ident.node_id, node.slot);
                        let session = myself
                            .resolve(ident.node_id, node.slot)
                            .await?;

                        if session.owner.default_id.node_id == ident.node_id {
                            session.owner.clone().into()
                        } else {
                            match { session.forwards.read().await.slots.get(&slot).cloned() } {
                                None => {
                                    log::debug!(
                                        "Forward packet from unknown address: {from}. Failed to resolve.",
                                    );
                                    bail!("Failed to resolve node with slot {slot} on session {from}")
                                }
                                Some(node) => node,
                            }
                        }
                    }
                }
            };

            // Decryption

            let packet = Forwarded {
                transport: match reliable {
                    true => TransportType::Reliable,
                    false => TransportType::Unreliable,
                },
                node_id: node.default_id,
                payload: Default::default(),
            };

            channel.tx.send(packet).map_err(|e| anyhow!("SessionLayer can't pass packet to other layers: {e}"))?;
            anyhow::Result::<()>::Ok(())
        }
            .map_err(|e| log::debug!("On forward failed: {e}"))
            .map(|_| ());

        tokio::task::spawn_local(fut);
        None
    }
}

mod testing {
    use crate::_session_layer::SessionLayer;
    use crate::_session_protocol::SessionProtocol;
    use crate::testing::accessors::SessionLayerPrivate;

    use anyhow::bail;
    use futures::future::LocalBoxFuture;
    use futures::FutureExt;

    impl SessionLayerPrivate for SessionLayer {
        fn get_protocol(&self) -> LocalBoxFuture<anyhow::Result<SessionProtocol>> {
            let myself = self.clone();
            async move {
                match myself.state.read().await.init_protocol.clone() {
                    None => bail!("SessionProtocol field is None"),
                    Some(protocol) => Ok(protocol),
                }
            }
            .boxed_local()
        }
    }
}
