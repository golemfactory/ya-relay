use anyhow::bail;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;

use crate::session::{SessionError, SessionResult};
use crate::Session;

#[derive(Clone, Default)]
pub struct GuardedSessions {
    state: Arc<RwLock<GuardedSessionsState>>,
}

impl GuardedSessions {
    pub async fn guard_initialization(
        &self,
        node_id: NodeId,
        addrs: &[SocketAddr],
    ) -> Arc<RwLock<()>> {
        if let Some(target) = {
            let state = self.state.read().await;
            state.find(node_id, addrs)
        } {
            return target.lock;
        }

        let target = SessionTarget::new(node_id, addrs.to_vec());
        let mut state = self.state.write().await;
        state.add(target.clone());

        target.lock.clone()
    }

    pub async fn stop_guarding(&self, node_id: NodeId, result: SessionResult<Arc<Session>>) {
        log::debug!("Stop guarding session initialization with: [{node_id}]");

        let mut state = self.state.write().await;
        if let Some(target) = state.find_by_id(node_id) {
            state.remove(&target);
            drop(state);

            log::debug!("Sending session init finish notification for Node: {node_id}");
            target.notify_finish.send(result).ok();
        }
    }

    /// Creates temporary session used only during initialization.
    /// Only one session will be created for one target Node address.
    /// If temporary session was already created, this function will wait for initialization finish.
    pub async fn temporary_session(&self, addr: SocketAddr, sink: OutStream) -> Arc<Session> {
        let mut state = self.state.write().await;
        match state.tmp_sessions.get(&addr) {
            None => {
                let session = Session::new(addr, SessionId::generate(), sink);
                state.tmp_sessions.insert(addr, session.clone());
                session
            }
            Some(session) => session.clone(),
        }
    }

    pub async fn register_waiting_for_node(&self, node_id: NodeId) -> anyhow::Result<NodeAwaiting> {
        let state = self.state.read().await;
        if let Some(target) = state.by_node_id.get(&node_id) {
            target.wait_for_connection.store(true, Ordering::SeqCst);
            Ok(NodeAwaiting {
                notify_finish: target.notify_finish.subscribe(),
                notify_msg: target.notify_msg.subscribe(),
            })
        } else {
            // If we are waiting for node to connect, we should already have entry
            // initialized by function `guard_initialization`. So this is programming error.
            bail!("Programming error. Waiting for node [{node_id}] to connect, without calling `guard_initialization` earlier.")
        }
    }

    pub async fn try_access_unguarded(&self, node_id: NodeId) -> bool {
        let state = self.state.read().await;
        if let Some(target) = state.by_node_id.get(&node_id) {
            target.wait_for_connection.swap(false, Ordering::SeqCst)
        } else {
            false
        }
    }

    pub async fn notify_first_message(&self, node_id: NodeId) {
        let sender = match { self.state.read().await.by_node_id.get(&node_id) } {
            None => return,
            Some(target) => target.notify_msg.clone(),
        };

        sender.send(()).ok();
    }

    /// Differs from `temporary_session`, that it doesn't create session, if it doesn't exist.
    pub async fn get_temporary_session(&self, addr: &SocketAddr) -> Option<Arc<Session>> {
        self.state.read().await.tmp_sessions.get(addr).cloned()
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.write().await;
        state.tmp_sessions.clear();
    }
}

#[derive(Default)]
struct GuardedSessionsState {
    by_node_id: HashMap<NodeId, SessionTarget>,
    by_addr: HashMap<SocketAddr, SessionTarget>,

    /// Temporary sessions stored during initialization period.
    /// After session is established, new struct in SessionManager is created
    /// and this one is removed.
    tmp_sessions: HashMap<SocketAddr, Arc<Session>>,
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

    fn add(&mut self, target: SessionTarget) {
        for addr in target.addresses.iter() {
            self.by_addr.insert(*addr, target.clone());
        }
        self.by_node_id.insert(target.node_id, target);
    }

    fn remove(&mut self, target: &SessionTarget) {
        for addr in target.addresses.iter() {
            self.by_addr.remove(addr);
            self.tmp_sessions.remove(addr);
        }
        self.by_node_id.remove(&target.node_id);
    }
}

#[derive(Clone)]
struct SessionTarget {
    node_id: NodeId,
    addresses: Arc<Vec<SocketAddr>>,
    lock: Arc<RwLock<()>>,

    /// In case of ReverseConnection, we would like to allow one incoming
    /// Session initialization attempt.
    wait_for_connection: Arc<AtomicBool>,

    /// Notifies, when establishing session is finished either with success or with failure.
    notify_finish: Arc<broadcast::Sender<SessionResult<Arc<Session>>>>,
    /// Notifies first connection message received from other party.
    /// Check `try_reverse_connection`, why we need this.
    notify_msg: Arc<broadcast::Sender<()>>,
}

impl SessionTarget {
    fn new(node_id: NodeId, addresses: Vec<SocketAddr>) -> Self {
        let (notify_finish, _) = broadcast::channel(1);
        let (notify_msg, _) = broadcast::channel(1);
        Self {
            node_id,
            addresses: Arc::new(addresses),
            lock: Default::default(),
            wait_for_connection: Arc::new(AtomicBool::new(false)),
            notify_finish: Arc::new(notify_finish),
            notify_msg: Arc::new(notify_msg),
        }
    }
}

pub struct NodeAwaiting {
    /// Notifies, when establishing session is finished either with success or with failure.
    notify_finish: broadcast::Receiver<SessionResult<Arc<Session>>>,
    /// Notifies first connection message received from other party.
    /// Check `try_reverse_connection`, why we need this.
    notify_msg: broadcast::Receiver<()>,
}

impl NodeAwaiting {
    pub async fn wait_for_first_message(&mut self) {
        self.notify_msg.recv().await.ok();
    }

    pub async fn wait_for_connection(&mut self) -> SessionResult<Arc<Session>> {
        self.notify_finish.recv().await.map_err(|_| {
            SessionError::Drop("Channel error while waiting for connection".to_string())
        })?
    }
}
