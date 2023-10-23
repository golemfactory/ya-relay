mod expire;
mod keep_alive;
pub mod network_view;
pub mod session_initializer;
pub mod session_state;
pub mod session_traits;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use derive_more::Display;
use futures::future::{AbortHandle, LocalBoxFuture};
use futures::{FutureExt, SinkExt, TryFutureExt};
use metrics::{gauge, increment_counter};
use std::cmp::{max, min};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::iter::FromIterator;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use self::expire::track_sessions_expiration;
use self::keep_alive::keep_alive_server_session;
use self::network_view::{NetworkView, SessionLock, SessionPermit, Validity};
use self::session_state::{RelayedState, ReverseState, SessionState};
use crate::client::{ClientConfig, Forwarded};
use crate::direct_session::{DirectSession, NodeEntry};
use crate::dispatch::{dispatch, Handler};
use crate::encryption::Encryption;
use crate::error::{
    ProtocolError, ResultExt, SessionError, SessionInitError, SessionResult, TransitionError,
};
use crate::metrics::{metric_session_established, TARGET_ID};
use crate::raw_session::{RawSession, SessionType};
use crate::routing_session::{NodeRouting, RoutingSender};
use crate::session::session_initializer::SessionInitializer;
use crate::transport::ForwardReceiver;

use crate::error::SenderError::Session;
use crate::session::session_state::SessionState::{Closed, FailedEstablish};
use crate::session::session_traits::{SessionDeregistration, SessionRegistration};
use crate::SessionError::Network;
use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::{Endpoint, NodeInfo, SessionId, TransportType};
use ya_relay_core::udp_stream::{udp_bind, OutStream};
use ya_relay_core::utils::spawn_local_abortable;
use ya_relay_core::{challenge, NodeId};
use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::control::ReverseConnection;
use ya_relay_proto::proto::{is_direct_message, Forward, RequestId, SlotId};
use ya_relay_stack::Channel;

type ReqFingerprint = (Vec<u8>, u64);

/// Describes which method was used to establish connection.
/// Numbers mapping is used on Grafana metrics. 0 is reserved for no session.
#[derive(Copy, Clone, Display, PartialEq, Eq)]
pub enum ConnectionMethod {
    Direct = 1,
    Reverse = 2,
    Relay = 3,
    // Here should be NAT punching type(s) defined in the future
}

/// Responsible for establishing/receiving connections from other Nodes and providing API,
/// that hides details, how the packets are routed to desired location.
///
/// There are 3 methods `SessionLayer` can use, to establish communication with other Node:
/// - Direct p2p connection - method is used when other Node has public ports and we can connect
///   with him directly
/// - Reverse connection - used when we have public ports but other Node doesn't. In this scenario
///   we use relay server to facilitate the connection. Our Node sends `ReverseConnection` message
///   which is proxied to other Node by relay. Than other Node tries to connect to us.
/// - Relayed connection - we use relay server to forward packets to destination Node. This method
///   is used when other options are not available.
///
/// Calling [`SessionLayer::session`] establishes session and returns [`RoutingSender`] struct, which should
/// be used to send data to destination Node. [`RoutingSender`] has ability to re-establish session,
/// if it was lost in the meantime.
///
/// [`RoutingSender`] is designed with possibility to use many relay servers at the same time.
/// If session with one relay will be closed, [`RoutingSender`] can update it's routing information
/// in a transparent way, so external layers won't notice the change, when sending subsequent packets.
/// Thanks to this [`TcpLayer`] doesn't have to close Tcp connection even if underlying session is closed.
#[derive(Clone)]
pub struct SessionLayer {
    pub config: Arc<ClientConfig>,
    /// Could be just `Option<OutStream>` but then we are not able to drop all senders
    /// when we are copying `SessionLayer`, what prevents clean shutdown.
    sink: Arc<Mutex<Option<OutStream>>>,

    pub(crate) state: Arc<RwLock<SessionLayerState>>,

    pub(crate) registry: NetworkView,
    ingress_channel: Channel<Forwarded>,

    // TODO: Could be per `Session`?
    processed_requests: Arc<Mutex<VecDeque<ReqFingerprint>>>,
}

#[derive(Default)]
pub struct SessionLayerState {
    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    public_addr: Option<SocketAddr>,
    /// Equals to `None` when not listening
    pub(crate) bind_addr: Option<SocketAddr>,

    /// Maps contains mapping for default and secondary identities.
    pub nodes: HashMap<NodeId, Arc<NodeRouting>>,
    pub p2p_sessions: HashMap<SocketAddr, Arc<DirectSession>>,
    pub p2p_nodes: HashMap<NodeId, Arc<DirectSession>>,

    pub(crate) init_protocol: Option<SessionInitializer>,

    // Collection of background tasks that must be stopped on shutdown.
    pub handles: Vec<AbortHandle>,
}

#[async_trait(?Send)]
impl SessionRegistration for SessionLayer {
    /// Registers initialized session to be ready to use.
    async fn register_session(
        &self,
        addr: SocketAddr,
        id: SessionId,
        node_id: NodeId,
        identities: Vec<Identity>,
    ) -> anyhow::Result<Arc<DirectSession>> {
        log::trace!("Calling register_session {id} [{node_id}] ({addr})");

        let session = RawSession::new(addr, id, self.out_stream()?);

        // We need a check this, because relay doesn't return identities list
        // and we don't have its public key (because relay doesn't have one).
        // We should move into direction, that there is no difference between p2p session and relay.
        // It could be possible to do this in backward compatibility manner, meaning that both new and
        // old Nodes could talk with new relay, but new Nodes couldn't cooperate with old relay.
        let is_relay = identities.is_empty();
        let direct = if is_relay {
            DirectSession::new_relay(node_id, session.clone()).map_err(|e| {
                anyhow!("Registering relay session for node [{node_id}] ({addr}): {e}")
            })?
        } else {
            DirectSession::new(node_id, identities.clone().into_iter(), session.clone())
                .map_err(|e| anyhow!("Registering session for node [{node_id}]: {e}"))?
        };

        let default_id = identities
            .iter()
            .find(|ident| ident.node_id == node_id)
            .cloned()
            .ok_or(anyhow!(
                "DirectSession constructor expects default id on identities list."
            ));

        let routing = match default_id {
            Ok(default_id) => Some(NodeRouting::new(
                NodeEntry::<Identity> {
                    default_id,
                    identities,
                },
                direct.clone(),
                Encryption {
                    crypto: self.config.crypto.clone(),
                },
            )),
            Err(_) if is_relay => None,
            Err(e) => bail!(e),
        };

        {
            let mut state = self.state.write().await;
            state.p2p_sessions.insert(session.remote, direct.clone());

            for id in &direct.owner.identities {
                state.p2p_nodes.insert(*id, direct.clone());

                if let Some(routing) = routing.clone() {
                    state.nodes.insert(*id, routing);
                }
            }

            log::trace!("Saved node session {id} [{node_id}] {addr}")
        }
        Ok(direct)
    }

    async fn register_routing(&self, routing: Arc<NodeRouting>) -> anyhow::Result<()> {
        log::trace!(
            "Calling `register_routing` for Node [{}]",
            routing.node.default_id.node_id
        );

        let route = routing
            .route
            .upgrade()
            .ok_or(anyhow!("`DirectSession` was closed"))?;

        let server_id = route.owner.default_id;
        let addr = route.raw.remote;

        let mut state = self.state.write().await;
        for id in &routing.node.identities {
            let node_id = id.node_id;
            state.nodes.insert(node_id, routing.clone());

            log::debug!(
                "Registered node [{node_id}] routing through server [{server_id}] ({addr})"
            );
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl SessionDeregistration for SessionLayer {
    async fn unregister(&self, node_id: NodeId) {
        log::debug!("[unregister]: Unregistering Node [{node_id}]");

        let direct = {
            let mut state = self.state.write().await;

            let mut ids: HashSet<NodeId> = HashSet::from_iter(vec![node_id]);

            let routing = state.nodes.get(&node_id).cloned();
            let direct = state.p2p_nodes.get(&node_id).cloned();

            if let Some(routing) = &routing {
                ids.extend(routing.node.identities.iter().map(|entry| entry.node_id))
            }

            if let Some(direct) = &direct {
                log::debug!(
                    "Disconnecting [{node_id}] - removing session: {} ({})",
                    direct.raw.id,
                    direct.raw.remote
                );

                state.p2p_sessions.remove(&direct.raw.remote);

                // List of ids should be the same in `NodeRouting` and `DirectSession`
                // we are using both to make sure we removed everything.
                ids.extend(direct.owner.identities.iter())
            }

            for id in ids {
                log::debug!("Disconnecting [{node_id}] - removing entries for identity: {id}");

                state.p2p_nodes.remove(&id);
                // `NodeRouting` will be dropped here and all `RoutingSender` containing `Weak<NodeRouting>`
                // pointing to this Node will lose connection.
                if let Some(direct) = state
                    .nodes
                    .remove(&id)
                    .and_then(|routing| routing.route.upgrade())
                {
                    // In case we had relayed connection, we remove entry from session used for this.
                    direct.remove(&id).ok();
                }
            }

            if let Some(entry) = self.registry.get_entry(node_id).await {
                log::trace!("[unregister]: found entry for {node_id}, removing...",);
                self.registry.remove_node(node_id).await;
            }

            direct
        };

        if let Some(direct) = direct {
            self.unregister_session(direct).await;
        } else {
            increment_counter!("ya-relay.client.session.closed", TARGET_ID => node_id.to_string());
        }
    }

    /// Closing session but without state changes.
    /// Function is separated for 2 reasons:
    /// - To allow spawning in separate thread, so dropping future that initiated disconnection
    ///   won't result in unfinished
    /// - To use this function as part of `unregister` (which changes state itself so we can't
    ///   do this for the second time)
    async fn unregister_session(&self, session: Arc<DirectSession>) {
        log::info!(
            "Closing session {} with [{}] ({})",
            session.raw.id,
            session.owner.default_id,
            session.raw.remote
        );

        // Notifies other Node that we are closing connection. This is only graceful optimization.
        // Node should handle disconnected Nodes properly even if he won't be notified.
        session.raw.disconnect().await.ok();

        if session.owner.default_id == NodeId::default() {
            let f = session.list();
            log::trace!(
                "[close_session]: lost session with server - remove {} forwards",
                f.len()
            );
            for e in f {
                log::trace!(
                    "[close_session]: removing forward node_id {}.",
                    e.default_id
                );
                self.registry.remove_node(e.default_id).await;
            }
        }

        let forwards = session.list();
        {
            let mut state = self.state.write().await;
            for id in &session.owner.identities {
                state.p2p_nodes.remove(id);
                state.nodes.remove(id);
            }
            state.p2p_sessions.remove(&session.raw.remote);

            for id in forwards.iter().flat_map(|entry| entry.identities.iter()) {
                state.nodes.remove(id);
            }
        }

        let target_id = session.owner.default_id.to_string();
        gauge!("ya-relay.client.session.type", ConnectionMethod::no_connection(), TARGET_ID => target_id.clone());
        increment_counter!("ya-relay.client.session.closed", TARGET_ID => target_id);

        log::info!(
            "Session {} with [{}] ({}) closed",
            session.raw.id,
            session.owner.default_id,
            session.raw.remote
        );
    }

    /// Function doesn't wait for abort to finish.
    async fn abort_initializations(&self, remote: SocketAddr) -> Result<(), SessionError> {
        log::trace!("Called `abort_initializations` for {remote}");

        let entry = match self.registry.get_entry_by_addr(&remote).await {
            None => {
                return Err(SessionError::NotFound(format!(
                    "Can't find `NodeView` by addr {remote}"
                )))
            }
            Some(entry) => entry,
        };

        entry.abort_initialization().await;

        let protocol = self.get_protocol().await?;
        if let Some(session) = protocol.get_temporary_session(&remote) {
            session.disconnect().await.ok();
            protocol.cleanup_initialization(&session.id).await;
        }
        Ok(())
    }
}

impl SessionLayer {
    pub fn new(config: Arc<ClientConfig>) -> SessionLayer {
        let state = SessionLayerState::default();

        SessionLayer {
            sink: Arc::new(Mutex::new(None)),
            config,
            state: Arc::new(RwLock::new(state)),
            registry: Default::default(),
            ingress_channel: Default::default(),
            processed_requests: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub async fn spawn(&mut self) -> anyhow::Result<SocketAddr> {
        self.spawn_with_dispatcher(self.clone()).await
    }

    /// Function split from `spawn` to enable mocking and capturing packets, before they reach `SessionLayer`.
    pub(crate) async fn spawn_with_dispatcher(
        &mut self,
        handler: impl Handler + Clone + 'static,
    ) -> anyhow::Result<SocketAddr> {
        let (stream, sink, bind_addr) = udp_bind(&self.config.bind_url).await?;

        {
            *self.sink.lock().unwrap() = Some(sink.clone());
        }

        let mut handles: Vec<AbortHandle> = Vec::from([
            spawn_local_abortable(dispatch(handler, stream)),
            spawn_local_abortable(track_sessions_expiration(self.clone())),
        ]);

        if self.config.auto_connect && !self.config.auto_connect_fail_fast {
            handles.push(spawn_local_abortable(keep_alive_server_session(
                self.clone(),
            )));
        } else {
            log::debug!("Keep alive server session not started");
        };

        {
            let mut state = self.state.write().await;

            state.bind_addr.replace(bind_addr);
            for h in handles {
                state.handles.push(h);
            }

            state.init_protocol = Some(SessionInitializer::new(
                self.config.clone(),
                self.clone(),
                sink,
            ));
        }

        Ok(bind_addr)
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let (starting, abort_handles) = {
            let mut state = self.state.write().await;
            let starting = state.init_protocol.take();
            let handles = std::mem::take(&mut state.handles);
            (starting, handles)
        };

        for abort_handle in abort_handles {
            abort_handle.abort();
        }

        if let Some(mut starting) = starting {
            starting.shutdown().await;
        }

        // Close sessions simultaneously, otherwise shutdown could last too long.
        let sessions = self.sessions().await;
        futures::future::join_all(
            sessions
                .into_iter()
                .filter_map(|session| session.upgrade())
                .map(|session| self.unregister_session(session)),
        )
        .await;

        let out_stream = { self.sink.lock().unwrap().take() };
        if let Some(mut out_stream) = out_stream {
            if let Err(e) = out_stream.close().await {
                log::warn!("Error closing socket (output stream). {e}");
            }
        }

        self.registry.shutdown().await;
        Ok(())
    }

    pub async fn is_p2p(&self, node_id: NodeId) -> bool {
        match self.get_node_routing(node_id).await {
            None => false,
            Some(routing) => routing.session_type() == SessionType::P2P,
        }
    }

    pub async fn remote_id(&self, addr: &SocketAddr) -> Option<NodeId> {
        let state = self.state.read().await;
        state
            .p2p_sessions
            .get(addr)
            .map(|direct| direct.owner.default_id)
    }

    pub async fn list_connected(&self) -> Vec<NodeId> {
        let state = self.state.read().await;
        state.nodes.keys().cloned().collect()
    }

    pub async fn default_id(&self, node_id: NodeId) -> Option<NodeId> {
        self.registry.get_entry(node_id).await.map(|entry| entry.id)
    }

    pub async fn get_public_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.public_addr
    }

    pub async fn set_public_addr(&self, addr: Option<SocketAddr>) {
        self.state.write().await.public_addr = addr;
    }

    pub async fn get_local_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.bind_addr
    }

    pub fn receiver(&self) -> Option<ForwardReceiver> {
        self.ingress_channel.receiver()
    }

    /// Doesn't try to initialize session. `None` if session didn't exist.
    pub async fn get_node_routing(&self, node_id: NodeId) -> Option<RoutingSender> {
        let state = self.state.read().await;
        state
            .nodes
            .get(&node_id)
            .cloned()
            .map(|routing| RoutingSender::from_node_routing(node_id, routing, self.clone()))
    }

    pub async fn sessions(&self) -> Vec<Weak<DirectSession>> {
        let state = self.state.read().await;
        state.p2p_sessions.values().map(Arc::downgrade).collect()
    }

    pub async fn find_session(&self, addr: SocketAddr) -> Option<Arc<DirectSession>> {
        let state = self.state.read().await;
        state.p2p_sessions.get(&addr).cloned()
    }

    /// Queries information about Node from relay server.
    /// Information is cached in `NetworkView` and will be returned from there.
    /// From time to time query to relay server will be made to check if it is up to date.
    pub async fn query_node_info(&self, node_id: NodeId) -> anyhow::Result<NodeInfo> {
        if let Some(entry) = self.registry.get_entry(node_id).await {
            // TODO: Probably we should still use outdated info if we are not able to query
            //       relay server. This could make network more resilient.
            match entry.info().await {
                Validity::UpToDate(info) => return Ok(info),
                Validity::UpdateRecommended(_) => {}
            };
        }

        log::trace!("Querying Node [{node_id}] info, because it might be outdated.");

        let server_session = self
            .server_session()
            .await
            .map_err(|e| anyhow!("Failed to get relay server session: {e}"))?;
        let node = server_session
            .raw
            .find_node(node_id)
            .await
            .map_err(|e| anyhow!("Failed to find Node on relay server: {e}"))?;

        let info =
            NodeInfo::try_from(node).map_err(|e| anyhow!("Failed to convert NodeInfo: {e}"))?;

        self.registry
            .update_entry(info.clone())
            .await
            .map_err(|e| SessionError::Internal(format!("NetworkView update failed: {e}")))?;
        Ok(info)
    }

    /// Disconnects from provided Node and all secondary identities.
    /// If we had p2p session with Node, it will be closed.
    /// TODO: Function should be abort-safe
    /// TODO: `disconnect` shouldn't fail, because there is no reasonable reaction to this case.
    ///       This function must leave everything in clean state.
    pub async fn disconnect(&self, node_id: NodeId) -> Result<(), SessionError> {
        log::info!("[disconnect]: Disconnecting Node [{node_id}]");

        // Note: This function shouldn't return before changing state to `Closed` (abort-safety).
        let entry = self.registry.guard(node_id, &[]).await;
        entry.transition(SessionState::Closing).await?;
        self.unregister(node_id).await;
        entry.transition(SessionState::Closed).await?;
        Ok(())
    }

    pub async fn close_session(&self, session: Arc<DirectSession>) -> Result<(), SessionError> {
        let myself = self.clone();
        // Makes function abort-safe. Dropping this future won't stop execution
        // of closing function.
        tokio::task::spawn_local(async move {
            let entry = myself.registry.guard(session.owner.default_id, &[]).await;

            entry.transition(SessionState::Closing).await?;
            myself.unregister_session(session).await;
            entry.transition(SessionState::Closed).await?;
            Ok(())
        })
        .await
        .map_err(|e| SessionError::Unexpected(e.to_string()))?
    }

    pub(crate) async fn close_server_session(&self) -> bool {
        if let Ok(session) = self.server_session().await {
            if self.close_session(session).await.is_ok() {
                return true;
            }
        }
        false
    }

    pub async fn session(&self, node_id: NodeId) -> Result<RoutingSender, SessionError> {
        self.session_filtered_connection_methods(node_id, vec![])
            .await
    }

    /// Returns `RoutingSender` which can be used to send packets to desired Node.
    /// Creates session with Node if necessary. Function will choose the most optimal
    /// route to destination.
    pub async fn session_filtered_connection_methods(
        &self,
        node_id: NodeId,
        dont_use: Vec<ConnectionMethod>,
    ) -> Result<RoutingSender, SessionError> {
        log::trace!("[session]: Requested session with [{node_id}]");

        if let Some(routing) = self.get_node_routing(node_id).await {
            // Why we need this ugly solution? Can't we just return `RoutingSender`?
            // The problem is that we can never have full knowledge about other Node's state.
            // And we don't know what he knows about our state. It is possible, that other Node will
            // send packets earlier, because he thinks, that connection is ready.
            // To handle this, we need to have routing already registered in `SessionLayer`.
            // But at the same time, we don't know if we can start sending packets, so from our
            // perspective session is not established fully.
            // That's why we need to wait here for established state, despite having all data structures
            // in place.
            // And there is second reason: if we have many threads waiting for session, than someone who
            // will come later, will get through, but the rest of threads would wait for `Established` state.
            self.await_connected(node_id).await?;

            log::trace!("Resolving Node [{node_id}]. Returning already existing connection (route = {} ({})).", routing.route(), routing.session_type());
            return Ok(routing);
        }

        log::trace!("[session]: Node [{node_id}] not found in routing tables. Trying to establish session...");

        // Query relay server for Node information, we need to find out default id and aliases.
        let info = self
            .query_node_info(node_id)
            .await
            .map_err(|e| SessionError::NotFound(format!("Error querying node {node_id}: {e}")))?;

        if info.identities.is_empty() {
            return Err(SessionError::Unexpected(format!(
                "No default id from relay response for [{node_id}]"
            )));
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
        //       In previous implementation we were filtering, but rationale is unknown.
        let addrs: Vec<SocketAddr> = self.filter_own_addresses(&info.endpoints).await;
        let this = self.clone();

        let session = match self.registry.lock_outgoing(remote_id, &addrs, this).await {
            SessionLock::Permit(mut permit) => {
                log::trace!("Acquired `SessionPermit` to init session with [{remote_id}]");
                let myself = self.clone();

                // Caller of `SessionLayer:session` can drop function execution at any time, but
                // other threads can be waiting for initialization as well. That's why we initialize
                // session in other task and wait for finish.
                tokio::task::spawn_local(async move {
                    permit
                        .collect_results(
                            permit
                                .run_abortable(myself.resolve(remote_id, &permit, &dont_use))
                                .await,
                        )
                        .on_err(|e| log::info!("Failed init session with {remote_id}. Error: {e}"))
                })
                .await
                .map_err(|e| SessionError::Unexpected(e.to_string()))?
            }
            SessionLock::Wait(mut waiter) => waiter.await_for_finish().await,
        }
        .map_err(|e| SessionError::Generic(e.to_string()))?;

        session.raw.dispatcher.handle_error(
            proto::StatusCode::Unauthorized as i32,
            true,
            self.clone(),
            Arc::downgrade(&session),
            Self::error_handler(),
        );

        self.get_node_routing(node_id)
            .await
            .ok_or(SessionError::Internal(format!(
                "Session with [{remote_id}] closed immediately after establishing."
            )))
    }

    fn error_handler() -> fn(i32, SessionLayer, Weak<DirectSession>) -> LocalBoxFuture<'static, ()>
    {
        move |code: i32, layer: SessionLayer, session: Weak<DirectSession>| {
            async move {
                if let Some(session) = session.upgrade() {
                    layer.close_session(session).await;
                }
                log::trace!("[session-layer]: handle_error {code}");
            }
            .boxed_local()
        }
    }

    pub async fn server_session(&self) -> Result<Arc<DirectSession>, SessionError> {
        // A little bit dirty hack, that we give default NodeId (0x00) for relay server.
        // TODO: In the future relays should have regular NodeId
        let remote_id = NodeId::default();
        let addr = self.config.srv_addr;
        let this = self.clone();

        log::trace!("Requested Relay server session with [{remote_id}] ({addr}).");

        if let Some(session) = { self.state.read().await.p2p_sessions.get(&addr).cloned() } {
            log::trace!("Resolving Relay server session. Returning already existing connection ([{}] ({})).", session.owner.default_id, session.raw.remote);
            return Ok(session);
        }

        log::trace!("Relay [{remote_id}] not found. Trying to establish session...");

        let session = match self.registry.lock_outgoing(remote_id, &[addr], this).await {
            SessionLock::Permit(mut permit) => {
                let myself = self.clone();

                // Caller of `SessionLayer:session` can drop function execution at any time, but
                // other threads can be waiting for initialization as well. That's why we initialize
                // session in other task and wait for finish.
                tokio::task::spawn_local(async move {
                    permit
                        .collect_results(
                            permit
                                .run_abortable(myself.try_server_session(remote_id, addr, &permit))
                                .await,
                        )
                        .on_err(|e| {
                            log::info!(
                                "Failed init relay session with {remote_id} ({addr}). Error: {e}"
                            )
                        })
                })
                .await
                .map_err(|e| SessionError::Unexpected(e.to_string()))?
            }
            SessionLock::Wait(mut waiter) => waiter.await_for_finish().await,
        }
        .map_err(|e| SessionError::Generic(e.to_string()))?;

        session.raw.dispatcher.handle_error(
            proto::StatusCode::Unauthorized as i32,
            true,
            self.clone(),
            Arc::downgrade(&session),
            Self::error_handler(),
        );

        // TODO: Make sure this functionality is replaced in new code.
        // let fast_lane = self.virtual_tcp_fast_lane.clone();
        // session.raw.on_drop(move || {
        //     fast_lane.borrow_mut().clear();
        // });

        Ok(session)
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

            match self.try_direct_session(node_id, permit).await {
                Ok(session) => {
                    metric_session_established(node_id, ConnectionMethod::Direct);
                    return Ok(session);
                }
                Err(e) => {
                    log::warn!("Failed to establish direct p2p session with [{node_id}]. {e}");
                    permit
                        .registry
                        .transition(SessionState::RestartConnect)
                        .await?;
                }
            }
        } else {
            log::debug!("Omitting attempt to establish direct p2p connection with [{node_id}].");
        }

        if !dont_use.contains(&ConnectionMethod::Reverse) {
            log::debug!("Attempting to establish reverse p2p connection with [{node_id}].");

            match self.try_reverse_connection(node_id, permit).await {
                Ok(session) => {
                    metric_session_established(node_id, ConnectionMethod::Reverse);
                    return Ok(session);
                }
                Err(e) => {
                    log::warn!(
                        "Failed to establish reverse direct p2p session with [{node_id}]. {e}"
                    );
                    permit
                        .registry
                        .transition(SessionState::RestartConnect)
                        .await?;
                }
            }
        } else {
            log::debug!("Omitting attempt to establish reverse p2p connection with [{node_id}].");
        }

        log::debug!("All attempts to establish direct session with node [{node_id}] failed");

        // TODO: If one party has public IP, but previous resolution attempts failed, then we should
        //       consider if it would be better not to use relayed connection.
        //       If we are using relay, we don't know if other Node is reachable at all, until
        //       we establish TCP connection on higher layer. That means that on `SessionLayer` level,
        //       we are not aware if the relayed connection doesn't work.
        if !dont_use.contains(&ConnectionMethod::Relay) {
            log::info!("Attempting to use relay Server to forward packets to [{node_id}]");

            match self.try_relayed_connection(node_id, permit).await {
                Ok(session) => {
                    metric_session_established(node_id, ConnectionMethod::Relay);
                    return Ok(session);
                }
                Err(e) => log::debug!("Can't use relayed connection with [{node_id}]. {e}"),
            }
        } else {
            log::debug!("Not using relayed connection with [{node_id}].");
        }

        // TODO: We will always get this error if something fails. We need something better.
        Err(SessionError::Generic(format!(
            "All attempts to establish session with node [{node_id}] failed"
        )))
    }

    pub async fn try_server_session(
        &self,
        node_id: NodeId,
        addr: SocketAddr,
        permit: &SessionPermit,
    ) -> SessionResult<Arc<DirectSession>> {
        let protocol = self.get_protocol().await?;
        let session = match protocol.init_server_session(addr, permit).await {
            Ok(session) => session,
            Err(SessionInitError::Relay(_, e)) | Err(SessionInitError::P2P(_, e)) => return Err(e),
        };

        let endpoints = session.raw.register_endpoints(vec![]).await?;

        // If there is any (correct) endpoint on the list, that means we have public IP.
        if let Some(addr) = endpoints
            .into_iter()
            .find_map(|endpoint| endpoint.try_into().ok())
        {
            gauge!("ya-relay.client.public-address", 1.0);
            self.set_public_addr(Some(addr)).await;
        } else {
            gauge!("ya-relay.client.public-address", 0.0);
        }

        gauge!("ya-relay.client.session.type", ConnectionMethod::Direct.metric(), TARGET_ID => node_id.to_string());
        Ok(session)
    }

    pub async fn try_direct_session(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> SessionResult<Arc<DirectSession>> {
        let protocol = self.get_protocol().await?;
        let addrs = permit.registry.public_addresses().await;
        if addrs.is_empty() {
            return Err(SessionError::NotApplicable(format!(
                "Node [{node_id}] has no public endpoints."
            )));
        }

        // Try to connect to remote Node's public endpoints.
        for addr in addrs {
            match protocol.init_p2p_session(addr, permit).await {
                Ok(session) => return Ok(session),
                Err(SessionInitError::Relay(_, e)) | Err(SessionInitError::P2P(_, e)) => {
                    log::debug!(
                        "Failed to establish p2p session with node [{node_id}], using address: {addr}. Error: {e}"
                    );
                }
            }
        }

        Err(SessionError::Generic(format!(
            "All attempts to establish direct session with node [{node_id}] failed"
        )))
    }

    async fn try_reverse_connection(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionError> {
        // We are trying to connect to Node without public IP. Send `ReverseConnection` message,
        // so Node will connect to us.
        if self.get_public_addr().await.is_none() {
            return Err(SessionError::NotApplicable(
                "We don't have public endpoints.".to_string(),
            ));
        }

        log::info!(
            "Request reverse connection. me={}, remote={node_id}",
            self.config.node_id,
        );

        // Must be called before we send `ReverseConnection` otherwise we might miss event
        // notifying us about incoming connection.
        let mut awaiting = permit.registry.awaiting_notifier();
        permit
            .registry
            .transition(SessionState::ReverseConnection(ReverseState::Awaiting))
            .await?;

        let server_session = self.server_session().await?;
        server_session.raw.reverse_connection(node_id).await?;

        log::debug!("ReverseConnection requested with node [{node_id}]");

        // Waiting for any message from other Node.
        // We don't want to wait for connection finish, because we would need longer timeout for this.
        // But if we won't receive any message from other Node, we would like to exit early,
        // to try out different connection methods.
        tokio::time::timeout(
            self.config.reverse_connection_tmp_timeout,
            awaiting.await_handshake(),
        )
        .await
        .map_err(|e| {
            SessionError::Timeout(format!(
                "Timeout ({}) elapsed when waiting for `ReverseConnection` handshake. {e}",
                humantime::format_duration(self.config.reverse_connection_tmp_timeout)
            ))
        })??;

        // If we have first handshake message from other node, we can wait with
        // longer timeout now, because we can hope, that this node is responsive.
        let result = tokio::time::timeout(
            self.config.reverse_connection_real_timeout,
            awaiting.await_reverse_finish(),
        )
        .await;

        match result {
            Ok(Ok(session)) => {
                log::info!("ReverseConnection - got session with node: [{node_id}]");
                Ok(session)
            }
            Ok(Err(e)) => {
                log::info!("ReverseConnection - failed to establish session with: [{node_id}]");
                Err(e)
            }
            Err(_) => {
                log::info!(
                    "ReverseConnection - waiting for session timed out ({}). Node: [{node_id}]",
                    humantime::format_duration(self.config.reverse_connection_real_timeout)
                );
                Err(SessionError::Timeout(format!(
                    "Not able to setup ReverseConnection within timeout with node: [{node_id}]"
                )))
            }
        }
    }

    async fn try_relayed_connection(
        &self,
        node_id: NodeId,
        permit: &SessionPermit,
    ) -> Result<Arc<DirectSession>, SessionError> {
        permit
            .registry
            .transition(SessionState::Relayed(RelayedState::Initializing))
            .await?;

        // Currently only single server supported. In the future we could use multiple
        // relays or use other p2p Nodes to forward traffic.
        // We could even route traffic through many Nodes/Servers at the same time.
        let server = self
            .server_session()
            .await
            .map_err(|e| SessionError::Relay(e.to_string()))?;

        let server_id = server.owner.default_id;
        let addr = server.raw.remote;

        // Information should be already cached in registry.
        let node = permit.registry.info().await.just_get();
        let slot: SlotId = node.slot;
        let ids = permit
            .registry
            .identities()
            .await
            .map_err(|e| SessionError::Internal(e.to_string()))?;

        log::info!("Using relay server [{server_id}] ({addr}) to forward packets to [{node_id}] (slot {slot})");

        server.register(ids.clone().into(), slot);

        let routing = NodeRouting::new(
            ids.clone(),
            server.clone(),
            Encryption {
                crypto: self.config.crypto.clone(),
            },
        );

        self.register_routing(routing)
            .await
            .map_err(|e| SessionError::Unexpected(e.to_string()))?;

        permit
            .registry
            .transition(SessionState::Relayed(RelayedState::Ready))
            .await?;
        Ok(server)
    }

    pub(crate) async fn await_connected(&self, node_id: NodeId) -> Result<(), SessionError> {
        log::trace!("[await_connected]: Session with Node [{node_id}] is registered.");

        let entry = self
            .registry
            .get_entry(node_id)
            .await
            .ok_or(SessionError::Internal(format!(
                "Entry for Node [{node_id}] not found, despite it should exits."
            )))?;

        log::trace!("[await_connected]: Waiting until it will be ready..");

        entry.awaiting_notifier().await_for_finish().await?;
        Ok(())
    }

    pub async fn dispatch_session<'a>(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        request: proto::request::Session,
    ) -> Result<(), SessionError> {
        log::trace!(
            "[dispatch_session]: from {}, sessionId: {}.",
            from,
            hex::encode(&session_id)
        );

        let protocol = self.get_protocol().await?;
        let this = self.clone();

        if session_id.is_empty() {
            log::trace!(
                "[dispatch_session]: empty session id from {from}. Handling init attempt.."
            );

            // Empty `session_id` indicates attempt to initialize session.
            let remote_id = challenge::recover_default_node_id(&request)
                .map_err(|e| ProtocolError::RecoverId(e.to_string()))?;

            // TODO: Check if we don't have the session already. In such a case we should let other party
            //       establish session, but we must replace session as gracefully as possible on our side,
            //       to avoid breaking services that might use this connection (We shouldn't go through `Closed` state).
            //       When removing session information, we can't send disconnect by accident.

            return match self.registry.lock_incoming(remote_id, &[from], this).await {
                SessionLock::Permit(mut permit) => {
                    permit.collect_results(
                        permit
                            .run_abortable(protocol.new_session(request_id, from, &permit, request))
                            .await,
                    )?;
                    gauge!("ya-relay.client.session.type", ConnectionMethod::Direct.metric(), TARGET_ID => remote_id.to_string());
                    Ok(())
                }
                SessionLock::Wait(waiter) => {
                    // We didn't get lock, so probably other thread is in charge of initializing session.
                    // TODO: This could be situation, when both sides tried to initialize connection.
                    //       Maybe we should implement algorithm, which would decide, who has precedence.
                    //       It could be based on some kind of ordering according to NodeIds.
                    log::debug!("Handling Session packet: Initialization is already in progress.. State: {}", waiter.registry.state().await);
                    Ok(())
                }
            };
        }

        let session_id = SessionId::try_from(session_id.clone())
            .map_err(|e| ProtocolError::InvalidSessionId(session_id, e.to_string()))?;
        protocol
            .existing_session(session_id, request_id, from, request)
            .await
    }

    /// TODO: Don't respond to ping when we don't have session with other Node.
    ///       In this case we should probably send disconnect, so the other Node will re-establish
    ///       session.
    ///       This doesn't work with relay, which sends ping to find out if we have public IP.
    pub async fn on_ping(
        &self,
        session_id: Vec<u8>,
        request_id: RequestId,
        from: SocketAddr,
        _request: proto::request::Ping,
    ) {
        log::trace!(
            "[on_ping]: from {}, sessionId: {}.",
            from,
            hex::encode(&session_id)
        );
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
        log::trace!(
            "[on_disconnected]: from {}, sessionId: {} by {:?}.",
            from,
            hex::encode(&session_id),
            by
        );

        // SessionId should be valid, otherwise this is some unknown session
        // so we should be cautious, when processing it.
        let session_id = SessionId::try_from(session_id)?;

        if let Ok(node) = match by {
            By::Slot(id) => match self.find_session(from).await {
                // TODO: It's necessary to unregister routing as well.
                Some(session) => session.remove_by_slot(id),
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

                return match self.find_session(from).await {
                    Some(session) => {
                        if session.raw.id != session_id {
                            bail!("Unexpected Session id: {session_id}")
                        }

                        self.close_session(session).await.ok();
                        Ok(())
                    }
                    None => {
                        // If we didn't find established session, maybe we have temporary session
                        // during initialization.
                        Ok(self.abort_initializations(from).await?)
                    }
                };
            }
        } {
            log::info!("Node [{node}] disconnected from Relay. Stopping forwarding..");
            self.disconnect(node).await.ok();
        }
        Ok(())
    }

    pub async fn on_reverse_connection(
        &self,
        _session_id: Vec<u8>,
        from: SocketAddr,
        message: ReverseConnection,
    ) -> anyhow::Result<()> {
        let this = self.clone();
        let node_id = NodeId::try_from(&message.node_id).map_err(|e| {
            anyhow!(
                "ReverseConnection with invalid NodeId: {:?}. {e}",
                message.node_id
            )
        })?;

        if message.endpoints.is_empty() {
            bail!("Got ReverseConnection for Node [{node_id}] from {from} with no endpoints to connect to.")
        }

        let endpoints = self
            .filter_own_addresses(
                &message
                    .endpoints
                    .iter()
                    .cloned()
                    .filter_map(|e| e.try_into().ok())
                    .collect::<Vec<_>>(),
            )
            .await;

        log::info!(
            "Got ReverseConnection message from {from}. node={node_id}, endpoints={:?}",
            message.endpoints
        );

        let mut permit = match self.registry.lock_outgoing(node_id, &endpoints, this).await {
            SessionLock::Permit(permit) => permit,
            // In this connection is already in progress
            SessionLock::Wait(_waiter) => return Ok(()),
        };

        // Don't try to use `ReverseConnection`, when handling `ReverseConnection`.
        // We don't want `Relayed` connection as well, because other Node can use it if he wants.
        permit
            .collect_results(
                permit
                    .run_abortable(self.resolve(
                        node_id,
                        &permit,
                        &[ConnectionMethod::Reverse, ConnectionMethod::Relay],
                    ))
                    .await,
            )
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

    pub(crate) fn record_duplicate(&self, session_id: Vec<u8>, request_id: u64) {
        const REQ_DEDUPLICATE_BUF_SIZE: usize = 32;

        let mut processed_requests = self.processed_requests.lock().unwrap();
        processed_requests.push_back((session_id, request_id));
        if processed_requests.len() > REQ_DEDUPLICATE_BUF_SIZE {
            processed_requests.pop_front();
        }
    }

    pub(crate) fn is_request_duplicate(&self, session_id: &Vec<u8>, request_id: u64) -> bool {
        self.processed_requests
            .lock()
            .unwrap()
            .iter()
            .any(|(sess_id, req_id)| *req_id == request_id && sess_id == session_id)
    }

    pub(crate) async fn get_protocol(&self) -> Result<SessionInitializer, SessionError> {
        match self.state.read().await.init_protocol.clone() {
            None => Err(SessionError::Internal(
                "`SessionProtocol` empty (not initialized?)".to_string(),
            )),
            Some(protocol) => Ok(protocol),
        }
    }

    async fn send_disconnect(&self, session_id: SessionId, addr: SocketAddr) -> anyhow::Result<()> {
        // Don't use temporary session, because we don't want to initialize session
        // with this address, nor receive the response.
        let session = RawSession::new(addr, session_id, self.out_stream()?);
        session.disconnect().await
    }

    pub fn out_stream(&self) -> anyhow::Result<OutStream> {
        self.sink
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("Network sink not initialized"))
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
}

impl Handler for SessionLayer {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<RawSession>>> {
        async move {
            if let Ok(protocol) = self.get_protocol().await {
                protocol.get_temporary_session(&from)
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
                            session.pause_forwarding().await;
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
                            session.resume_forwarding().await;
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
                            .map_err(|e| log::debug!("Handling `Disconnected`: {e}"))
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
        let (request_id, kind) = match request {
            proto::Request {
                request_id,
                kind: Some(kind),
            } => (request_id, kind),
            proto::Request { request_id, .. } => {
                log::trace!("Empty request packet ({request_id}) from {from}");
                return None;
            }
        };

        if self.is_request_duplicate(&session_id, request_id) {
            log::trace!("Dropping duplicated request packet ({request_id}) from {from}");
            return None;
        }
        self.record_duplicate(session_id.clone(), request_id);

        log::trace!("Received request packet ({request_id}) from {from}: {kind}");

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
        let reliable = forward.is_reliable();
        let _encrypted = forward.is_encrypted();
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

            let sender = if is_direct_message(slot) {
                session.owner.default_id
            } else {
                // Messages forwarded through relay server or other relay Node.
                match { session.get_by_slot(slot) } {
                    Some(node) => node.default_id,
                    None => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {slot}) through session [{from}]. Resolving.."
                        );

                        let session = myself.server_session().await?;
                        let node = session.raw.find_slot(slot).await?;
                        let ident = Identity::try_from(&node)?;

                        // TODO: Consider just adding node to `DirectSession` forwards list. If the other Node couldn't
                        //       establish p2p session with us, we won't be able to do this anyway.
                        log::debug!("Attempting to establish connection to Node {} (slot {})", ident.node_id, node.slot);

                        let session = myself
                            .session_filtered_connection_methods(ident.node_id, vec![ConnectionMethod::Reverse, ConnectionMethod::Direct])
                            .await.map_err(|e| anyhow!("Failed to resolve node with slot {slot}. {e}"))?;

                        session.target()
                    }
                }
            };

            // Decryption

            let size = forward.encoded_len();
            let transport = match reliable {
                true => TransportType::Reliable,
                false => TransportType::Unreliable,
            };
            let packet = Forwarded {
                transport,
                node_id: sender,
                payload: forward.payload,
            };

            channel.tx.send(packet).map_err(|e| anyhow!("SessionLayer can't pass packet to other layers: {e}"))?;

            session.record_incoming(sender, transport, size);
            anyhow::Result::<()>::Ok(())
        }
        .map_err(move |e| log::debug!("Forward from {from} failed: {e}"))
        .map(|_| ());

        Some(fut.boxed_local())
    }
}

impl ConnectionMethod {
    pub fn metric(&self) -> f64 {
        (*self as u16) as f64
    }

    pub fn no_connection() -> f64 {
        0.0
    }
}
