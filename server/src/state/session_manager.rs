use crate::state::hamming_distance;
use crate::state::last_seen::{Clock, LastSeen};
use crate::state::session_manager::metrics::SessionManagerMetrics;
use anyhow::{anyhow, Context};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use std::{cmp, fs, io, iter, thread};
use tokio::time;
use ya_relay_core::crypto::PublicKey;
use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::Endpoint;
use ya_relay_proto::proto::Protocol::Udp;

#[derive(clap::Args)]
#[command(next_help_heading = "Session manager options")]
pub struct SessionManagerConfig {
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10s")]
    pub session_cleaner_interval: Duration,
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10min")]
    pub session_purge_timeout: Duration,
}

mod metrics {
    use metrics::{recorder, Counter, Gauge, Histogram, Key};

    static SESSIONS: Key = Key::from_static_name("ya-relay.session");

    static NODES: Key = Key::from_static_name("ya-relay.session.nodes");
    static CREATED: Key = Key::from_static_name("ya-relay.session.created");
    static REMOVED: Key = Key::from_static_name("ya_relay.session.removed");

    static PURGED: Key = Key::from_static_name("ya_relay.session.purged");

    static PROCESSING: Key = Key::from_static_name("ya-relay.session.cleaner.processing-time");

    pub struct SessionManagerMetrics {
        pub created: Counter,
        pub removed: Counter,
        pub purged: Counter,
        pub sessions: Gauge,
        pub nodes: Gauge,
        pub processing: Histogram,
    }

    impl Default for SessionManagerMetrics {
        fn default() -> Self {
            let r = recorder();
            let created = r.register_counter(&CREATED);
            let removed = r.register_counter(&REMOVED);
            let purged = r.register_counter(&PURGED);
            let sessions = r.register_gauge(&SESSIONS);
            let nodes = r.register_gauge(&NODES);
            let processing = r.register_histogram(&PROCESSING);

            Self {
                created,
                removed,
                purged,
                sessions,
                nodes,
                processing,
            }
        }
    }
}

pub type SessionRef = Arc<Session>;

pub type SessionWeakRef = Weak<Session>;

pub enum Selector {
    All,
    Prefix { value: u32, mask: u32 },
}

impl FromStr for Selector {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "all" {
            return Ok(Selector::All);
        }

        let v = u32::from_str_radix(s, 16)?;
        let bits = s.len() * 4;
        let value = v << (32 - bits);
        let mask = u32::MAX << (32 - bits);
        Ok(Selector::Prefix { value, mask })
    }
}

trait PrefixExt {
    fn extract_prefix(&self) -> u32;
}

impl PrefixExt for SessionId {
    fn extract_prefix(&self) -> u32 {
        let a = self.to_array();
        u32::from_be_bytes(a[0..4].try_into().unwrap())
    }
}

impl PrefixExt for NodeId {
    fn extract_prefix(&self) -> u32 {
        let a = self.into_array();
        u32::from_be_bytes(a[0..4].try_into().unwrap())
    }
}

impl Selector {
    fn match_prefix<T: PrefixExt>(&self, v: T) -> bool {
        match self {
            Self::All => true,
            Self::Prefix { value, mask } => (v.extract_prefix() & *mask) == *value,
        }
    }
}

pub struct Session {
    pub session_id: SessionId,
    pub peer: SocketAddr,
    pub ts: LastSeen,
    pub node_id: NodeId,
    pub keys: Vec<Identity>,
    pub supported_encryptions: Vec<String>,
    pub addr_status: Mutex<AddrStatus>,
}

#[derive(Serialize, Deserialize)]
struct PubKey {
    #[serde(with = "serde_bytes_array")]
    inner: [u8; 64],
}

impl<'a> From<&'a Identity> for PubKey {
    fn from(value: &'a Identity) -> Self {
        let inner = value.public_key.bytes().clone();
        Self { inner }
    }
}

impl PubKey {
    fn decode(&self) -> Identity {
        let public_key = PublicKey::from_slice(&self.inner).unwrap();
        let node_id = NodeId::from(*public_key.address());
        Identity {
            node_id,
            public_key,
        }
    }
}

mod serde_bytes_array {
    use core::convert::TryInto;
    use std::borrow::Cow;

    use serde::de::Error;
    use serde::{Deserializer, Serializer};

    /// This just specializes [`serde_bytes::serialize`] to `<T = [u8]>`.
    pub(crate) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde_bytes::serialize(bytes, serializer)
    }

    /// This takes the result of [`serde_bytes::deserialize`] from `[u8]` to `[u8; N]`.
    pub(crate) fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let slice: Cow<'_, [u8]> = serde_bytes::deserialize(deserializer)?;
        let array: [u8; N] = slice.as_ref().try_into().map_err(|_| {
            let expected = format!("[u8; {}]", N);
            D::Error::invalid_length(slice.len(), &expected.as_str())
        })?;
        Ok(array)
    }
}

// WARN: Never change this struct.
#[derive(Serialize, Deserialize)]
struct SessionData {
    session_id: SessionId,
    peer: SocketAddr,
    session_key: Option<PubKey>,
    keys: Vec<PubKey>,
    supported_encryptions: Vec<String>,
    addr_valid: bool,
    flags: u64,
}

impl Session {
    pub fn endpoint(&self) -> Option<Endpoint> {
        match &*self.addr_status.lock() {
            AddrStatus::Valid(_) => Some(Endpoint {
                protocol: Udp.into(),
                address: self.peer.ip().to_string(),
                port: self.peer.port().into(),
            }),
            _ => None,
        }
    }
}

pub enum AddrStatus {
    Unknown,
    Pending(Instant),
    Valid(Instant),
    Invalid(Instant),
}

impl AddrStatus {
    pub fn is_pending(&self) -> bool {
        matches!(self, AddrStatus::Unknown | AddrStatus::Pending(_))
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        matches!(self, AddrStatus::Valid(_))
    }

    pub fn set_valid(&mut self, valid: bool) {
        *self = if valid {
            AddrStatus::Valid(Instant::now())
        } else {
            AddrStatus::Invalid(Instant::now())
        }
    }

    pub fn age(&self) -> Duration {
        match self {
            AddrStatus::Valid(v) => v.elapsed(),
            AddrStatus::Invalid(v) => v.elapsed(),
            _ => Duration::default(),
        }
    }
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

    pub fn num_sessions(&self) -> usize {
        self.sessions.iter().map(|s| s.lock().len()).sum()
    }

    pub fn nodes_for(
        &self,
        selector: Selector,
        limit: usize,
    ) -> HashMap<NodeId, Vec<SessionWeakRef>> {
        self.node_sessions
            .iter()
            .filter(|e| selector.match_prefix(*e.key()))
            .map(|e| (*e.key(), e.value().lock().clone()))
            .take(limit)
            .collect()
    }

    pub fn start_cleanup_processor(
        self: &Arc<Self>,
        &SessionManagerConfig {
            session_cleaner_interval,
            session_purge_timeout,
            ..
        }: &SessionManagerConfig,
    ) {
        let g_nodes = self.metrics.nodes.clone();
        let g_sessions = self.metrics.sessions.clone();

        let this = Arc::downgrade(self);
        log::info!("start {:?}", thread::current().id());
        tokio::spawn(async move {
            log::info!("spawn {:?}", thread::current().id());
            loop {
                let start = Instant::now();
                log::debug!("clean wait");
                time::sleep(session_cleaner_interval).await;
                log::debug!("clean start {:?}", session_purge_timeout);
                let clock = Clock::now();
                let sm = match this.upgrade() {
                    Some(sm) => sm,
                    None => break,
                };
                //log::debug!("total = {}", sm.sessions.iter().map(|shard| shard.lock().len()).sum::<usize>());

                let mut total_clean = 0;
                let mut total_size = 0;
                for slot in &sm.sessions {
                    let mut g = slot.lock();
                    let start_size = g.len();
                    g.retain(|_session_id, session_ref| {
                        let age = clock.age(&session_ref.ts);

                        age <= session_purge_timeout
                    });
                    //fence(Ordering::AcqRel);
                    let end_size = g.len();
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
                log::debug!("clean end: {total_clean}/{}", total_size + total_clean);
                g_sessions.set(total_size as f64);
                sm.clean_node_sessions();
                g_nodes.set(sm.node_sessions.len() as f64);
                sm.metrics.processing.record(start.elapsed());
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
            fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for Distance {
            fn cmp(&self, other: &Self) -> cmp::Ordering {
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

    pub fn link_sessions(&self, session: &SessionRef) {
        let session_w = Arc::downgrade(session);
        for id in &session.keys {
            let entry = self.node_sessions.entry(id.node_id).or_default();
            let mut g = entry.lock();
            g.retain(|s| s.strong_count() > 0);
            if g.iter().all(|s| !Weak::ptr_eq(s, &session_w)) {
                g.push(session_w.clone())
            }
        }
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
        self.node_sessions.retain(|&_node_id, sessions| {
            let mut g = sessions.lock();
            g.retain(|s| s.upgrade().is_some());
            !g.is_empty()
        })
    }

    pub fn new_session(
        &self,
        clock: &Clock,
        session_id: SessionId,
        peer: SocketAddr,
        node_id: NodeId,
        keys: Vec<Identity>,
        supported_encryptions: Vec<String>,
    ) -> Result<SessionRef, SessionRef> {
        let addr_status = Mutex::new(AddrStatus::Unknown);
        let ts = clock.last_seen();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            node_id,
            keys,
            supported_encryptions,
            addr_status,
        });

        let mut g = self.session_slot(&session_id).lock();
        let prev = g.insert(session_id, session_ref.clone());
        if let Some(prev) = prev {
            g.insert(session_id, prev.clone());
            drop(g);
            Err(prev)
        } else {
            drop(g);
            self.metrics.created.increment(1);
            Ok(session_ref)
        }
    }

    #[cfg(test)]
    fn add_dummy_session(&self) -> SessionRef {
        let session_id = SessionId::generate();
        let ts = LastSeen::now();
        let peer = "127.0.0.1:40".parse().unwrap();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            node_id: Default::default(),
            keys: vec![],
            supported_encryptions: vec![],
            addr_status: Mutex::new(AddrStatus::Unknown),
        });
        self.session_slot(&session_id)
            .lock()
            .insert(session_id, session_ref.clone());
        session_ref
    }

    #[cfg(test)]
    fn add_est_session(&self, node_id: NodeId) -> SessionRef {
        let session_id = SessionId::generate();
        let ts = LastSeen::now();
        let peer = "127.0.0.1:40".parse().unwrap();
        let session_ref = Arc::new(Session {
            session_id,
            peer,
            ts,
            node_id,
            keys: Default::default(),
            supported_encryptions: Default::default(),
            addr_status: Mutex::new(AddrStatus::Unknown),
        });
        self.session_slot(&session_id)
            .lock()
            .insert(session_id, session_ref.clone());
        session_ref
    }

    pub fn session(&self, session_id: &SessionId) -> Option<SessionRef> {
        self.session_slot(session_id)
            .lock()
            .get(session_id)
            .cloned()
    }

    pub fn with_session<Out, F: FnOnce(&Session) -> Out>(
        &self,
        session_id: &SessionId,
        f: F,
    ) -> Option<Out> {
        self.session(session_id).map(|session_ref| f(&session_ref))
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

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut f = io::BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?,
        );

        for shard in &self.sessions {
            for s in shard.lock().values() {
                let session_data = SessionData {
                    session_id: s.session_id,
                    peer: s.peer,
                    session_key: None,
                    flags: 0,
                    supported_encryptions: s.supported_encryptions.clone(),
                    keys: s.keys.iter().map(Into::into).collect(),
                    addr_valid: s.addr_status.lock().is_valid(),
                };
                rmp_serde::encode::write(&mut f, &session_data)?;
            }
        }
        f.flush()?;

        Ok(())
    }

    pub fn load(path: &Path) -> anyhow::Result<Arc<Self>> {
        let mut f = io::BufReader::new(fs::OpenOptions::new().read(true).open(path)?);

        let me = Self::new();

        while has_data(&mut f)? {
            let node_info: SessionData =
                rmp_serde::decode::from_read(&mut f).context("decoding state")?;
            let addr_status = if node_info.addr_valid {
                AddrStatus::Valid(Instant::now())
            } else {
                AddrStatus::Invalid(Instant::now())
            };
            let keys: Vec<_> = node_info.keys.iter().map(PubKey::decode).collect();

            let session = Arc::new(Session {
                session_id: node_info.session_id,
                peer: node_info.peer,
                ts: LastSeen::now(),
                node_id: keys.first().ok_or_else(|| anyhow!("invalid data"))?.node_id,
                keys,
                supported_encryptions: node_info.supported_encryptions,
                addr_status: Mutex::new(addr_status),
            });
            me.session_slot(&session.session_id)
                .lock()
                .insert(session.session_id, session.clone());
            me.link_sessions(&session);
        }

        Ok(me)
    }
}

fn has_data<R: BufRead>(r: &mut R) -> io::Result<bool> {
    Ok(!r.fill_buf()?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{thread_rng, Rng};
    use std::io;
    use ya_relay_core::NodeId;

    fn gen_node_id() -> NodeId {
        thread_rng().gen::<[u8; 20]>().into()
    }

    #[test_log::test]
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

    #[test_log::test]
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
            .map(|s| hamming_distance(base, s.node_id))
            .collect::<Vec<_>>();
        let mut v2 = ids
            .into_iter()
            .map(|id| hamming_distance(base, id))
            .collect::<Vec<_>>();
        v2.sort();
        assert_eq!(v1, &v2[1..=10]);
    }

    #[test_log::test]
    fn test_save_load() {
        let mut buffer = Vec::new();

        for _ in 0..15 {
            let session_id = SessionId::generate();
            let peer = "127.0.0.1:40".parse().unwrap();
            let inner = [0u8; 64];
            let key = PubKey { inner };

            let data = SessionData {
                session_id,
                peer,
                session_key: None,
                keys: vec![key],
                supported_encryptions: vec![],
                addr_valid: false,
                flags: 0,
            };
            rmp_serde::encode::write(&mut buffer, &data).unwrap();
        }
        log::info!("{} bytes written", buffer.len());
        let mut c = io::Cursor::new(&buffer);
        while has_data(&mut c).unwrap() {
            let data: SessionData = rmp_serde::decode::from_read(&mut c).unwrap();
            log::info!("decoded {}", data.session_id);
        }
    }
}
