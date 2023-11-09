use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::RateLimiter;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use crate::identity::Identity;
use ya_client_model::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::{SlotId, SESSION_ID_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, derive_more::Display, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportType {
    Unreliable,
    Reliable,
    Transfer,
}

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq, Ord, Serialize, Deserialize)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
}

impl SessionId {
    pub fn to_array(&self) -> [u8; SESSION_ID_SIZE] {
        self.id
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub protocol: proto::Protocol,
    pub address: SocketAddr,
}

#[derive(Clone)]
pub struct NodeInfo {
    pub identities: Vec<Identity>,
    pub slot: SlotId,

    /// Endpoints registered by Node.
    pub endpoints: Vec<Endpoint>,
    pub supported_encryption: Vec<String>,
}

impl NodeInfo {
    pub fn default_node_id(&self) -> NodeId {
        self.identities.get(0).map(|ident| ident.node_id).unwrap()
    }

    pub fn public_key(&self) -> Vec<u8> {
        self.identities
            .get(0)
            .map(|ident| ident.public_key.bytes().to_vec())
            .unwrap()
    }
}

#[derive(Clone)]
pub struct LastSeen {
    last_seen: Arc<Mutex<DateTime<Utc>>>,
}

#[derive(Clone)]
pub struct RequestHistory {
    capacity: usize,
    ids: Arc<RwLock<VecDeque<u64>>>,
}

#[derive(Clone)]
pub struct NodeSession {
    pub info: NodeInfo,

    /// Address from which Session was initialized
    pub address: SocketAddr,
    pub session: SessionId,
    pub last_seen: LastSeen,
    pub forwarding_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,

    /// Request IDs of the last n requests
    pub request_history: RequestHistory,
}

impl From<DateTime<Utc>> for LastSeen {
    fn from(datetime: DateTime<Utc>) -> Self {
        LastSeen {
            last_seen: Arc::new(Mutex::new(datetime)),
        }
    }
}

impl LastSeen {
    pub fn now() -> LastSeen {
        LastSeen::from(Utc::now())
    }

    pub fn update(&self, datetime: DateTime<Utc>) {
        *self.last_seen.lock().unwrap() = datetime
    }

    pub fn time(&self) -> DateTime<Utc> {
        *self.last_seen.lock().unwrap()
    }
}

impl RequestHistory {
    pub fn new(capacity: usize) -> Self {
        RequestHistory {
            capacity,
            ids: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub fn push(&self, request_id: u64) {
        let mut ids = self.ids.write().unwrap();

        if ids.len() == self.capacity {
            ids.pop_front();
        }

        ids.push_back(request_id);
    }

    pub fn contains(&self, request_id: u64) -> bool {
        self.ids.read().unwrap().contains(&request_id)
    }
}

impl TryFrom<Vec<u8>> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: Vec<u8>) -> Result<Self> {
        if session.len() != SESSION_ID_SIZE {
            bail!("Invalid SessionID: {}", String::from_utf8(session)?)
        }

        let mut id: [u8; SESSION_ID_SIZE] = [0; SESSION_ID_SIZE];
        session[0..SESSION_ID_SIZE]
            .iter()
            .enumerate()
            .for_each(|(i, s)| id[i] = *s);

        Ok(SessionId { id })
    }
}

impl TryFrom<&[u8]> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: &[u8]) -> Result<Self> {
        let id: [u8; SESSION_ID_SIZE] = session.try_into()?;

        Ok(SessionId { id })
    }
}

impl TryFrom<&str> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: &str) -> Result<Self> {
        SessionId::try_from(hex::decode(session)?)
    }
}

impl From<[u8; SESSION_ID_SIZE]> for SessionId {
    fn from(array: [u8; SESSION_ID_SIZE]) -> Self {
        SessionId { id: array }
    }
}

impl From<SessionId> for [u8; SESSION_ID_SIZE] {
    fn from(session: SessionId) -> [u8; SESSION_ID_SIZE] {
        session.id
    }
}

impl SessionId {
    pub fn generate() -> SessionId {
        SessionId {
            id: rand::thread_rng().gen::<[u8; SESSION_ID_SIZE]>(),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.id.to_vec()
    }
}

impl<'a> PartialEq<&'a [u8]> for SessionId {
    fn eq(&self, other: &&'a [u8]) -> bool {
        &self.id[..] == *other
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode(self.id))
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.id))
    }
}

impl TryFrom<proto::Endpoint> for Endpoint {
    type Error = anyhow::Error;

    fn try_from(endpoint: proto::Endpoint) -> Result<Self> {
        let address = format!("{}:{}", endpoint.address, endpoint.port);
        let address: SocketAddr = address.parse()?;

        Ok(Endpoint {
            protocol: endpoint
                .protocol
                .try_into()
                .map_err(|_| anyhow!("Invalid protocol enum: {}", endpoint.protocol))?,
            address,
        })
    }
}

impl From<Endpoint> for proto::Endpoint {
    fn from(endpoint: Endpoint) -> Self {
        proto::Endpoint {
            protocol: endpoint.protocol as i32,
            address: endpoint.address.ip().to_string(),
            port: endpoint.address.port() as u32,
        }
    }
}

impl TryFrom<proto::response::Node> for NodeInfo {
    type Error = anyhow::Error;

    fn try_from(value: proto::response::Node) -> std::result::Result<Self, Self::Error> {
        let identities = value
            .identities
            .iter()
            .map(Identity::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(NodeInfo {
            identities,
            slot: value.slot,
            endpoints: value
                .endpoints
                .into_iter()
                .map(Endpoint::try_from)
                .collect::<anyhow::Result<Vec<_>>>()?,
            supported_encryption: value.supported_encryptions,
        })
    }
}
