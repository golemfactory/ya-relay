use crate::session::{NodeSession, SessionId};

use ya_relay_proto::codec::PacketKind;
use ya_relay_proto::proto;
use ya_relay_proto::proto::response::Node;

pub trait PacketsCreator {
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
                    endpoints: node_info.info.endpoints.clone(),
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
}

impl PacketsCreator for PacketKind {}
