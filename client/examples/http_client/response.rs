use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use ya_relay_client::model::SessionDesc;

use crate::DisplayableNode;

#[derive(Serialize, Debug)]
pub(crate) struct SessionsResponse {
    sessions: Vec<SessionPayload>,
}

impl fmt::Display for SessionsResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let result = self
            .sessions
            .iter()
            .map(SessionPayload::to_string)
            .collect::<Vec<String>>()
            .join("\n");
        f.write_fmt(format_args!("Sessions:\n{result}\n"))
    }
}

impl From<Vec<SessionDesc>> for SessionsResponse {
    fn from(session_descs: Vec<SessionDesc>) -> Self {
        let sessions = session_descs
            .into_iter()
            .map(SessionPayload::from)
            .collect();
        Self { sessions }
    }
}

#[derive(Serialize, Debug)]
pub(crate) struct SessionPayload {
    address: std::net::IpAddr,
    port: u16,
}

impl fmt::Display for SessionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}:{}", self.address, self.port))
    }
}

impl From<SessionDesc> for SessionPayload {
    fn from(session: SessionDesc) -> Self {
        let address = session.remote.ip();
        let port = session.remote.port();
        Self { address, port }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct PongResponse {
    pub node_id: String,
    pub duration: Duration,
}

impl fmt::Display for PongResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Ping node {} took {} ms",
            self.node_id,
            self.duration.as_millis()
        ))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct TransferResponse {
    pub mb_transfered: usize,
    pub node_id: String,
    pub duration: Duration,
    pub speed: f32,
}

impl fmt::Display for TransferResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Transfer of {} MB to node {} took {} ms which is {} MB/s",
            self.mb_transfered,
            self.node_id,
            self.duration.as_millis(),
            self.speed
        ))
    }
}
