#![allow(dead_code)]
#![allow(unused)]

use anyhow::bail;
use derive_more::Display;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::_error::{SessionError, SessionResult, TransitionError};
use crate::_routing_session::{DirectSession, NodeEntry};
use crate::_session::RawSession;

use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;

#[derive(Clone, Default)]
pub struct GuardedSessions {
    state: Arc<RwLock<GuardedSessionsState>>,
}

#[derive(Default)]
struct GuardedSessionsState {
    by_node_id: HashMap<NodeId, SessionEntry>,
    by_addr: HashMap<SocketAddr, SessionEntry>,
}

#[derive(Clone, PartialEq, Display, Debug)]
pub enum SessionState {
    #[display(fmt = "Outgoing-{}", _0)]
    Outgoing(InitState),
    #[display(fmt = "Incoming-{}", _0)]
    Incoming(InitState),
    #[display(fmt = "Reverse-{}", _0)]
    ReverseConnection(InitState),
    Established,
    Closed,
}

#[derive(Clone, PartialEq, Display, Debug)]
pub enum InitState {
    /// This state indicates that someone plans to initialize connection,
    /// so we are not allowed to do this. Instead we should wait until
    /// connection will be ready.
    ConnectIntent,
    /// State set on the beginning of initialization function.
    Initializing,
    WaitingForReverseConnection,
    /// First round of `Session` requests, challenges sent.
    ChallengeHandshake,
    /// Second round of handshake, challenge response received.
    HandshakeResponse,
    /// Challenge from other party is valid.
    ChallengeVerified,
    /// Session is registered in `SessionLayer`.
    /// We should be ready to receive packets, but shouldn't send packets yet,
    /// because we are waiting for `ResumeForwarding` packet.
    SessionRegistered,
    /// We received (or sent in case of initiator) `ResumeForwarding` packet,
    /// so Protocol initialization part is finished.
    /// From this state session can transition to established.
    Ready,
}

#[derive(Clone)]
pub struct SessionEntry {
    /// Node default id. Duplicates information in state, but can be accessed
    /// without acquiring and awaiting RwLock.  
    pub id: NodeId,
    pub state: Arc<RwLock<SessionEntryState>>,
    pub state_notifier: Arc<broadcast::Sender<SessionState>>,
}

pub struct SessionEntryState {
    node: NodeEntry<NodeId>,
    addresses: Vec<SocketAddr>,
    state: SessionState,
}

impl GuardedSessions {
    /// Returns `SessionGuard` for other Node. Operation is atomic,
    /// you should get the same object in all places in the code.
    /// `SessionGuard` should be stored even if connection was closed,
    /// to avoid having multiple objects pointing to the same Node.
    pub async fn guard_initialization(
        &self,
        node_id: NodeId,
        addrs: &[SocketAddr],
    ) -> SessionEntry {
        let mut state = self.state.write().await;
        if let Some(target) = state.find(node_id, addrs) {
            return target.clone();
        }

        let target = SessionEntry::new(node_id, addrs.to_vec());

        state.by_node_id.insert(target.id, target.clone());
        for addr in addrs.iter() {
            state.by_addr.insert(*addr, target.clone());
        }

        target
    }

    pub async fn lock_outgoing(&self, node_id: NodeId, addrs: &[SocketAddr]) -> SessionLock {
        self.guard_initialization(node_id, addrs)
            .await
            .lock_outgoing()
            .await
    }

    pub async fn lock_incoming(&self, node_id: NodeId, addrs: &[SocketAddr]) -> SessionLock {
        self.guard_initialization(node_id, addrs)
            .await
            .lock_incoming()
            .await
    }

    pub async fn stop_guarding(&self, node_id: NodeId, result: SessionResult<Arc<DirectSession>>) {
        todo!()
        // log::debug!("Stop guarding session initialization with: [{node_id}]");
        //
        // let mut state = self.state.write().await;
        // if let Some(target) = state.find_by_id(node_id) {
        //     state.remove(&target);
        //     drop(state);
        //
        //     log::debug!("Sending session init finish notification for Node: {node_id}");
        //     target.notify_finish.send(result).ok();
        // }
    }

    pub async fn register_waiting_for_node(&self, node_id: NodeId) -> anyhow::Result<NodeAwaiting> {
        todo!()
        // let state = self.state.read().await;
        // if let Some(target) = state.by_node_id.get(&node_id) {
        //     target.wait_for_connection.store(true, Ordering::SeqCst);
        //     Ok(NodeAwaiting {
        //         notify_finish: target.notify_finish.subscribe(),
        //         notify_msg: target.notify_msg.subscribe(),
        //     })
        // } else {
        //     // If we are waiting for node to connect, we should already have entry
        //     // initialized by function `guard_initialization`. So this is programming error.
        //     bail!("Programming error. Waiting for node [{node_id}] to connect, without calling `guard_initialization` earlier.")
        // }
    }

    pub async fn try_access_unguarded(&self, node_id: NodeId) -> bool {
        todo!()
        // let state = self.state.read().await;
        // if let Some(target) = state.by_node_id.get(&node_id) {
        //     target.wait_for_connection.swap(false, Ordering::SeqCst)
        // } else {
        //     false
        // }
    }

    pub async fn transition(
        &self,
        node_id: NodeId,
        new_state: SessionState,
    ) -> Result<SessionState, TransitionError> {
        self.session_target(node_id)
            .await?
            .transition(new_state)
            .await
    }

    pub async fn transition_incoming(
        &self,
        node_id: NodeId,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        self.session_target(node_id)
            .await?
            .transition_incoming(new_state)
            .await
    }

    pub async fn transition_outgoing(
        &self,
        node_id: NodeId,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        self.session_target(node_id)
            .await?
            .transition_outgoing(new_state)
            .await
    }

    // TODO: Use other error type than `TransitionError`
    async fn session_target(&self, node_id: NodeId) -> Result<SessionEntry, TransitionError> {
        match { self.state.read().await.by_node_id.get(&node_id) } {
            None => return Err(TransitionError::NodeNotFound(node_id)),
            Some(target) => Ok(target.clone()),
        }
    }

    pub async fn shutdown(&self) {}
}

impl GuardedSessionsState {
    fn find(&self, node_id: NodeId, addrs: &[SocketAddr]) -> Option<SessionEntry> {
        self.by_node_id
            .get(&node_id)
            .or_else(|| addrs.iter().filter_map(|a| self.by_addr.get(a)).next())
            .cloned()
    }

    fn find_by_id(&self, node_id: NodeId) -> Option<SessionEntry> {
        self.by_node_id.get(&node_id).cloned()
    }

    // fn remove(&mut self, target: &SessionTarget) {
    //     for addr in target.addresses.iter() {
    //         self.by_addr.remove(addr);
    //     }
    //     self.by_node_id.remove(&target.node_id);
    // }
}

impl SessionEntry {
    pub async fn transition(
        &self,
        new_state: SessionState,
    ) -> Result<SessionState, TransitionError> {
        {
            let mut target = self.state.write().await;
            target.state.transition(new_state.clone())?;
        }

        self.notify_change(new_state.clone());
        Ok(new_state)
    }

    pub async fn transition_incoming(
        &self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        let new_state = {
            let mut target = self.state.write().await;
            target.state.transition_incoming(new_state.clone())?
        };

        self.notify_change(new_state.clone());
        Ok(new_state)
    }

    pub async fn transition_outgoing(
        &self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        let new_state = {
            let mut target = self.state.write().await;
            target.state.transition_outgoing(new_state.clone())?
        };

        self.notify_change(new_state.clone());
        Ok(new_state)
    }

    fn notify_change(&self, new_state: SessionState) {
        // TODO: Consider changing to trace
        log::debug!(
            "State changed to {} for session with [{}]",
            new_state,
            self.id
        );

        self.state_notifier
            .send(new_state)
            .map_err(|_| log::trace!("Notifying state change for {}: No listeners", self.id))
            .ok();
    }
}

impl InitState {
    pub fn allowed(&self, new_state: &InitState) -> bool {
        match (self, &new_state) {
            (InitState::ConnectIntent, InitState::Initializing) => true,
            (InitState::Initializing, InitState::ChallengeHandshake) => true,
            (InitState::ChallengeHandshake, InitState::HandshakeResponse) => true,
            (InitState::HandshakeResponse, InitState::ChallengeVerified) => true,
            (InitState::ChallengeVerified, InitState::SessionRegistered) => true,
            (InitState::SessionRegistered, InitState::Ready) => true,
            _ => false,
        }
    }
}

impl SessionState {
    pub fn transition_incoming(
        &mut self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        // If we are initializing incoming session, we don't know if it is due to ReverseConnection
        // message or other Node just started connection without reason.
        // That's why we need to translate `InitState` depending on the context.
        let new_state = match self {
            SessionState::Incoming(_) => SessionState::Incoming(new_state),
            SessionState::ReverseConnection(_) => SessionState::ReverseConnection(new_state),
            _ => SessionState::Incoming(new_state),
        };
        self.transition(new_state)
    }

    pub fn transition_outgoing(
        &mut self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        let new_state = match self {
            _ => SessionState::Outgoing(new_state),
        };
        self.transition(new_state)
    }

    pub fn transition(&mut self, new_state: SessionState) -> Result<SessionState, TransitionError> {
        let allowed = match (&self, &new_state) {
            (SessionState::Incoming(InitState::SessionRegistered), SessionState::Established) => {
                true
            }
            (SessionState::Outgoing(InitState::SessionRegistered), SessionState::Established) => {
                true
            }
            (
                SessionState::ReverseConnection(InitState::SessionRegistered),
                SessionState::Established,
            ) => true,
            (
                SessionState::Outgoing(InitState::WaitingForReverseConnection),
                SessionState::ReverseConnection(InitState::Initializing),
            ) => true,
            (SessionState::Closed, SessionState::Outgoing(InitState::ConnectIntent)) => true,
            (SessionState::Closed, SessionState::Incoming(InitState::ConnectIntent)) => true,
            (SessionState::Incoming(prev), SessionState::Incoming(next)) => prev.allowed(&next),
            (SessionState::Outgoing(prev), SessionState::Outgoing(next)) => prev.allowed(&next),
            (SessionState::ReverseConnection(prev), SessionState::ReverseConnection(next)) => {
                prev.allowed(&next)
            }
            _ => false,
        };

        if !allowed {
            return Err(TransitionError::InvalidTransition(
                self.clone(),
                new_state.clone(),
            ));
        } else {
            *self = new_state;
            Ok(self.clone())
        }
    }

    pub fn is_finished(&self) -> bool {
        match self {
            SessionState::Established => true,
            SessionState::Closed => true,
            _ => false,
        }
    }
}

impl SessionEntry {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>) -> Self {
        let (notify_msg, _) = broadcast::channel(1);

        Self {
            id: node_id,
            state: Arc::new(RwLock::new(SessionEntryState {
                node: NodeEntry {
                    default_id: node_id,
                    identities: vec![],
                },
                addresses,
                state: SessionState::Closed,
            })),
            state_notifier: Arc::new(notify_msg),
        }
    }

    /// Make intent, that you want initialize session with this Node.
    /// Function either will return Lock granting you exclusive right to
    /// initialize this session or object for awaiting on session.
    pub async fn lock_outgoing(&self) -> SessionLock {
        // We need to subscribe to state change events, before we will start transition,
        // otherwise a few events could be lost.
        let notifier = self.awaiting_notifier();
        match self.transition_outgoing(InitState::ConnectIntent).await {
            Ok(_) => SessionLock::Permit(SessionPermit {
                guard: self.clone(),
            }),
            Err(e) => {
                if let TransitionError::InvalidTransition(prev, _) = e {
                    log::debug!(
                        "Initialization of session with: [{}] in progress, state: {}. Thread will wait for finish.",
                        self.id, prev
                    )
                };

                SessionLock::Wait(notifier)
            }
        }
    }

    /// See: `SessionGuard::lock_outgoing`
    /// TODO: Unify implementation with `lock_outgoing`.
    pub async fn lock_incoming(&self) -> SessionLock {
        // We need to subscribe to state change events, before we will start transition,
        // otherwise a few events could be lost.
        let notifier = self.awaiting_notifier();
        match self.transition_incoming(InitState::ConnectIntent).await {
            Ok(_) => SessionLock::Permit(SessionPermit {
                guard: self.clone(),
            }),
            Err(e) => {
                if let TransitionError::InvalidTransition(prev, _) = e {
                    log::debug!(
                        "Initialization of session with: [{}] in progress, state: {}. Thread will wait for finish.",
                        self.id, prev
                    )
                };

                SessionLock::Wait(notifier)
            }
        }
    }

    fn awaiting_notifier(&self) -> NodeAwaiting {
        NodeAwaiting {
            guard: self.clone(),
            notifier: self.state_notifier.subscribe(),
        }
    }
}

/// Structure giving you exclusive right to initialize session (`Permit`)
/// or allows to wait for results (`Wait`).
pub enum SessionLock {
    Permit(SessionPermit),
    Wait(NodeAwaiting),
}

/// Structure giving you exclusive right to initialize session.
/// TODO: Struct should ensure clear Session state on drop.
pub struct SessionPermit {
    pub guard: SessionEntry,
}

/// Structure for awaiting established connection.
pub struct NodeAwaiting {
    pub guard: SessionEntry,
    notifier: broadcast::Receiver<SessionState>,
}

impl NodeAwaiting {
    async fn await_for_finish(&mut self) -> anyhow::Result<()> {
        while let Ok(state) = self.notifier.recv().await {
            match state {
                SessionState::Established => return Ok(()),
                // TODO: We would like to return more meaningful error message.
                SessionState::Closed => bail!("Initialization failed."),
                _ => (),
            };
        }
        bail!("Waiting for session initialization: notifier dropped.")
    }
}
