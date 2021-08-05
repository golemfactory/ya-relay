use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rand::Rng;
use std::convert::TryFrom;
use std::fmt;

use ya_client_model::NodeId;
use ya_relay_proto::proto::{Endpoint, SESSION_ID_SIZE};

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
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

impl SessionId {
    pub fn generate() -> SessionId {
        SessionId {
            id: rand::thread_rng().gen::<[u8; SESSION_ID_SIZE]>(),
        }
    }

    pub fn vec(&self) -> Vec<u8> {
        self.id.to_vec()
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
