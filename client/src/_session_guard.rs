use anyhow::bail;
use derive_more::Display;
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
    by_node_id: HashMap<NodeId, SessionTarget>,
    by_addr: HashMap<SocketAddr, SessionTarget>,

    /// Temporary sessions stored during initialization period.
    /// After session is established, new struct in SessionManager is created
    /// and this one is removed.
    tmp_sessions: HashMap<SocketAddr, Arc<RawSession>>,
}

#[derive(Clone, PartialEq, Display, Debug)]
pub enum SessionState {
    Outgoing(InitState),
    Incoming(InitState),
    ReverseConnection(InitState),
    Established,
    Closed,
}

#[derive(Clone, PartialEq, Display, Debug)]
pub enum InitState {
    Initializing,
    WaitingForReverseConnection,
    ChallengeReceived,
    ChallengeSolved,
    ChallengeResponseSent,
    ChallengeVerified,
    SessionRegistered,
}

#[derive(Clone)]
struct SessionTarget {
    pub id: NodeId,
    pub state: Arc<RwLock<SessionTargetState>>,
    pub lock: Arc<RwLock<()>>,

    pub state_notifier: Arc<broadcast::Sender<SessionState>>,
}

struct SessionTargetState {
    node: NodeEntry<NodeId>,
    addresses: Vec<SocketAddr>,
    state: SessionState,
}

impl GuardedSessions {
    pub async fn guard_initialization(
        &self,
        node_id: NodeId,
        addrs: &[SocketAddr],
    ) -> Arc<RwLock<()>> {
        todo!()
        // let mut state = self.state.write().await;
        // if let Some(target) = state.find(node_id, addrs) {
        //     return target.lock.clone();
        // }
        //
        // let target = SessionTarget::new(node_id, addrs.to_vec());
        //
        // state.add(target.clone());
        // target.lock.clone()
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

    /// Creates temporary session used only during initialization.
    /// Only one session will be created for one target Node address.
    /// If temporary session was already created, this function will wait for initialization finish.
    pub async fn temporary_session(&self, addr: SocketAddr, sink: OutStream) -> Arc<RawSession> {
        let mut state = self.state.write().await;
        match state.tmp_sessions.get(&addr) {
            None => {
                let session = RawSession::new(addr, SessionId::generate(), sink);
                state.tmp_sessions.insert(addr, session.clone());
                session
            }
            Some(session) => session.clone(),
        }
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

    pub async fn notify_first_message(&self, node_id: NodeId) {
        todo!()
        // let sender = match { self.state.read().await.by_node_id.get(&node_id) } {
        //     None => return,
        //     Some(target) => target.notify_msg.clone(),
        // };
        //
        // sender.send(()).ok();
    }

    /// Differs from `temporary_session`, that it doesn't create session, if it doesn't exist.
    pub async fn get_temporary_session(&self, addr: &SocketAddr) -> Option<Arc<RawSession>> {
        self.state.read().await.tmp_sessions.get(addr).cloned()
    }

    pub async fn transition(
        &self,
        node_id: NodeId,
        new_state: SessionState,
    ) -> Result<(), TransitionError> {
        self.session_target(node_id)
            .await?
            .transition(new_state)
            .await
    }

    pub async fn transition_incoming(
        &self,
        node_id: NodeId,
        new_state: InitState,
    ) -> Result<(), TransitionError> {
        self.session_target(node_id)
            .await?
            .transition_incoming(new_state)
            .await
    }

    pub async fn transition_outgoing(
        &self,
        node_id: NodeId,
        new_state: InitState,
    ) -> Result<(), TransitionError> {
        self.session_target(node_id)
            .await?
            .transition_outgoing(new_state)
            .await
    }

    // TODO: Use other error type than `TransitionError`
    async fn session_target(&self, node_id: NodeId) -> Result<SessionTarget, TransitionError> {
        match { self.state.read().await.by_node_id.get(&node_id) } {
            None => return Err(TransitionError::NodeNotFound(node_id)),
            Some(target) => Ok(target.clone()),
        }
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.write().await;
        state.tmp_sessions.clear();
    }
}

impl GuardedSessionsState {
    fn find(&self, node_id: NodeId, addrs: &[SocketAddr]) -> Option<SessionTarget> {
        self.by_node_id
            .get(&node_id)
            .or_else(|| addrs.iter().filter_map(|a| self.by_addr.get(a)).next())
            .cloned()
    }

    fn find_by_id(&self, node_id: NodeId) -> Option<SessionTarget> {
        self.by_node_id.get(&node_id).cloned()
    }

    // fn add(&mut self, target: SessionTarget) {
    //     for addr in target.addresses.iter() {
    //         self.by_addr.insert(*addr, target.clone());
    //     }
    //     self.by_node_id.insert(target.node_id, target);
    // }

    // fn remove(&mut self, target: &SessionTarget) {
    //     for addr in target.addresses.iter() {
    //         self.by_addr.remove(addr);
    //         self.tmp_sessions.remove(addr);
    //     }
    //     self.by_node_id.remove(&target.node_id);
    // }
}

impl SessionTarget {
    pub async fn transition(&self, new_state: SessionState) -> Result<(), TransitionError> {
        {
            let mut target = self.state.write().await;
            target.state.transition(new_state.clone())?;
        }

        Ok(self.notify_change(new_state))
    }

    pub async fn transition_incoming(&self, new_state: InitState) -> Result<(), TransitionError> {
        let new_state = {
            let mut target = self.state.write().await;
            target.state.transition_incoming(new_state.clone())?
        };

        Ok(self.notify_change(new_state))
    }

    pub async fn transition_outgoing(&self, new_state: InitState) -> Result<(), TransitionError> {
        let new_state = {
            let mut target = self.state.write().await;
            target.state.transition_outgoing(new_state.clone())?
        };

        Ok(self.notify_change(new_state))
    }

    fn notify_change(&self, new_state: SessionState) {
        self.state_notifier
            .send(new_state)
            .map_err(|e| log::warn!("Failed to send state change for {}", self.id))
            .ok();
    }
}

impl InitState {
    pub fn allowed(&self, new_state: &InitState) -> bool {
        match (self, &new_state) {
            (InitState::Initializing, InitState::ChallengeReceived) => true,
            (InitState::ChallengeReceived, InitState::ChallengeSolved) => true,
            (InitState::ChallengeSolved, InitState::ChallengeResponseSent) => true,
            (InitState::ChallengeResponseSent, InitState::ChallengeVerified) => true,
            (InitState::ChallengeVerified, InitState::SessionRegistered) => true,
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
            (SessionState::Closed, SessionState::Outgoing(_)) => true,
            (SessionState::Closed, SessionState::Incoming(_)) => true,
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
}

impl SessionTarget {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>) -> Self {
        let (notify_msg, _) = broadcast::channel(1);

        Self {
            id: node_id,
            state: Arc::new(RwLock::new(SessionTargetState {
                node: NodeEntry {
                    default_id: node_id,
                    identities: vec![],
                },
                addresses,
                state: SessionState::Closed,
            })),
            lock: Default::default(),
            state_notifier: Arc::new(notify_msg),
        }
    }
}

pub struct NodeAwaiting {
    /// Notifies, when establishing session is finished either with success or with failure.
    notify_finish: broadcast::Receiver<SessionResult<Arc<DirectSession>>>,
    /// Notifies first connection message received from other party.
    /// Check `try_reverse_connection`, why we need this.
    notify_msg: broadcast::Receiver<()>,
}

impl NodeAwaiting {
    pub async fn wait_for_first_message(&mut self) {
        self.notify_msg.recv().await.ok();
    }

    pub async fn wait_for_connection(&mut self) -> SessionResult<Arc<DirectSession>> {
        self.notify_finish.recv().await.map_err(|_| {
            SessionError::Internal("Channel error while waiting for connection".to_string())
        })?
    }
}
