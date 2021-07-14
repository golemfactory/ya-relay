use actix::prelude::*;
use anyhow::{bail, Result};
use rand::Rng;
use std::convert::TryFrom;

use std::fmt;
use ya_relay_proto::proto::{Control, Endpoint, Request, Response, SESSION_ID_SIZE};

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
}

#[derive(Message, Clone)]
#[rtype(result = "Result<()>")]
pub struct RequestPacket(pub Request);

#[derive(Message, Clone)]
#[rtype(result = "Result<()>")]
pub struct ControlPacket(pub Control);

#[derive(Message, Clone)]
#[rtype(result = "Result<()>")]
pub struct ResponsePacket(pub Response);

pub struct NodeSession {
    session: SessionId,
    /// Change to typed NodeId
    node_id: Vec<u8>,
    endpoints: Vec<Endpoint>,
}

impl Actor for NodeSession {
    type Context = Context<Self>;
}

impl TryFrom<Vec<u8>> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: Vec<u8>) -> Result<Self> {
        if session.len() != SESSION_ID_SIZE {
            bail!("Invalid SessionID")
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
        for &byte in self.id.iter() {
            write!(f, "{:X}", byte).expect("Unable to write");
        }
        Ok(())
    }
}

impl Handler<RequestPacket> for NodeSession {
    type Result = ActorResponse<Self, (), anyhow::Error>;

    fn handle(&mut self, msg: RequestPacket, ctx: &mut Context<Self>) -> Self::Result {
        ActorResponse::reply(Ok(()))
    }
}

impl Handler<ControlPacket> for NodeSession {
    type Result = ActorResponse<Self, (), anyhow::Error>;

    fn handle(&mut self, msg: ControlPacket, ctx: &mut Context<Self>) -> Self::Result {
        ActorResponse::reply(Ok(()))
    }
}

impl Handler<ResponsePacket> for NodeSession {
    type Result = ActorResponse<Self, (), anyhow::Error>;

    fn handle(&mut self, msg: ResponsePacket, ctx: &mut Context<Self>) -> Self::Result {
        ActorResponse::reply(Ok(()))
    }
}
