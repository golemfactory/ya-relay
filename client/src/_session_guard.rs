#![allow(dead_code)]
#![allow(unused)]

use anyhow::bail;
use derive_more::Display;
use educe::Educe;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};
use ya_relay_core::identity::Identity;

use crate::_direct_session::{DirectSession, NodeEntry};
use crate::_error::{SessionError, SessionResult, TransitionError};
use crate::_session::RawSession;

use ya_relay_core::session::{Endpoint, SessionId};
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

#[derive(Clone, Educe, Display, Debug)]
#[educe(PartialEq)]
pub enum SessionState {
    #[display(fmt = "Outgoing-{}", _0)]
    Outgoing(InitState),
    #[display(fmt = "Incoming-{}", _0)]
    Incoming(InitState),
    #[display(fmt = "Reverse-{}", _0)]
    ReverseConnection(InitState),
    /// Holds established session.
    #[display(fmt = "Established")]
    Established(#[educe(PartialEq(ignore))] Weak<DirectSession>),
    /// Last attempt to init session failed. Holds error.
    #[display(fmt = "FailedEstablish")]
    FailedEstablish(SessionError),
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
    /// TODO: We need to store public keys here. Using `Identity` is problematic
    ///       here, because we not always have full information, when initializing this
    ///       struct. To have full info, we need to query it from relay server. In case we got
    ///       `ReverseConnection` this could increase time to receiving first message by initiator.
    ///       On the other side if we don't have all Node identities, than we risk that we won't
    ///       protect ourselves from attempting to initialize 2 sessions at the same time.
    ///       For example we could create 2 separate SessionEntries for default and secondary identity.
    node: Option<NodeEntry<Identity>>,
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
            return target;
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
            None => Err(TransitionError::NodeNotFound(node_id)),
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

    pub async fn public_addresses(&self) -> Vec<SocketAddr> {
        let mut target = self.state.read().await;
        target.addresses.clone()
    }

    pub async fn identities(&self) -> NodeEntry<Identity> {
        todo!()
        // let mut target = self.state.read().await;
        // target.node.clone()
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
    #[allow(clippy::match_like_matches_macro)]
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
        self.transition(SessionState::Outgoing(new_state))
    }

    pub fn transition(&mut self, new_state: SessionState) -> Result<SessionState, TransitionError> {
        let allowed = match (&self, &new_state) {
            (SessionState::Incoming(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Outgoing(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::ReverseConnection(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Incoming(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::Outgoing(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::ReverseConnection(InitState::Ready), SessionState::Established(_)) => {
                true
            }
            (
                SessionState::Outgoing(InitState::WaitingForReverseConnection),
                SessionState::ReverseConnection(InitState::Initializing),
            ) => true,
            (SessionState::Closed, SessionState::Outgoing(InitState::ConnectIntent)) => true,
            (SessionState::Closed, SessionState::Incoming(InitState::ConnectIntent)) => true,
            (SessionState::Established(_), SessionState::Established(_)) => true,
            (SessionState::Established(_), SessionState::Established(_)) => true,
            (
                SessionState::FailedEstablish(_),
                SessionState::Outgoing(InitState::ConnectIntent),
            ) => true,
            (
                SessionState::FailedEstablish(_),
                SessionState::Incoming(InitState::ConnectIntent),
            ) => true,
            (SessionState::Incoming(prev), SessionState::Incoming(next)) => prev.allowed(next),
            (SessionState::Outgoing(prev), SessionState::Outgoing(next)) => prev.allowed(next),
            (SessionState::ReverseConnection(prev), SessionState::ReverseConnection(next)) => {
                prev.allowed(next)
            }
            _ => false,
        };

        if !allowed {
            Err(TransitionError::InvalidTransition(self.clone(), new_state))
        } else {
            *self = new_state;
            Ok(self.clone())
        }
    }

    #[allow(clippy::match_like_matches_macro)]
    pub fn is_finished(&self) -> bool {
        match self {
            SessionState::Established(_)
            | SessionState::Closed
            | SessionState::FailedEstablish(_) => true,
            _ => false,
        }
    }
}

impl SessionEntry {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>) -> Self {
        let (notify_msg, _) = broadcast::channel(10);

        Self {
            id: node_id,
            state: Arc::new(RwLock::new(SessionEntryState {
                node: None,
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
                result: None,
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
                result: None,
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

    pub fn awaiting_notifier(&self) -> NodeAwaiting {
        NodeAwaiting {
            guard: self.clone(),
            notifier: self.state_notifier.subscribe(),
        }
    }

    pub async fn state(&self) -> SessionState {
        self.state.read().await.state.clone()
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
/// TODO: Rename `guard` in every place in the code, since it has completely
///       different purpose now
pub struct SessionPermit {
    pub guard: SessionEntry,
    pub(crate) result: Option<Result<Arc<DirectSession>, SessionError>>,
}

impl SessionPermit {
    pub(crate) async fn async_drop(
        guard: SessionEntry,
        result: Option<Result<Arc<DirectSession>, SessionError>>,
    ) {
        let node_id = guard.id;
        let new_state = match result {
            None => SessionState::FailedEstablish(SessionError::ProgrammingError(
                "Dropping `SessionPermit` without result.".to_string(),
            )),
            Some(Ok(session)) => SessionState::Established(Arc::downgrade(&session)),
            Some(Err(e)) => SessionState::FailedEstablish(e),
        };

        // We don't expect to fail here, so if we do, something has gone really bad.
        if let Err(e) = guard.transition(new_state.clone()).await {
            log::warn!("Dropping Permit for Node {node_id}: {e}");
            guard
                .transition(SessionState::FailedEstablish(
                    SessionError::ProgrammingError(format!(
                        "Failed to transition to state: {new_state}: {e}"
                    )),
                ))
                .await;
        }
    }

    pub fn results(
        &mut self,
        result: Result<Arc<DirectSession>, SessionError>,
    ) -> Result<Arc<DirectSession>, SessionError> {
        self.result = Some(result.clone());
        result
    }
}

impl Drop for SessionPermit {
    fn drop(&mut self) {
        log::trace!("Dropping `SessionPermit` for {}.", self.guard.id,);
        tokio::task::spawn_local(SessionPermit::async_drop(
            self.guard.clone(),
            self.result.take(),
        ));
    }
}

/// Structure for awaiting established connection.
pub struct NodeAwaiting {
    pub guard: SessionEntry,
    notifier: broadcast::Receiver<SessionState>,
}

impl NodeAwaiting {
    pub async fn await_for_finish(&mut self) -> Result<Weak<DirectSession>, SessionError> {
        // If we query `NodeAwaiting` after session is already established, we won't get
        // any notification, so we must check state before entering loop.
        let mut state = self.guard.state().await;
        let node_id = self.guard.id;

        loop {
            match state {
                SessionState::Established(session) => return Ok(session),
                // TODO: We would like to return more meaningful error message.
                SessionState::Closed => {
                    return Err(SessionError::Unexpected("Connection closed.".to_string()))
                }
                SessionState::FailedEstablish(e) => return Err(e),
                _ => log::trace!("Waiting for established session with {node_id}: state: {state}"),
            };

            state = match self.notifier.recv().await {
                Ok(state) => state,
                Err(RecvError::Closed) => {
                    return Err(SessionError::Internal(
                        "Waiting for session initialization: notifier dropped.".to_string(),
                    ))
                }
                Err(RecvError::Lagged(lost)) => {
                    // TODO: Maybe we could handle lags better, by checking current state?
                    return Err(SessionError::Internal(format!("Waiting for session initialization: notifier lagged. Lost {lost} state change(s).")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lazy_static::lazy_static;

    use std::str::FromStr;
    use std::time::Duration;
    use tokio::time::timeout;

    use ya_relay_core::crypto::{Crypto, CryptoProvider, FallbackCryptoProvider};
    use ya_relay_proto::codec::PacketKind;

    lazy_static! {
        static ref CRYPTO1: FallbackCryptoProvider = FallbackCryptoProvider::default();
        static ref CRYPTO2: FallbackCryptoProvider = FallbackCryptoProvider::default();
        static ref NODE_ID1: NodeId = CRYPTO1.default_node_id();
        static ref NODE_ID2: NodeId = CRYPTO2.default_node_id();
        static ref ADDR1: SocketAddr = SocketAddr::from_str("127.0.0.1:8000").unwrap();
        static ref ADDR2: SocketAddr = SocketAddr::from_str("127.0.0.1:8001").unwrap();
        static ref CRYPTOS: HashMap<NodeId, &'static FallbackCryptoProvider> = {
            let mut map = HashMap::<NodeId, &'static FallbackCryptoProvider>::new();
            map.insert(*NODE_ID1, &CRYPTO1);
            map.insert(*NODE_ID2, &CRYPTO2);
            map
        };
    }

    async fn mock_establish_outgoing(mut permit: SessionPermit) -> anyhow::Result<()> {
        let node = permit.guard.clone();
        node.transition_outgoing(InitState::Initializing).await?;
        node.transition_outgoing(InitState::ChallengeHandshake)
            .await?;
        node.transition_outgoing(InitState::HandshakeResponse)
            .await?;
        node.transition_outgoing(InitState::ChallengeVerified)
            .await?;
        node.transition_outgoing(InitState::SessionRegistered)
            .await?;
        node.transition_outgoing(InitState::Ready).await?;

        permit.results(Ok(mock_session(&permit).await));
        drop(permit);
        Ok(())
    }

    async fn mock_establish_incoming(mut permit: SessionPermit) -> anyhow::Result<()> {
        let node = permit.guard.clone();
        node.transition_incoming(InitState::Initializing).await?;
        node.transition_incoming(InitState::ChallengeHandshake)
            .await?;
        node.transition_incoming(InitState::HandshakeResponse)
            .await?;
        node.transition_incoming(InitState::ChallengeVerified)
            .await?;
        node.transition_incoming(InitState::SessionRegistered)
            .await?;
        node.transition_incoming(InitState::Ready).await?;

        permit.results(Ok(mock_session(&permit).await));
        drop(permit);
        Ok(())
    }

    async fn mock_session(permit: &SessionPermit) -> Arc<DirectSession> {
        let node_id = permit.guard.id;
        let addr = permit.guard.public_addresses().await[0];

        let crypto_provider = *CRYPTOS.get(&node_id).unwrap();
        let crypto = crypto_provider.get(node_id).await.unwrap();
        let public_key = crypto.public_key().await.unwrap();
        let identity = Identity {
            node_id,
            public_key,
        };

        let (sink, _) = futures::channel::mpsc::channel::<(PacketKind, SocketAddr)>(1);

        let raw = RawSession::new(addr, SessionId::generate(), sink);
        DirectSession::new(permit.guard.id, vec![identity].into_iter(), raw).unwrap()
    }

    /// Checks if `Permits` and `NodeAwaiting` work independently.
    #[actix_rt::test]
    async fn test_session_guards_independent_permits() {
        let guards = GuardedSessions::default();
        let permit1 = match guards.lock_outgoing(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        let permit2 = match guards.lock_outgoing(*NODE_ID2, &[*ADDR2]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter1 = match guards.lock_outgoing(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter2 = match guards.lock_outgoing(*NODE_ID2, &[*ADDR2]).await {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        // Initialize session related to permit 1
        // Waiters should work independently from each other. Waiting one initialization
        // shouldn't affect second initialization.
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            mock_establish_outgoing(permit1).await.unwrap();
        });

        timeout(Duration::from_millis(600), waiter1.await_for_finish())
            .await
            .unwrap()
            .unwrap();

        // Second permit should still be locked
        assert!(
            timeout(Duration::from_millis(200), waiter2.await_for_finish())
                .await
                .is_err()
        );
    }

    #[actix_rt::test]
    async fn test_session_guards_incoming() {
        let guards = GuardedSessions::default();
        let permit = match guards.lock_incoming(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter = match guards.lock_incoming(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            mock_establish_incoming(permit).await.unwrap();
        });

        timeout(Duration::from_millis(600), waiter.await_for_finish())
            .await
            .unwrap()
            .unwrap();
    }

    /// Checks if permits work correctly in case of attempts to initialize
    /// both incoming and outgoing session.
    #[actix_rt::test]
    async fn test_session_guards_incoming_and_outgoing() {
        let guards = GuardedSessions::default();
        let permit = match guards.lock_incoming(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// We shouldn't get `Permit` if incoming session initialization started.
        let mut waiter = match guards.lock_outgoing(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            mock_establish_incoming(permit).await.unwrap();
        });

        timeout(Duration::from_millis(600), waiter.await_for_finish())
            .await
            .unwrap()
            .unwrap();
    }

    #[actix_rt::test]
    async fn test_session_guards_already_established() {
        let guards = GuardedSessions::default();
        let permit = match guards.lock_outgoing(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        mock_establish_outgoing(permit).await.unwrap();

        let mut waiter = match guards.lock_outgoing(*NODE_ID1, &[*ADDR1]).await {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };
        timeout(Duration::from_millis(100), waiter.await_for_finish())
            .await
            .unwrap()
            .unwrap();
    }
}
