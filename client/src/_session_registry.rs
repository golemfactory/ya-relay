#![allow(dead_code)]
#![allow(unused)]

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use derive_more::Display;
use educe::Educe;
use futures::future::{AbortHandle, Abortable, LocalBoxFuture};
use futures::FutureExt;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};

use crate::_direct_session::{DirectSession, NodeEntry};
use crate::_error::{SessionError, SessionResult, TransitionError};
use crate::_raw_session::RawSession;

use crate::_session_traits::SessionDeregistration;
use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::{Endpoint, LastSeen, NodeInfo, SessionId};
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::{SlotId, FORWARD_SLOT_ID};

#[derive(Clone)]
pub struct RegistryConfig {
    pub node_info_ttl: chrono::Duration,
}

#[derive(Clone, Default)]
pub struct Registry {
    config: Arc<RegistryConfig>,
    state: Arc<RwLock<RegistryState>>,
}

/// TODO: We never remove entries from State. In general we should keep entries
///       as long as possible, because removal could break uniqueness of RegistryEntry
///       in case other threads will attempt to initialize session at this exact moment.
#[derive(Default)]
struct RegistryState {
    by_node_id: HashMap<NodeId, RegistryEntry>,
    by_addr: HashMap<SocketAddr, RegistryEntry>,
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
    #[display(fmt = "Relayed-{}", _0)]
    Relayed(RelayedState),
    /// Holds established session.
    #[display(fmt = "Established")]
    Established(#[educe(PartialEq(ignore))] Weak<DirectSession>),
    /// Last attempt to init session failed. Holds error.
    #[display(fmt = "FailedEstablish")]
    FailedEstablish(SessionError),
    Closing,
    /// Session was closed gracefully. (Still the reason
    /// for closing could be some kind of failure)
    Closed,
}

#[derive(Clone, PartialEq, Display, Debug)]
pub enum RelayedState {
    Initializing,
    Ready,
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

impl Registry {
    pub fn new(config: Arc<RegistryConfig>) -> Registry {
        Registry {
            config,
            state: Arc::new(Default::default()),
        }
    }

    /// Returns `SessionGuard` for other Node. Operation is atomic,
    /// you should get the same object in all places in the code.
    /// `SessionGuard` should be stored even if connection was closed,
    /// to avoid having multiple objects pointing to the same Node.
    pub async fn guard(&self, node_id: NodeId, addrs: &[SocketAddr]) -> RegistryEntry {
        let mut state = self.state.write().await;
        if let Some(target) = state.find(node_id, addrs) {
            return target;
        }

        let target = RegistryEntry::new(node_id, addrs.to_vec(), self.config.clone());

        state.by_node_id.insert(target.id, target.clone());
        for addr in addrs.iter() {
            state.by_addr.insert(*addr, target.clone());
        }

        target
    }

    /// Updates information about given Node.
    /// This function must keep data in `Registry` consistent. It should check all NodeIds,
    /// because some of them may already been in separate `RegistryEntries`. This could happen
    /// if we had only partial info about Nodes earlier.
    /// This is the reason we don't have update function on single `RegistryEntry`.
    pub async fn update_entry(&self, info: NodeInfo) -> anyhow::Result<()> {
        log::trace!("Updating `RegistryEntry` for [{}]", info.node_id());

        // TODO: For now we use the simplest implementation possible.
        //       Apply considerations from comment later.
        let addrs = info
            .endpoints
            .iter()
            .map(|endpoint| endpoint.address)
            .collect::<Vec<_>>();

        let mut state = self.state.write().await;
        let entries = info
            .identities
            .iter()
            .map(|ident| ident.node_id)
            .filter_map(|node| state.find(node, &[]))
            .collect::<Vec<_>>();

        // We are in correct state if all `RegistryEntries` are the same struct.
        let mut entry = if !entries.is_empty() {
            let state = entries[0].state.clone();
            if !entries
                .iter()
                .all(|entry| Arc::ptr_eq(&entry.state, &state))
            {
                // Maybe in some cases (when Node identities are changing) we could recover from this
                // but it is safer to just return error for now.
                bail!("Inconsistent `Registry` state. A few entries are pointing to the same Node.")
            }
            entries[0].clone()
        } else {
            RegistryEntry::new(info.node_id(), addrs.clone(), self.config.clone())
        };

        for id in &info.identities {
            state.by_node_id.insert(id.node_id, entry.clone());
        }

        for addr in addrs {
            state.by_addr.insert(addr, entry.clone());
        }

        entry.update_info(info.clone()).await;
        Ok(())
    }

    pub async fn get_entry(&self, node_id: NodeId) -> Option<RegistryEntry> {
        let mut state = self.state.read().await;
        state.find(node_id, &[])
    }

    pub async fn get_entry_by_addr(&self, remote: &SocketAddr) -> Option<RegistryEntry> {
        let mut state = self.state.read().await;
        state.find_by_addr(remote)
    }

    pub async fn lock_outgoing(
        &self,
        node_id: NodeId,
        addrs: &[SocketAddr],
        layer: impl SessionDeregistration,
    ) -> SessionLock {
        self.guard(node_id, addrs).await.lock_outgoing(layer).await
    }

    pub async fn lock_incoming(
        &self,
        node_id: NodeId,
        addrs: &[SocketAddr],
        layer: impl SessionDeregistration,
    ) -> SessionLock {
        self.guard(node_id, addrs).await.lock_incoming(layer).await
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
    async fn session_target(&self, node_id: NodeId) -> Result<RegistryEntry, TransitionError> {
        match { self.state.read().await.by_node_id.get(&node_id) } {
            None => Err(TransitionError::NodeNotFound(node_id)),
            Some(target) => Ok(target.clone()),
        }
    }

    pub async fn shutdown(&self) {}
}

impl RegistryState {
    fn find(&self, node_id: NodeId, addrs: &[SocketAddr]) -> Option<RegistryEntry> {
        self.by_node_id
            .get(&node_id)
            .or_else(|| addrs.iter().filter_map(|a| self.by_addr.get(a)).next())
            .cloned()
    }

    fn find_by_id(&self, node_id: NodeId) -> Option<RegistryEntry> {
        self.by_node_id.get(&node_id).cloned()
    }

    fn find_by_addr(&self, addr: &SocketAddr) -> Option<RegistryEntry> {
        self.by_addr.get(addr).cloned()
    }
}

#[derive(Clone)]
pub struct RegistryEntry {
    /// Node default id. Duplicates information in state, but can be accessed
    /// without acquiring and awaiting RwLock.  
    pub id: NodeId,

    state: Arc<RwLock<RegistryEntryState>>,
    state_notifier: Arc<broadcast::Sender<SessionState>>,

    /// Last update of information about Node.
    last_info_update: LastSeen,

    pub config: Arc<RegistryConfig>,
}

pub struct RegistryEntryState {
    /// TODO: We need to store public keys here. Using `NodeEntry<Identity>` is problematic
    ///       here, because we not always have full information, when initializing this
    ///       struct. To have full info, we need to query it from relay server. In case we got
    ///       `ReverseConnection` this could increase time to receiving first message by initiator.
    ///       On the other side if we don't have all Node identities, than we risk that we won't
    ///       protect ourselves from attempting to initialize 2 sessions at the same time.
    ///       For example we could create 2 separate RegistryEntries for default and secondary identity.
    ///       The second problem is the fact, that relay server doesn't have public key, so NodeEntry
    ///       structure is not suitable for this case.
    ///       As a compromise I'm using `Vec<Identity>` with hope that we can replace it with
    ///       `NodeEntry<Identity>` in the future.
    ///
    /// First `Identity` should be default id. Vector can be empty if we don't have enough information.
    node: Vec<Identity>,
    /// Assumes only UDP addresses.
    addresses: Vec<SocketAddr>,
    supported_encryption: Vec<String>,
    state: SessionState,
    /// Currently we are storing slot of Node on relay server. This assumes, that there is only
    /// one relay server, what I hope, will not be true forever.
    /// In the future we should store here slot assigned by us, that can be used to forward packets
    /// through our Node.
    slot: SlotId,

    /// Handle to abort initialization that's currently in progress.
    abort_handle: Option<AbortHandle>,
}

impl RegistryEntry {
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

    pub async fn transition_closed(&self) -> Result<(), TransitionError> {
        let new_state = {
            let mut target = self.state.write().await;
            target.state.transition(SessionState::Closed)?
        };

        self.notify_change(SessionState::Closed);
        Ok(())
    }

    pub async fn public_addresses(&self) -> Vec<SocketAddr> {
        let mut target = self.state.read().await;
        target.addresses.clone()
    }

    /// Returns `NodeInfo` wrapped with enum indicating if it is recommended to update information.
    ///
    /// There are few things taken into consideration:
    /// - Do we have any cached information? (Identities vector empty?)
    /// - When the last update of information happened?
    /// - Do we have established session with this Node? (In this case we threat
    ///   information about Node as up to date)
    pub async fn info(&self) -> Validity<NodeInfo> {
        let state = self.state.read().await;

        let should_update = if state.node.is_empty() {
            true
        } else if matches!(state.state, SessionState::Established(_)) {
            false
        } else {
            Utc::now() - self.last_info_update.time() > self.config.node_info_ttl
        };

        match should_update {
            true => Validity::UpdateRecommended(state.info()),
            false => Validity::UpToDate(state.info()),
        }
    }

    /// This function should be kept accessible only for this module, since the registry
    /// consistency depends on checks in `Registry`. Updating individual `RegistryEntry`
    /// could result in de-synchronization.
    pub(self) async fn update_info(&self, info: NodeInfo) -> anyhow::Result<()> {
        if !info.identities.is_empty() {
            // TODO: We should be resilient to identity change.
            if info.identities[0].node_id != self.id {
                bail!(
                    "Default NodeId changed from {} to {}",
                    self.id,
                    info.identities[0].node_id
                )
            }
        }

        // TODO: We can't be sure that new information is more valid than previous.
        //       For example if we got info directly from Node, than it should be more
        //       reliable than information from relays. But at the same time this direct
        //       information can be outdated.
        //       We need to consider all scenarios and find a best way to handle this.
        let mut state = self.state.write().await;

        state.slot = info.slot;
        state.supported_encryption = info.supported_encryption;
        // TODO: What should we do if identity lists differ? Is new list always better?
        state.node = info.identities;
        // TODO: We should distinguish between public IPs and addresses assigned temporarily
        //       by routers. `Registry` contains addresses from which we received packets.
        //       Here we assign only public, but earlier (`guard_initialization`) we added mapped addresses
        state.addresses = info.endpoints.into_iter().map(|e| e.address).collect();
        Ok(())
    }

    pub async fn identities(&self) -> anyhow::Result<NodeEntry<Identity>> {
        let mut target = self.state.read().await;
        let ids = target.node.clone();
        if ids.is_empty() {
            bail!("Identities list is empty")
        }

        Ok(NodeEntry::<Identity> {
            default_id: ids[0].clone(),
            identities: ids,
        })
    }

    fn notify_change(&self, new_state: SessionState) {
        log::trace!(
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

pub enum Validity<T: Sized> {
    UpToDate(T),
    UpdateRecommended(T),
}

impl<T> Validity<T>
where
    T: Sized,
{
    pub fn just_get(self) -> T {
        match self {
            Validity::UpToDate(content) => content,
            Validity::UpdateRecommended(content) => content,
        }
    }

    pub fn update_recommended(&self) -> bool {
        matches!(self, Validity::UpdateRecommended(_))
    }
}

impl RegistryEntryState {
    pub fn info(&self) -> NodeInfo {
        NodeInfo {
            identities: self.node.clone(),
            slot: self.slot,
            endpoints: self
                .addresses
                .iter()
                .map(|addr| Endpoint {
                    protocol: proto::Protocol::Udp,
                    address: *addr,
                })
                .collect(),
            supported_encryption: self.supported_encryption.clone(),
        }
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

impl RelayedState {
    #[allow(clippy::match_like_matches_macro)]
    pub fn allowed(&self, new_state: &RelayedState) -> bool {
        match (self, &new_state) {
            (RelayedState::Initializing, RelayedState::Ready) => true,
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
            (SessionState::Relayed(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Incoming(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Outgoing(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::ReverseConnection(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Incoming(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::Outgoing(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::Relayed(RelayedState::Ready), SessionState::Established(_)) => true,
            (
                SessionState::Outgoing(InitState::ConnectIntent),
                SessionState::Relayed(RelayedState::Initializing),
            ) => true,
            (
                SessionState::ReverseConnection(InitState::ConnectIntent),
                SessionState::Relayed(RelayedState::Initializing),
            ) => true,
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
            (SessionState::Established(_), SessionState::Closing) => true,
            (SessionState::Closing, SessionState::Closed) => true,
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
            (SessionState::Relayed(prev), SessionState::Relayed(next)) => prev.allowed(next),
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

impl RegistryEntry {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>, config: Arc<RegistryConfig>) -> Self {
        let (notify_msg, _) = broadcast::channel(10);

        Self {
            id: node_id,
            last_info_update: LastSeen::now(),
            state: Arc::new(RwLock::new(RegistryEntryState {
                node: vec![],
                addresses,
                supported_encryption: vec![],
                state: SessionState::Closed,
                slot: FORWARD_SLOT_ID,
                abort_handle: None,
            })),
            state_notifier: Arc::new(notify_msg),
            config,
        }
    }

    /// Make intent, that you want initialize session with this Node.
    /// Function either will return Lock granting you exclusive right to
    /// initialize this session or object for awaiting on session.
    /// TODO: Handle situation when we are attempting to connect and closing session at the same time.
    pub async fn lock_outgoing(&self, layer: impl SessionDeregistration) -> SessionLock {
        // We need to subscribe to state change events, before we will start transition,
        // otherwise a few events could be lost.
        let notifier = self.awaiting_notifier();
        match self.transition_outgoing(InitState::ConnectIntent).await {
            Ok(_) => SessionLock::Permit(SessionPermit {
                registry: self.clone(),
                layer: Arc::new(Box::new(layer)),
                result: None,
            }),
            Err(e) => {
                if let TransitionError::InvalidTransition(prev, _) = e {
                    if !matches!(prev, SessionState::Established(_)) {
                        log::debug!(
                            "Initialization of session with: [{}] in progress, state: {}. Thread will wait for finish.",
                            self.id, prev
                        )
                    }
                };

                SessionLock::Wait(notifier)
            }
        }
    }

    /// See: `SessionGuard::lock_outgoing`
    /// TODO: Unify implementation with `lock_outgoing`.
    pub async fn lock_incoming(&self, layer: impl SessionDeregistration) -> SessionLock {
        // We need to subscribe to state change events, before we will start transition,
        // otherwise a few events could be lost.
        let notifier = self.awaiting_notifier();
        match self.transition_incoming(InitState::ConnectIntent).await {
            Ok(_) => SessionLock::Permit(SessionPermit {
                registry: self.clone(),
                layer: Arc::new(Box::new(layer)),
                result: None,
            }),
            Err(e) => {
                if let TransitionError::InvalidTransition(prev, _) = e {
                    if !matches!(prev, SessionState::Established(_)) {
                        log::debug!(
                            "Initialization of session with: [{}] in progress, state: {}. Thread will wait for finish.",
                            self.id, prev
                        )
                    }
                };

                SessionLock::Wait(notifier)
            }
        }
    }

    pub fn awaiting_notifier(&self) -> NodeAwaiting {
        NodeAwaiting {
            registry: self.clone(),
            notifier: self.state_notifier.subscribe(),
        }
    }

    pub async fn state(&self) -> SessionState {
        self.state.read().await.state.clone()
    }

    pub async fn register_abortable(&self, abort: AbortHandle) {
        let mut state = self.state.write().await;
        state.abort_handle = Some(abort);
    }

    pub async fn abort_initialization(&self) {
        if let Some(handle) = {
            let mut state = self.state.write().await;
            state.abort_handle.take()
        } {
            log::debug!("Aborting session initialization with [{}]", self.id);
            handle.abort();
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
    pub registry: RegistryEntry,
    layer: Arc<Box<dyn SessionDeregistration>>,
    pub(crate) result: Option<Result<Arc<DirectSession>, SessionError>>,
}

impl SessionPermit {
    pub(crate) async fn async_drop(
        guard: RegistryEntry,
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

    pub fn collect_results(
        &mut self,
        result: Result<Arc<DirectSession>, SessionError>,
    ) -> Result<Arc<DirectSession>, SessionError> {
        self.result = Some(result.clone());
        result
    }

    /// Run initialization as abort-able future.
    ///
    /// It would be nice to unify this function with `SessionPermit::collect_results`, but the problem
    /// is that `collect_results` needs mut access to `SessionPermit` and abortable future needs reference
    /// to the same `SessionPermit`.
    pub async fn run_abortable<
        'a,
        F: Future<Output = Result<Arc<DirectSession>, SessionError>> + 'a,
    >(
        &'a self,
        future: F,
    ) -> Result<Arc<DirectSession>, SessionError> {
        let (abort_handle, abort_registration) = AbortHandle::new_pair();

        self.registry.register_abortable(abort_handle).await;

        Abortable::new(future, abort_registration)
            .await
            .map_err(|e| SessionError::Aborted(e.to_string()))?
    }
}

impl Drop for SessionPermit {
    fn drop(&mut self) {
        log::trace!("Dropping `SessionPermit` for {}.", self.registry.id);
        tokio::task::spawn_local(SessionPermit::async_drop(
            self.registry.clone(),
            self.result.take(),
        ));
    }
}

/// Structure for awaiting established connection.
pub struct NodeAwaiting {
    pub registry: RegistryEntry,
    notifier: broadcast::Receiver<SessionState>,
}

impl NodeAwaiting {
    pub async fn await_for_finish(&mut self) -> Result<Arc<DirectSession>, SessionError> {
        // If we query `NodeAwaiting` after session is already established, we won't get
        // any notification, so we must check state before entering loop.
        let mut state = self.registry.state().await;
        let node_id = self.registry.id;

        loop {
            match state {
                SessionState::Established(session) => {
                    return session.upgrade().ok_or(SessionError::Unexpected(
                        "Session closed unexpectedly.".to_string(),
                    ))
                }
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

impl Default for RegistryConfig {
    fn default() -> Self {
        RegistryConfig {
            node_info_ttl: chrono::Duration::seconds(300),
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

    use crate::testing::mocks::NoOpSessionLayer;
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

    async fn mock_establish_outgoing(
        mut permit: SessionPermit,
    ) -> anyhow::Result<Arc<DirectSession>> {
        let node = permit.registry.clone();
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

        let session = permit
            .collect_results(Ok(mock_session(&permit).await))
            .unwrap();
        drop(permit);
        Ok(session)
    }

    async fn mock_establish_incoming(
        mut permit: SessionPermit,
    ) -> anyhow::Result<Arc<DirectSession>> {
        let node = permit.registry.clone();
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

        let session = permit
            .collect_results(Ok(mock_session(&permit).await))
            .unwrap();
        drop(permit);
        Ok(session)
    }

    async fn mock_session(permit: &SessionPermit) -> Arc<DirectSession> {
        let node_id = permit.registry.id;
        let addr = permit.registry.public_addresses().await[0];

        let crypto_provider = *CRYPTOS.get(&node_id).unwrap();
        let crypto = crypto_provider.get(node_id).await.unwrap();
        let public_key = crypto.public_key().await.unwrap();
        let identity = Identity {
            node_id,
            public_key,
        };

        let (sink, _) = futures::channel::mpsc::channel::<(PacketKind, SocketAddr)>(1);

        let raw = RawSession::new(addr, SessionId::generate(), sink);
        DirectSession::new(permit.registry.id, vec![identity].into_iter(), raw).unwrap()
    }

    /// Checks if `Permits` and `NodeAwaiting` work independently.
    #[actix_rt::test]
    async fn test_session_guards_independent_permits() {
        let guards = Registry::default();
        let permit1 = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        let permit2 = match guards
            .lock_outgoing(*NODE_ID2, &[*ADDR2], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter1 = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter2 = match guards
            .lock_outgoing(*NODE_ID2, &[*ADDR2], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        // Initialize session related to permit 1
        // Waiters should work independently from each other. Waiting one initialization
        // shouldn't affect second initialization.
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _session = mock_establish_outgoing(permit1).await.unwrap();
            // Keep session so it won't be dropped and `await_for_finish` won't return error
            tokio::time::sleep(Duration::from_millis(300)).await;
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
        let guards = Registry::default();
        let permit = match guards
            .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter = match guards
            .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _session = mock_establish_incoming(permit).await.unwrap();
            // Keep session so it won't be dropped and `await_for_finish` won't return error
            tokio::time::sleep(Duration::from_millis(200)).await;
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
        let guards = Registry::default();
        let permit = match guards
            .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        /// We shouldn't get `Permit` if incoming session initialization started.
        let mut waiter = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _session = mock_establish_incoming(permit).await.unwrap();
            // Keep session so it won't be dropped and `await_for_finish` won't return error
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        timeout(Duration::from_millis(600), waiter.await_for_finish())
            .await
            .unwrap()
            .unwrap();
    }

    #[actix_rt::test]
    async fn test_session_guards_already_established() {
        let guards = Registry::default();
        let permit = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        let _session = mock_establish_outgoing(permit).await.unwrap();

        let mut waiter = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };
        timeout(Duration::from_millis(100), waiter.await_for_finish())
            .await
            .unwrap()
            .unwrap();
    }
}
