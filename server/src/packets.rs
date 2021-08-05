use crate::session::{NodeSession, SessionId};

use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::response::Node;

pub trait PacketsCreator {
    fn bad_request(session_id: Vec<u8>) -> PacketKind {
        Self::error(session_id, proto::StatusCode::BadRequest)
    }

    fn error(session_id: Vec<u8>, code: proto::StatusCode) -> PacketKind {
        PacketKind::Packet(proto::Packet {
            session_id,
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: code as i32,
                kind: None,
            })),
        })
    }

    fn session(packet: &PacketKind) -> Vec<u8> {
        match packet {
            PacketKind::Packet(proto::Packet { session_id, .. }) => session_id.clone(),
            PacketKind::Forward(proto::Forward { session_id, .. }) => session_id.to_vec(),
            PacketKind::ForwardCtd(_) => vec![],
        }
    }

    fn node_response(
        session_id: SessionId,
        node_info: NodeSession,
        public_key: bool,
    ) -> PacketKind {
        let public_key = match public_key {
            true => node_info.info.public_key,
            false => vec![],
        };

        PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: proto::StatusCode::Ok as i32,
                kind: Some(proto::response::Kind::Node(Node {
                    node_id: node_info.info.node_id.into_array().to_vec(),
                    public_key,
                    endpoints: node_info
                        .info
                        .endpoints
                        .into_iter()
                        .map(proto::Endpoint::from)
                        .collect(),
                    seen_ts: node_info.last_seen.timestamp() as u32,
                    slot: 0,
                    random: false,
                })),
            })),
        })
    }

    fn pong_response(session_id: SessionId) -> PacketKind {
        PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: proto::StatusCode::Ok as i32,
                kind: Some(proto::response::Kind::Pong(proto::response::Pong {})),
            })),
        })
    }

    fn session_response(session_id: SessionId) -> PacketKind {
        PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: proto::StatusCode::Ok as i32,
                kind: Some(proto::response::Kind::Session(proto::response::Session {})),
            })),
        })
    }

    fn register_response(session_id: SessionId, endpoints: Vec<proto::Endpoint>) -> PacketKind {
        PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Response(proto::Response {
                code: proto::StatusCode::Ok as i32,
                kind: Some(proto::response::Kind::Register(proto::response::Register {
                    endpoints,
                })),
            })),
        })
    }

    fn ping_packet(session_id: SessionId) -> PacketKind {
        PacketKind::Packet(proto::Packet {
            session_id: session_id.vec(),
            kind: Some(proto::packet::Kind::Request(proto::Request {
                kind: Some(proto::request::Kind::Ping(proto::request::Ping {})),
            })),
        })
    }
}

impl PacketsCreator for PacketKind {}

pub fn dispatch_response(packet: PacketKind) -> Result<proto::response::Kind, proto::StatusCode> {
    match packet {
        PacketKind::Packet(proto::Packet {
            kind: Some(proto::packet::Kind::Response(proto::Response { kind, code })),
            ..
        }) => match kind {
            None => {
                Err(proto::StatusCode::from_i32(code as i32).ok_or(proto::StatusCode::Undefined)?)
            }
            Some(response) => {
                match proto::StatusCode::from_i32(code as i32) {
                    Some(proto::StatusCode::Ok) => Ok(response),
                    _ => Err(proto::StatusCode::from_i32(code as i32)
                        .ok_or(proto::StatusCode::Undefined)?),
                }
            }
        },
        _ => Err(proto::StatusCode::Undefined),
    }
}
