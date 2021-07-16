use actix::prelude::*;
use anyhow::{bail, Result};
use rand::Rng;
use std::convert::TryFrom;
use std::fmt;
use std::net::SocketAddr;

use ya_client_model::NodeId;
use ya_relay_proto::proto::{request, Control, Endpoint, Request, Response, SESSION_ID_SIZE};

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
}

#[derive(Clone)]
pub struct NodeInfo {
    pub session: SessionId,
    pub node_id: NodeId,
    pub public_key: Vec<u8>,

    pub address: SocketAddr,
}

pub struct NodeSession {
    pub info: NodeInfo,
    pub endpoints: Vec<Endpoint>,
}

impl NodeSession {
    pub fn new(info: NodeInfo) -> NodeSession {
        NodeSession {
            info,
            endpoints: vec![],
        }
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

impl SessionId {
    pub fn generate() -> SessionId {
        SessionId {
            id: rand::thread_rng().gen::<[u8; SESSION_ID_SIZE]>(),
        }
    }

    pub fn vec(&self) -> Vec<u8> {
        self.id.iter().cloned().collect()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode(self.id))
    }
}
