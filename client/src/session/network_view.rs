use anyhow::bail;
use chrono::Utc;
use futures::future::{AbortHandle, Abortable};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};

use super::session_state::{InitState, ReverseState, SessionState};
use crate::direct_session::{DirectSession, NodeEntry};
use crate::error::{SessionError, TransitionError};
use crate::session::session_traits::SessionDeregistration;

use crate::session::session_state::SessionState::Closed;
use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::{Endpoint, LastSeen, NodeInfo};
use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::{SlotId, FORWARD_SLOT_ID};

#[derive(Clone)]
pub struct NetworkViewConfig {
    pub node_info_ttl: chrono::Duration,
}

/// Collects our local knowledge about the network. Responsible for ensuring
/// that we won't attempt to initialize 2 sessions with the same Node at the same time.
///
/// In p2p network we can never have full knowledge about the network state, so although this struct
/// will attempt to keep information updated, we can't ever be sure that the information is fully correct.
#[derive(Clone, Default)]
pub struct NetworkView {
    config: Arc<NetworkViewConfig>,
    state: Arc<RwLock<NetworkViewState>>,
}

/// TODO: We never remove entries from State. In general we should keep entries
///       as long as possible, because removal could break uniqueness of NodeView
///       in case other threads will attempt to initialize session at this exact moment.
#[derive(Default)]
struct NetworkViewState {
    by_node_id: HashMap<NodeId, NodeView>,
    by_addr: HashMap<SocketAddr, NodeView>,
}

impl NetworkView {
    pub fn new(config: Arc<NetworkViewConfig>) -> NetworkView {
        NetworkView {
            config,
            state: Arc::new(Default::default()),
        }
    }

    /// Returns `SessionGuard` for other Node. Operation is atomic,
    /// you should get the same object in all places in the code.
    /// `SessionGuard` should be stored even if connection was closed,
    /// to avoid having multiple objects pointing to the same Node.
    pub async fn guard(&self, node_id: NodeId, addrs: &[SocketAddr]) -> NodeView {
        let mut state = self.state.write().await;
        if let Some(target) = state.find(node_id, addrs) {
            return target;
        }

        let target = NodeView::new(node_id, addrs.to_vec(), self.config.clone());

        state.by_node_id.insert(target.id, target.clone());
        for addr in addrs.iter() {
            state.by_addr.insert(*addr, target.clone());
        }

        target
    }

    /// Updates information about given Node.
    /// This function must keep data in `NetworkView` consistent. It should check all NodeIds,
    /// because some of them may already been in separate `NodeViews`. This could happen
    /// if we had only partial info about Nodes earlier.
    /// This is the reason we don't have update function on single `NodeView`.
    pub async fn update_entry(&self, info: NodeInfo) -> anyhow::Result<()> {
        log::trace!("Updating `NodeView` for [{}]", info.default_node_id());

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

        // We are in correct state if all `NodeViews` are the same struct.
        let entry = if !entries.is_empty() {
            let state = entries[0].state.clone();
            if !entries
                .iter()
                .all(|entry| Arc::ptr_eq(&entry.state, &state))
            {
                // Maybe in some cases (when Node identities are changing) we could recover from this
                // but it is safer to just return error for now.
                bail!("Inconsistent `NetworkView` state. A few entries are pointing to the same Node.")
            }
            entries[0].clone()
        } else {
            NodeView::new(info.default_node_id(), addrs.clone(), self.config.clone())
        };

        for id in &info.identities {
            state.by_node_id.insert(id.node_id, entry.clone());
        }

        for addr in addrs {
            state.by_addr.insert(addr, entry.clone());
        }

        entry.update_info(info.clone()).await.ok();
        Ok(())
    }

    pub async fn get_entry(&self, node_id: NodeId) -> Option<NodeView> {
        let state = self.state.read().await;
        state.find(node_id, &[])
    }

    pub async fn get_entry_by_addr(&self, remote: &SocketAddr) -> Option<NodeView> {
        let state = self.state.read().await;
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
    async fn session_target(&self, node_id: NodeId) -> Result<NodeView, TransitionError> {
        match { self.state.read().await.by_node_id.get(&node_id) } {
            None => Err(TransitionError::NodeNotFound(node_id)),
            Some(target) => Ok(target.clone()),
        }
    }

    pub async fn shutdown(&self) {}
}

impl NetworkViewState {
    fn find(&self, node_id: NodeId, addrs: &[SocketAddr]) -> Option<NodeView> {
        self.by_node_id
            .get(&node_id)
            .or_else(|| addrs.iter().filter_map(|a| self.by_addr.get(a)).next())
            .cloned()
    }

    fn find_by_addr(&self, addr: &SocketAddr) -> Option<NodeView> {
        self.by_addr.get(addr).cloned()
    }
}

#[derive(Clone)]
pub struct NodeView {
    /// Node default id. Duplicates information in state, but can be accessed
    /// without acquiring and awaiting RwLock.  
    pub id: NodeId,

    state: Arc<RwLock<NodeViewState>>,
    state_notifier: Arc<broadcast::Sender<SessionState>>,

    /// Last update of information about Node.
    last_info_update: LastSeen,

    pub config: Arc<NetworkViewConfig>,
}

pub struct NodeViewState {
    /// TODO: We need to store public keys here. Using `NodeEntry<Identity>` is problematic
    ///       here, because we not always have full information, when initializing this
    ///       struct. To have full info, we need to query it from relay server. In case we got
    ///       `ReverseConnection` this could increase time to receiving first message by initiator.
    ///       On the other side if we don't have all Node identities, than we risk that we won't
    ///       protect ourselves from attempting to initialize 2 sessions at the same time.
    ///       For example we could create 2 separate `NodeViews` for default and secondary identity.
    ///       The second problem is the fact, that relay server doesn't have public key, so `NodeView`
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
    /// one relay server, what we hope that it will not be true forever.
    /// In the future we should store here slot assigned by us, that can be used to forward packets
    /// through our Node.
    slot: SlotId,

    /// Handle to abort initialization that's currently in progress.
    /// We will have 2 abort handles during `ReverseConnection` initialization.
    abort_handle: Vec<AbortHandle>,
}

impl NodeView {
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
        let target = self.state.read().await;
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
    /// consistency depends on checks in `NetworkView`. Updating individual `NodeView`
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
        //       by routers. `NetworkView` contains addresses from which we received packets.
        //       Here we assign only public, but earlier (`NetworkView::guard`) we added mapped addresses
        state.addresses = info.endpoints.into_iter().map(|e| e.address).collect();
        Ok(())
    }

    pub async fn identities(&self) -> anyhow::Result<NodeEntry<Identity>> {
        let target = self.state.read().await;
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
            .map_err(|_| log::trace!("Notifying state change for [{}]: No listeners", self.id))
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

impl NodeViewState {
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

impl NodeView {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>, config: Arc<NetworkViewConfig>) -> Self {
        let (notify_msg, _) = broadcast::channel(10);

        Self {
            id: node_id,
            last_info_update: LastSeen::now(),
            state: Arc::new(RwLock::new(NodeViewState {
                node: vec![],
                addresses,
                supported_encryption: vec![],
                state: SessionState::Closed,
                slot: FORWARD_SLOT_ID,
                abort_handle: vec![],
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
                reverse: false,
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
            Ok(state) => SessionLock::Permit(SessionPermit {
                registry: self.clone(),
                reverse: matches!(
                    state,
                    SessionState::ReverseConnection(ReverseState::InProgress(
                        InitState::ConnectIntent
                    ))
                ),
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
        state.abort_handle.push(abort);
    }

    pub async fn unregister_abortable(&self) {
        let mut state = self.state.write().await;
        state.abort_handle.clear();
    }

    pub async fn abort_initialization(&self) {
        let handles = {
            let mut state = self.state.write().await;
            state.abort_handle.drain(..).collect::<Vec<_>>()
        };

        if !handles.is_empty() {
            log::debug!("Aborting session initialization with [{}]", self.id);
            for handle in handles {
                handle.abort();
            }
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
/// Ensures clear Session state on drop including un-registration from `SessionLayer`.
pub struct SessionPermit {
    pub registry: NodeView,
    /// Flag is set to true, if the Permit was created for incoming connection in response
    /// to `ReverseConnection`. In this case we have 2 permits existing at the same time, because
    /// at the same time other part of code is waiting for incoming connection.
    reverse: bool,
    layer: Arc<Box<dyn SessionDeregistration>>,
    pub(crate) result: Option<Result<Arc<DirectSession>, SessionError>>,
}

impl SessionPermit {
    fn final_state(&mut self) -> SessionState {
        match self.result.take() {
            None => {
                let err = SessionError::ProgrammingError(
                    "Dropping `SessionPermit` without result.".to_string(),
                );

                if self.reverse {
                    SessionState::ReverseConnection(ReverseState::Finished(Err(err)))
                } else {
                    SessionState::FailedEstablish(err)
                }
            }
            Some(result) if self.reverse => {
                SessionState::ReverseConnection(ReverseState::Finished(result))
            }
            Some(Ok(session)) => SessionState::Established(Arc::downgrade(&session)),
            Some(Err(e)) => SessionState::FailedEstablish(e),
        }
    }

    pub(crate) async fn async_drop(
        node: NodeView,
        layer: Arc<Box<dyn SessionDeregistration>>,
        new_state: SessionState,
    ) {
        let node_id = node.id;
        let reverse = matches!(&new_state, SessionState::ReverseConnection(_));

        match &new_state {
            SessionState::FailedEstablish(_) => Self::clean_state(&node, layer.clone()).await,
            SessionState::ReverseConnection(ReverseState::Finished(_)) => {}
            _ => node.unregister_abortable().await,
        }

        // We don't expect to fail here, so if we do, something has gone really bad.
        if let Err(e) = node.transition(new_state.clone()).await {
            if reverse {
                return;
            }

            log::warn!("Dropping Permit for Node {node_id}: {e}");

            Self::clean_state(&node, layer).await;

            // Error really shouldn't happen here.
            node.transition(SessionState::FailedEstablish(
                SessionError::ProgrammingError(format!(
                    "Failed to transition to state: {new_state}: {e}"
                )),
            ))
            .await
            .ok();
        }
    }

    /// Makes sure we deregister all data about this Node and abort all futures
    /// that might be during initialization.
    pub(crate) async fn clean_state(node: &NodeView, layer: Arc<Box<dyn SessionDeregistration>>) {
        for addr in node.public_addresses().await {
            layer.abort_initializations(addr).await.ok();
        }
        layer.unregister(node.id).await;
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
        let is_reverse = match self.reverse {
            true => " reverse",
            false => "",
        };
        log::trace!(
            "Dropping{is_reverse} `SessionPermit` for [{}].",
            self.registry.id
        );
        tokio::task::spawn_local(SessionPermit::async_drop(
            self.registry.clone(),
            self.layer.clone(),
            self.final_state(),
        ));
    }
}

/// Structure for awaiting established connection.
pub struct NodeAwaiting {
    pub registry: NodeView,
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
                    log::trace!("Finished waiting for established session with [{node_id}].");
                    return session.upgrade().ok_or(SessionError::Unexpected(
                        "Session closed unexpectedly.".to_string(),
                    ));
                }
                // TODO: We would like to return more meaningful error message.
                SessionState::Closed => {
                    return Err(SessionError::Unexpected("Connection closed.".to_string()))
                }
                SessionState::FailedEstablish(e) => return Err(e),
                _ => {
                    log::trace!("Waiting for established session with [{node_id}]: state: {state}")
                }
            };

            state = self.next().await?;
        }
    }

    pub async fn await_for_closed_or_failed(&mut self) -> Result<(), SessionError> {
        // If we query `NodeAwaiting` after session is already established, we won't get
        // any notification, so we must check state before entering loop.
        let mut state = self.registry.state().await;
        let node_id = self.registry.id;

        loop {
            match state {
                SessionState::Closed => {
                    return Err(SessionError::Unexpected("Connection closed.".to_string()))
                }
                SessionState::FailedEstablish(e) => return Err(e),
                _ => {
                    log::trace!("Waiting for Closed or FailedEstablished session with [{node_id}]. Current state: {state}")
                }
            };

            state = self.next().await?;
        }
    }

    /// When state reaches `ChallengeHandshake`, we've got first response from other Node
    /// so we know that it is responsive.
    ///
    /// Function assumes that `NodeAwaiting` was created, before the state change happened,
    /// otherwise we might miss the event.
    pub async fn await_handshake(&mut self) -> Result<(), SessionError> {
        let mut state = self.registry.state().await;
        let node_id = self.registry.id;

        loop {
            // This function is used for `ReverseConnection`, so we are interested only in
            // `SessionState::ReverseConnection(InitState::ChallengeHandshake)` state, but it doesn't
            // hurt us to include other Handshake states and this way we will have more general function, that
            // can be used to wait for any first messages exchange between Nodes.
            match state {
                SessionState::Outgoing(InitState::ChallengeHandshake)
                | SessionState::Incoming(InitState::ChallengeHandshake)
                | SessionState::ReverseConnection(ReverseState::InProgress(
                    InitState::ChallengeHandshake,
                )) => {
                    log::trace!("Finished waiting for first message from [{node_id}].");
                    return Ok(());
                }
                // Still waiting.
                _ => (),
            };

            state = self.next().await?;
        }
    }

    pub async fn await_reverse_finish(&mut self) -> Result<Arc<DirectSession>, SessionError> {
        let mut state = self.registry.state().await;
        loop {
            if let SessionState::ReverseConnection(ReverseState::Finished(result)) = state {
                return result;
            };

            state = self.next().await?;
        }
    }

    async fn next(&mut self) -> Result<SessionState, SessionError> {
        match self.notifier.recv().await {
            Ok(state) => Ok(state),
            Err(RecvError::Closed) => Err(SessionError::Internal(
                "Waiting for session initialization: notifier dropped.".to_string(),
            )),
            Err(RecvError::Lagged(lost)) => {
                // TODO: Maybe we could handle lags better, by checking current state?
                Err(SessionError::Internal(format!("Waiting for session initialization: notifier lagged. Lost {lost} state change(s).")))
            }
        }
    }
}

impl Default for NetworkViewConfig {
    fn default() -> Self {
        NetworkViewConfig {
            node_info_ttl: chrono::Duration::seconds(300),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::format;

    use lazy_static::lazy_static;

    use std::str::FromStr;
    use std::time::Duration;
    use strum::EnumCount;
    use tokio::time::timeout;

    use super::super::session_state::{InitState, RelayedState};
    use crate::raw_session::RawSession;
    use crate::testing::mocks::NoOpSessionLayer;

    use ya_relay_core::crypto::{Crypto, CryptoProvider, FallbackCryptoProvider};
    use ya_relay_core::server_session::SessionId;
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

        let transitions = [
            InitState::Initializing,
            InitState::ChallengeHandshake,
            InitState::HandshakeResponse,
            InitState::ChallengeVerified,
            InitState::SessionRegistered,
            InitState::Ready,
        ];

        for transition in transitions {
            let transition_result = node.transition_outgoing(transition).await;
            if transition_result.is_err() {
                bail!("{:?}", transition_result.err());
            };
        }

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

        let transitions = [
            InitState::Initializing,
            InitState::ChallengeHandshake,
            InitState::HandshakeResponse,
            InitState::ChallengeVerified,
            InitState::SessionRegistered,
            InitState::Ready,
        ];

        for transition in transitions {
            let transition_result = node.transition_incoming(transition).await;
            if transition_result.is_err() {
                bail!("{:?}", transition_result.err());
            };
        }

        let session = permit
            .collect_results(Ok(mock_session(&permit).await))
            .unwrap();
        drop(permit);
        Ok(session)
    }

    async fn mock_session(permit: &SessionPermit) -> Arc<DirectSession> {
        let node_id = permit.registry.id;
        let addr = permit.registry.public_addresses().await[0];

        mock_session_from_id_addr(node_id, addr).await
    }

    async fn mock_session_from_id_addr(node_id: NodeId, addr: SocketAddr) -> Arc<DirectSession> {
        let crypto_provider = *CRYPTOS.get(&node_id).unwrap();
        let crypto = crypto_provider.get(node_id).await.unwrap();
        let public_key = crypto.public_key().await.unwrap();
        let identity = Identity {
            node_id,
            public_key,
        };

        let (sink, _) = futures::channel::mpsc::channel::<(PacketKind, SocketAddr)>(1);

        let raw = RawSession::new(addr, SessionId::generate(), sink);
        DirectSession::new(node_id, vec![identity].into_iter(), raw).unwrap()
    }

    /// Checks if `Permits` and `NodeAwaiting` work independently.
    #[actix_rt::test]
    async fn test_network_view_independent_permits() {
        let guards = NetworkView::default();
        let permit1 = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        let _permit2 = match guards
            .lock_outgoing(*NODE_ID2, &[*ADDR2], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        // Second call to `lock_outgoing` should give us `SessionLock::Wait`
        let mut waiter1 = match guards
            .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(_) => panic!("Expected Waiter not Permit"),
            SessionLock::Wait(waiter) => waiter,
        };

        // Second call to `lock_outgoing` should give us `SessionLock::Wait`
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
    async fn test_network_view_incoming() {
        let guards = NetworkView::default();
        let permit = match guards
            .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        // Second call to `lock_outgoing` should give us `SessionLock::Wait`
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
    async fn test_network_view_incoming_and_outgoing() {
        let guards = NetworkView::default();
        let permit = match guards
            .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
            .await
        {
            SessionLock::Permit(permit) => permit,
            SessionLock::Wait(_) => panic!("Expected initialization Permit"),
        };

        // We shouldn't get `Permit` if incoming session initialization started.
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
    async fn test_network_view_already_established() {
        let guards = NetworkView::default();
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

    /// There are only 2 states from which transition to `ConnectIntent` should give us permit.
    /// Moreover from `ReverseState::Awaiting` we can get reverse `SessionPermit`, when calling
    /// `lock_incoming` (`lock_outgoing` doesn't give Permit).
    /// If this test fails, we are in danger of double session initialization.
    #[actix_rt::test]
    async fn test_network_view_permits_for_different_states() {
        let session = mock_session_from_id_addr(*NODE_ID1, *ADDR1).await;
        let mock_err = Err(SessionError::Unexpected("".to_string()));

        // Sanity checks. If the number of variants doesn't match, we need to update this test,
        // because we introduced or removed states.
        assert_eq!(SessionState::COUNT, 9);
        assert_eq!(InitState::COUNT, 7);
        assert_eq!(ReverseState::COUNT, 3);
        assert_eq!(RelayedState::COUNT, 2);

        // It is hard to generate all variants automatically if SessionState has sub-enums
        // and some variants contain additional data.
        let not_permitted_states = [
            SessionState::RestartConnect,
            SessionState::Closing,
            SessionState::Established(Arc::downgrade(&session)),
            SessionState::Outgoing(InitState::ChallengeHandshake),
            SessionState::Outgoing(InitState::ConnectIntent),
            SessionState::Outgoing(InitState::Initializing),
            SessionState::Outgoing(InitState::HandshakeResponse),
            SessionState::Outgoing(InitState::ChallengeVerified),
            SessionState::Outgoing(InitState::Ready),
            SessionState::Outgoing(InitState::SessionRegistered),
            SessionState::Incoming(InitState::ChallengeHandshake),
            SessionState::Incoming(InitState::ConnectIntent),
            SessionState::Incoming(InitState::Initializing),
            SessionState::Incoming(InitState::HandshakeResponse),
            SessionState::Incoming(InitState::ChallengeVerified),
            SessionState::Incoming(InitState::Ready),
            SessionState::Incoming(InitState::SessionRegistered),
            SessionState::Relayed(RelayedState::Initializing),
            SessionState::Relayed(RelayedState::Ready),
            SessionState::ReverseConnection(ReverseState::Awaiting),
            SessionState::ReverseConnection(ReverseState::Finished(mock_err)),
            SessionState::ReverseConnection(ReverseState::InProgress(
                InitState::ChallengeHandshake,
            )),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::ConnectIntent)),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::Initializing)),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::HandshakeResponse)),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::ChallengeVerified)),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::Ready)),
            SessionState::ReverseConnection(ReverseState::InProgress(InitState::SessionRegistered)),
            // These 2 states will give us `SessionPermit`
            //SessionState::Closed,
            //SessionState::FailedEstablish(mock_err),
        ];

        for state in not_permitted_states {
            let view = NetworkView::default();
            let node_view = view.guard(*NODE_ID1, &[*ADDR1]).await;

            {
                node_view.state.write().await.state = state.clone();
            }

            if let SessionLock::Permit(_permit) = view
                .lock_outgoing(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
                .await
            {
                panic!("We shouldn't get Permit from state: {}", state);
            }

            if state == SessionState::ReverseConnection(ReverseState::Awaiting) {
                continue;
            }

            if let SessionLock::Permit(_permit) = view
                .lock_incoming(*NODE_ID1, &[*ADDR1], NoOpSessionLayer {})
                .await
            {
                panic!("We shouldn't get Permit from state: {}", state);
            }
        }
    }
}
