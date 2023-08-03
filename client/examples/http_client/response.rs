use actix_web::HttpResponse;
use serde::{Deserialize, Serialize};
use std::{
    fmt::{self, Display},
    time::Duration,
};
use ya_relay_client::model::SessionDesc;

pub(crate) fn json<'a, RESPONSE>(msg: &'a str) -> actix_web::Result<HttpResponse>
where
    RESPONSE: Deserialize<'a> + Serialize + Display,
{
    let msg: RESPONSE = serde_json::from_str(msg)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

pub(crate) fn txt<'a, RESPONSE>(msg: &'a str) -> actix_web::Result<HttpResponse>
where
    RESPONSE: Deserialize<'a> + Serialize + Display,
{
    let msg: RESPONSE = serde_json::from_str(msg)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().body(msg.to_string()))
}

#[derive(Serialize, Debug)]
pub(crate) struct Sessions {
    sessions: Vec<Session>,
}

impl fmt::Display for Sessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let result = self
            .sessions
            .iter()
            .map(Session::to_string)
            .collect::<Vec<String>>()
            .join("\n");
        f.write_fmt(format_args!("Sessions:\n{result}\n"))
    }
}

impl From<Vec<SessionDesc>> for Sessions {
    fn from(session_descs: Vec<SessionDesc>) -> Self {
        let sessions = session_descs.into_iter().map(Session::from).collect();
        Self { sessions }
    }
}

#[derive(Serialize, Debug)]
pub(crate) struct Session {
    address: std::net::IpAddr,
    port: u16,
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}:{}", self.address, self.port))
    }
}

impl From<SessionDesc> for Session {
    fn from(session: SessionDesc) -> Self {
        let address = session.remote.ip();
        let port = session.remote.port();
        Self { address, port }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Pong {
    pub node_id: String,
    pub duration: Duration,
}

impl fmt::Display for Pong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Ping node {} took {} ms",
            self.node_id,
            self.duration.as_millis()
        ))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Transfer {
    pub mb_transfered: usize,
    pub node_id: String,
    pub duration: Duration,
    pub speed: f32,
}

impl fmt::Display for Transfer {
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
