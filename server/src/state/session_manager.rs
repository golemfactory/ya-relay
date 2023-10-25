use crate::state::hamming_distance;
use crate::state::last_seen::{Clock, LastSeen};
use crate::state::session_manager::metrics::SessionManagerMetrics;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::cmp::{Ordering, Reverse};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::iter;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::time;
use ya_relay_core::challenge::RawChallenge;
use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Endpoint, Protocol};

mod metrics {
    use metrics::{recorder, Counter, Key, Gauge};

    const SESSIONS: Key = Key::from_static_name("ya-relay.session");

    const NODES: Key = Key::from_static_name("ya-relay.session.nodes");
    const CREATED: Key = Key::from_static_name("ya-relay.session.created");
    const REMOVED: Key = Key::from_static_name("ya_relay.session.removed");

    const PURGED: Key = Key::from_static_name("ya_relay.session.purged");

    pub struct SessionManagerMetrics {
        pub created: Counter,
        pub removed: Counter,
        pub purged: Counter,
        pub sessions : Gauge,
        pub nodes : Gauge
    }

    impl Default for SessionManagerMetrics {
        fn default() -> Self {
            let r = recorder();
            let created = r.register_counter(&CREATED);
            let removed = r.register_counter(&REMOVED);
            let purged = r.register_counter(&PURGED);
            let sessions = r.register_gauge(&SESSIONS);
            let nodes = r.register_gauge(&NODES);

            Self {
                created,
                removed,
                purged,
                sessions,
                nodes
            }
        }
    }
}

pub type SessionRef = Arc<Session>;

pub type SessionWeakRef = Weak<Session>;

pub struct Session {
    pub session_id: SessionId,
    pub peer: SocketAddr,
    pub ts: LastSeen,
    pub state: Mutex<SessionState>,
}

impl Session {
    pub fn node_id(&self) -> Option<NodeId> {
        match &*self.state.lock() {
            SessionState::Est { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }
}

pub enum AddrStatus {
    Unknown,
    Pending,
    Valid,
    Invalid,
}

impl AddrStatus {
    pub fn endpoints(&self, peer: SocketAddr) -> Vec<Endpoint> {
        match self {
            AddrStatus::Valid => vec![Endpoint {
                protocol: Protocol::Udp.into(),
                address: peer.ip().to_string(),
                port: peer.port() as u32,
            }],
            _ => Vec::new(),
        }
    }
}

pub enum SessionState {
    Pending {
        challenge: RawChallenge,
        difficulty: u64,
    },
    Est {
        node_id: NodeId,
        keys: Vec<Identity>,
        addr_status: AddrStatus,
    },
    #[cfg(test)]
    Dummy,
}

type NodeSessionSet = Arc<Mutex<Vec<SessionWeakRef>>>;

pub struct SessionManager {
    sessions: [Mutex<HashMap<SessionId, SessionRef>>; 16],
    node_sessions: DashMap<NodeId, NodeSessionSet>,
    metrics: SessionManagerMetrics,
}

impl SessionManager {
    pub fn new() -> Arc<Self> {
        let sessions: [Mutex<HashMap<SessionId, SessionRef>>; 16] = Default::default();
        let node_sessions = Default::default();
        let metrics = Default::default();

        assert_eq!(sessions.len(), 0x10);

        Arc::new(Self {
            sessions,
            node_sessions,
            metrics,
        })
    }

    pub fn start_cleanup_processor(
        self: &Arc<Self>,
        session_cleaner_interval: Duration,
        session_timeout: Duration,
        session_purge_timeout: Duration,
    ) {
        let g_nodes = self.metrics.nodes.clone();
        let g_sessions = self.metrics.sessions.clone();

        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                log::debug!("clean wait");
                time::sleep(session_cleaner_interval).await;
                log::debug!("clean start {:?}", session_purge_timeout);
                let clock = Clock::now();
                let sm = match this.upgrade() {
                    Some(sm) => sm,
                    None => break,
                };

                let mut total_clean = 0;
                let mut total_size = 0;
                for slot in &sm.sessions {
                    let mut g = slot.lock();
                    let start_size = g.len();
                    g.retain(|session_id, session_ref| {
                        let age = clock.age(&session_ref.ts);
                        age <= session_purge_timeout
                    });
                    let end_size = black_box(g.len());
                    total_size += end_size;
                    let removed = start_size - end_size;
                    drop(g);

                    total_clean += removed;
                    if removed > 0 {
                        sm.metrics.purged.increment(removed as u64);
                        sm.metrics.removed.increment(removed as u64);
                        log::debug!("session clean {removed} removed from shard");
                    }
                }
                log::debug!("clean end: {total_clean}/{total_size}");
                g_sessions.set(total_size as f64);
                sm.clean_node_sessions();
                g_nodes.set(sm.node_sessions.len() as f64);
            }
        });
    }

    pub fn neighbours(&self, base_node_id: NodeId, count: usize) -> Vec<SessionRef> {
        #[derive(PartialEq, Eq)]
        struct Distance {
            distance: Reverse<u32>,
            id: NodeId,
        }

        impl PartialOrd for Distance {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for Distance {
            fn cmp(&self, other: &Self) -> Ordering {
                let lhr: (Reverse<u32>, &[u8]) = (self.distance, self.id.as_ref());
                let rhr: (Reverse<u32>, &[u8]) = (other.distance, other.id.as_ref());

                lhr.cmp(&rhr)
            }
        }

        let mut h = self
            .node_sessions
            .iter()
            .map(|entry| {
                let id = *entry.key();
                let distance = Reverse(hamming_distance(base_node_id, id));
                Distance { distance, id }
            })
            .collect::<BinaryHeap<_>>();

        iter::from_fn(|| h.pop())
            .filter_map(|d| {
                self.node_sessions
                    .get(&d.id)
                    .and_then(|entry| entry.value().lock().iter().filter_map(Weak::upgrade).last())
            })
            .skip(1)
            .take(count)
            .collect()
    }

    pub fn link_session(&self, node_id: NodeId, session: &SessionRef) {
        let session_w = Arc::downgrade(session);
        let entry = self.node_sessions.entry(node_id).or_default();
        let mut g = entry.lock();
        g.retain(|s| s.upgrade().is_some());
        g.push(session_w)
    }

    pub fn node_session(&self, node_id: NodeId) -> Option<SessionRef> {
        if let Some(refs) = self.node_sessions.get_mut(&node_id) {
            let mut g = refs.value().lock();
            while let Some(session_wref) = g.last() {
                if let Some(session_ref) = session_wref.upgrade() {
                    return Some(session_ref);
                }
                g.pop();
            }
        }
        None
    }

    fn clean_node_sessions(&self) {
        self.node_sessions.retain(|&node_id, sessions| {
            let mut g = sessions.lock();
            g.retain(|s| s.upgrade().is_some());
            !g.is_empty()
        })
    }

    pub fn new_session(
        &self,
        clock: &Clock,
        peer: SocketAddr,
        challenge: RawChallenge,
        difficulty: u64,
    ) -> SessionRef {
        self.metrics.created.increment(1);
        let session_id = SessionId::generate();
        let state = Mutex::new(SessionState::Pending {
            challenge,
            difficulty,
        });
        let ts = clock.last_seen();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            state,
        });

        self.session_slot(&session_id)
            .lock()
            .insert(session_id, session_ref.clone());
        session_ref
    }

    #[cfg(test)]
    fn add_dummy_session(&self) -> SessionRef {
        let session_id = SessionId::generate();
        let state = Mutex::new(SessionState::Dummy);
        let ts = LastSeen::now();
        let peer = "127.0.0.1:40".parse().unwrap();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            state,
        });
        self.session_slot(&session_id)
            .lock()
            .insert(session_id, session_ref.clone());
        session_ref
    }

    #[cfg(test)]
    fn add_est_session(&self, node_id: NodeId) -> SessionRef {
        let session_id = SessionId::generate();
        let state = Mutex::new(SessionState::Est {
            node_id,
            addr_status: AddrStatus::Unknown
        });
        let ts = LastSeen::now();
        let peer = "127.0.0.1:40".parse().unwrap();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            state,
        });
        self.session_slot(&session_id)
            .lock()
            .insert(session_id, session_ref.clone());
        session_ref
    }

    pub fn session(&self, session_id: &SessionId) -> Option<SessionRef> {
        self.session_slot(session_id)
            .lock()
            .get(&session_id)
            .cloned()
    }

    pub fn with_session<Out, F: FnOnce(&Session) -> Out>(
        &self,
        session_id: &SessionId,
        f: F,
    ) -> Option<Out> {
        if let Some(session_ref) = self.session(session_id) {
            Some(f(&session_ref))
        } else {
            None
        }
    }

    pub fn remove_session(&self, session: &SessionId) -> Option<SessionRef> {
        let prev = self.session_slot(session).lock().remove(session);
        if prev.is_some() {
            self.metrics.removed.increment(1);
        }
        prev
    }

    fn session_slot(&self, session: &SessionId) -> &Mutex<HashMap<SessionId, SessionRef>> {
        let mut s = DefaultHasher::new();
        session.hash(&mut s);
        let idx = (s.finish() & 0xf) as usize;
        &self.sessions[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;
    use rand::{thread_rng, Rng};
    use ya_relay_core::NodeId;

    fn gen_node_id() -> NodeId {
        thread_rng().gen::<[u8; 20]>().into()
    }

    #[test]
    fn test_clean_sessions() {
        let sm = SessionManager::new();
        let n1: NodeId = gen_node_id();
        let n2: NodeId = gen_node_id();
        let n3: NodeId = gen_node_id();
        let n4: NodeId = gen_node_id();
        let (s1, s2, s3) = (
            sm.add_dummy_session(),
            sm.add_dummy_session(),
            sm.add_dummy_session(),
        );
        sm.link_session(n1, &s1);
        sm.link_session(n2, &s2);
        sm.link_session(n3, &s3);
        sm.link_session(n4, &s3);
        let session_id_1 = s1.session_id;
        drop((s1, s2, s3));
        assert_eq!(sm.node_sessions.len(), 4);
        sm.clean_node_sessions();
        assert_eq!(sm.node_sessions.len(), 4);
        assert!(sm.node_session(n1).is_some());
        assert!(sm.node_session(n2).is_some());
        assert!(sm.node_session(n3).is_some());
        assert!(Arc::ptr_eq(
            &sm.node_session(n3).unwrap(),
            &sm.node_session(n4).unwrap()
        ));
        sm.remove_session(&session_id_1);
        assert!(sm.node_session(n1).is_none());
        let session_id_3 = sm.node_session(n4).unwrap();
        sm.remove_session(&session_id_3.session_id);
        drop(session_id_3);
        assert!(sm.node_session(n3).is_none());
        assert!(sm.node_session(n4).is_none());
        sm.clean_node_sessions();
        assert_eq!(sm.node_sessions.len(), 1);
    }

    #[test]
    fn test_neighbours() {
        let sm = SessionManager::new();
        let mut ids = Vec::new();
        for _ in 0..100 {
            let n = gen_node_id();
            let s = sm.add_est_session(n);
            ids.push(n);
            sm.link_session(n, &s);
        }
        let base = *ids.first().unwrap();
        let neighbours = sm.neighbours(base, 10);
        let v1 = neighbours
            .into_iter()
            .map(|s| hamming_distance(base, s.node_id().unwrap()))
            .collect_vec();
        let mut v2 = ids
            .into_iter()
            .map(|id| hamming_distance(base, id))
            .collect_vec();
        v2.sort();
        assert_eq!(v1, &v2[1..=10]);
    }
}
