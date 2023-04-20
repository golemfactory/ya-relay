use anyhow::anyhow;
use futures::future::{AbortHandle, LocalBoxFuture};
use futures::{FutureExt, TryFutureExt};
use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{RwLock, Semaphore};

use crate::_dispatch::{Dispatcher, Handler};
use crate::_error::SessionError;
use crate::_routing_session::{DirectSession, NodeEntry, NodeRouting, Routing};
use crate::_session_guard::GuardedSessions;
use crate::client::{ClientConfig, ForwardSender, Forwarded};

use crate::_encryption::Encryption;
use crate::_session::SystemSession;
use ya_relay_core::identity::Identity;
use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::control::disconnected::By;
use ya_relay_proto::proto::{Forward, SlotId, FORWARD_SLOT_ID};
use ya_relay_proto::{codec, proto};
use ya_relay_stack::Channel;

type ReqFingerprint = (Vec<u8>, u64);

/// Responsible for establishing/receiving connections from other Nodes.
/// Hides from upper layers the decisions, how to route packets to desired location
pub struct SessionLayer {
    pub config: Arc<ClientConfig>,
    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    public_addr: Option<SocketAddr>,
    /// Equals to `None` when not listening
    bind_addr: Option<SocketAddr>,
    sink: Option<OutStream>,

    state: Arc<RwLock<SessionLayerState>>,

    guards: GuardedSessions,
    ingress_channel: Channel<Forwarded>,

    // TODO: Could be per `Session`.
    processed_requests: Arc<Mutex<VecDeque<ReqFingerprint>>>,
    simultaneous_challenges: Arc<Semaphore>,
}

pub struct SessionLayerState {
    pub nodes: HashMap<NodeId, Arc<NodeRouting>>,
    pub p2p_sessions: HashMap<SocketAddr, Arc<DirectSession>>,

    // Collection of background tasks that must be stopped on shutdown.
    pub handles: Vec<AbortHandle>,
}

impl SessionLayer {
    /// Returns `NodeRouting` which can be used to send packets to desired Node.
    /// Creates session with Node if necessary. Function will choose the most optimal
    /// route to destination.
    pub async fn session(&self, node_id: NodeId) -> Result<Weak<NodeRouting>, SessionError> {
        unimplemented!()
    }

    pub async fn server_session(&self) -> Result<Weak<NodeRouting>, SessionError> {
        unimplemented!()
    }

    pub async fn close_session(
        &self,
        session: Arc<NodeRouting>,
    ) -> Result<RoutingSession, SessionError> {
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

        let session = SystemSession::new(addr, id, self.out_stream()?);
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

    async fn send_disconnect(&self, session_id: SessionId, addr: SocketAddr) -> anyhow::Result<()> {
        // Don't use temporary session, because we don't want to initialize session
        // with this address, nor receive the response.
        let session = SystemSession::new(addr, session_id, self.out_stream()?);
        session.disconnect().await
    }
}

impl Handler for SessionLayer {
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
                    .guards
                    .get_temporary_session(&from)
                    .await
                    .map(|entity| entity.dispatcher()),
            }
        }
        .boxed_local()
    }

    fn session(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<DirectSession>>> {
        let handler = self.clone();
        async move {
            let session = {
                let state = handler.state.read().await;
                state.p2p_sessions.get(&from).cloned()
            };

            // We get either dispatcher for already existing Session from self,
            // or temporary session, that is during creation.
            match session {
                Some(session) => Some(session),
                None => self
                    .guards
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
                            log::warn!("Cannot pause forwarding: session with {from} not found")
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
                            By::Slot(id) => self.registry.get_node_by_slot(id).await,
                            By::NodeId(id) if NodeId::try_from(&id).is_ok() => {
                                self.get_node(NodeId::try_from(&id).unwrap()).await
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
        let slot = forward.slot;

        // TODO: fast lane should be handled in virtual_tcp
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
                session.owner.clone()
            } else {
                // Messages forwarded through relay server or other relay Node.
                match { session.forwards.read().await.slots.get(&slot).cloned() }  {
                    Some(node) => node,
                    None => {
                        log::debug!(
                            "Forwarding from unknown Node (slot {slot}). Resolving.."
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
            .map_err(|e| log::debug!("On forward failed: {e}"))
            .map(|_| ());

        tokio::task::spawn_local(fut);
        None
    }
}
