use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;
use dashmap::DashMap;
use tokio::time;
use ya_relay_core::challenge::RawChallenge;
use ya_relay_core::NodeId;
use ya_relay_core::server_session::SessionId;
use crate::state::last_seen::{Clock, LastSeen};

type SessionRef = Arc<Session>;

type SessionWeakRef = Weak<Session>;

pub struct Session {
    pub session_id : SessionId,
    pub peer: SocketAddr,
    pub ts: LastSeen,
    pub state: Mutex<SessionState>,
}

pub enum SessionState {
    Pending {
        challenge: RawChallenge,
        difficulty: u64,
    },
    Est {},
    #[cfg(test)]
    Dummy
}

type NodeSessionSet = Arc<Mutex<Vec<SessionWeakRef>>>;

pub struct SessionManager {
    sessions: [Mutex<HashMap<SessionId, SessionRef>>; 16],
    node_sessions : DashMap<NodeId, NodeSessionSet>
}

impl SessionManager {
    pub fn new() -> Arc<Self> {
        let sessions : [Mutex<HashMap<SessionId, SessionRef>>; 16]  = Default::default();
        let node_sessions = Default::default();

        assert_eq!(sessions.len(), 0x10);

        Arc::new(Self { sessions, node_sessions })
    }

    pub fn start_cleanup_processor(self : &Arc<Self>,
                                   session_cleaner_interval: Duration,
                                   session_timeout: Duration,
                                   session_purge_timeout: Duration) {
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = time::interval(session_cleaner_interval);
            loop {
                interval.tick().await;
                let clock = Clock::now();
                let sm = match this.upgrade() {
                    Some(sm) => sm,
                    None => break
                };
                for slot in &sm.sessions {
                    slot.lock().retain(|session_id, session_ref| {
                        clock.age(&session_ref.ts) <= session_purge_timeout
                    })
                }
            }

        });
    }



    pub fn link_session(&self, node_id : NodeId, session: &SessionRef) {
        let session_w = Arc::downgrade(session);
        let entry = self.node_sessions.entry(node_id).or_default();
        let mut g = entry.lock();
        g.retain(|s| s.upgrade().is_some());
        g.push(session_w)
    }

    pub fn node_session(&self, node_id : NodeId) -> Option<SessionRef> {
        if let Some(refs) =  self.node_sessions.get_mut(&node_id) {
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
        clock : &Clock,
        peer: SocketAddr,
        challenge: RawChallenge,
        difficulty: u64,
    ) -> SessionRef {
        let session_id = SessionId::generate();
        let state = Mutex::new(SessionState::Pending {
            challenge,
            difficulty,
        });
        let ts = clock.last_seen();
        let session_ref = Arc::new(Session { session_id, peer, ts, state });

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
        let session_ref = Arc::new(Session { session_id, peer, ts, state });
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

    pub fn remove_session(&self, session:&SessionId) -> Option<SessionRef> {
        self.session_slot(session).lock().remove(session)
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
    use rand::{Rng, thread_rng};
    use ya_relay_core::NodeId;

    #[test]
    fn test_clean_sessions() {
        let sm = SessionManager::new();
        let n1 : NodeId = thread_rng().gen::<[u8;20]>().into();
        let n2 : NodeId = thread_rng().gen::<[u8;20]>().into();
        let n3 : NodeId = thread_rng().gen::<[u8;20]>().into();
        let n4 : NodeId = thread_rng().gen::<[u8;20]>().into();
        let (s1, s2, s3) = (sm.add_dummy_session(), sm.add_dummy_session(), sm.add_dummy_session());
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
        assert!(Arc::ptr_eq(&sm.node_session(n3).unwrap(), &sm.node_session(n4).unwrap()));
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

}