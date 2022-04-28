use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::RateLimiter;
use rand::Rng;
use std::convert::TryFrom;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::identity::Identity;
use ya_client_model::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::SESSION_ID_SIZE;

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq, Ord)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
}

#[derive(Clone, PartialEq)]
pub struct Endpoint {
    pub protocol: proto::Protocol,
    pub address: SocketAddr,
}

#[derive(Clone)]
pub struct NodeInfo {
    pub identities: Vec<Identity>,
    pub slot: u32,

    /// Endpoints registered by Node.
    pub endpoints: Vec<Endpoint>,
    pub supported_encryptions: Vec<String>,
}

impl NodeInfo {
    pub fn node_id(&self) -> NodeId {
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
pub struct NodeSession {
    pub info: NodeInfo,

    /// Address from which Session was initialized
    pub address: SocketAddr,
    pub session: SessionId,
    pub last_seen: DateTime<Utc>,
    pub forwarding_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
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
        let mut address: SocketAddr = endpoint.address.parse()?;
        address.set_port(endpoint.port as u16);

        Ok(Endpoint {
            protocol: proto::Protocol::from_i32(endpoint.protocol)
                .ok_or_else(|| anyhow!("Invalid protocol enum: {}", endpoint.protocol))?,
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
