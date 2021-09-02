use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use rand::Rng;
use std::convert::TryFrom;
use std::fmt;
use std::net::SocketAddr;

use ya_client_model::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::SESSION_ID_SIZE;

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq)]
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
    pub node_id: NodeId,
    pub public_key: Vec<u8>,
    pub slot: u32,

    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone)]
pub struct NodeSession {
    pub info: NodeInfo,

    pub session: SessionId,
    pub last_seen: DateTime<Utc>,
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
