use actix_web::HttpResponse;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use std::{
    fmt::{self, Display},
    time::Duration,
};
use ya_relay_client::model::SessionDesc;
use ya_relay_core::NodeId;
use ya_relay_proto::proto;

pub(crate) fn ok_json<'a, RESPONSE>(msg: &'a str) -> actix_web::Result<HttpResponse>
where
    RESPONSE: Deserialize<'a> + Serialize + Display,
{
    let msg: RESPONSE = serde_json::from_str(msg)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
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
    pub duration: u128,
}

impl fmt::Display for Pong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Ping node {} took {} ms",
            self.node_id, self.duration
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

#[derive(Serialize)]
pub(crate) struct FindNode {
    pub node: Node,
    pub duration: Duration,
}

impl Display for FindNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Found {} \nin {} ms",
            self.node,
            self.duration.as_millis()
        ))
    }
}

pub(crate) struct Node(pub proto::response::Node);

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identities = self
            .0
            .identities
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let endpoints = self
            .0
            .endpoints
            .iter()
            .map(|ep| {
                format!(
                    "{p}://{ip}:{port}",
                    p = if ep.protocol == i32::from(proto::Protocol::Udp) {
                        "UDP"
                    } else if ep.protocol == i32::from(proto::Protocol::Tcp) {
                        "TCP"
                    } else {
                        "Unknown"
                    },
                    ip = ep.address,
                    port = ep.port
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let slot = self.0.slot;
        let encr = self
            .0
            .supported_encryptions
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        f.write_str(
            format!(
                "Node \
        \n  with Identities: {identities} \
        \n  with Endpoints: {endpoints} \
        \n  with Slot: {slot} \
        \n  with Supported encryption: {encr} \
        \n"
            )
            .as_str(),
        )
    }
}

impl Serialize for Node {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("Node", 5)?;
        let identities: Vec<NodeId> = self
            .0
            .identities
            .iter()
            .map(|id| NodeId::from(id.node_id.as_slice()))
            .collect();
        state.serialize_field("identities", &identities)?;
        let endpoints: Vec<Endpoint> = self
            .0
            .endpoints
            .iter()
            .map(|ep| Endpoint(ep.clone()))
            .collect();
        state.serialize_field("endpoints", &endpoints)?;
        state.serialize_field("seen_ts", &self.0.seen_ts)?;
        state.serialize_field("slot", &self.0.slot)?;
        state.serialize_field("supported_encryptions", &self.0.supported_encryptions)?;
        state.end()
    }
}

struct Endpoint(proto::Endpoint);

impl Serialize for Endpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Endpoint", 5)?;
        state.serialize_field("protocol", &self.0.protocol)?;
        state.serialize_field("address", &self.0.address)?;
        state.serialize_field("port", &self.0.port)?;
        state.end()
    }
}
