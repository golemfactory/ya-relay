#![allow(dead_code)]
#![allow(unused)]

use anyhow::{anyhow, bail};
use derive_more::Display;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::RwLock;

use ya_relay_core::identity::Identity;
use ya_relay_core::session::{SessionId, TransportType};
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Payload, SlotId};

use crate::_direct_session::{DirectSession, NodeEntry};
use crate::_encryption::Encryption;
use crate::_error::SessionError;
use crate::_session::{RawSession, SessionType};
use crate::_session_layer::SessionLayer;

/// Routing information about Node. Node can have either p2p session or relayed session.
/// This struct hides `DirectSession` choice from caller.
///
/// Nodes are using always their default NodeId to communicate. Secondary ids will be resolved
/// to defaults, so each node will have the same entry for all identities.
///
/// TODO: Encryption should be implemented on this layer, since we have access to public
///       key of destination Node.
#[derive(Clone)]
pub struct NodeRouting {
    pub node: NodeEntry<Identity>,

    /// If `NodeRouting` is relayed session, than we have Relay Server `DirectSession` here.
    /// `DirectSession` contains all info (for example SlotID) required to send packets using this session.  
    pub route: Weak<DirectSession>,
    encryption: Encryption,
}

impl NodeRouting {
    pub fn new(
        node: NodeEntry<Identity>,
        session: Arc<DirectSession>,
        encryption: Encryption,
    ) -> Arc<NodeRouting> {
        Arc::new(NodeRouting {
            node,
            route: Arc::downgrade(&session),
            encryption,
        })
    }

    /// `transport` is only declaration which will be used to set flags in
    /// `Forward` packet.
    pub async fn send(
        &self,
        packet: Payload,
        transport: TransportType,
    ) -> Result<(), SessionError> {
        if let Some(direct) = self.route.upgrade() {
            let packet = self
                .encryption
                .encrypt(packet)
                .await
                .map_err(|e| SessionError::Internal(e.to_string()))?;
            direct
                .send(self.node.default_id.node_id, packet, transport, false)
                .await
                .map_err(|e| {
                    SessionError::Network(format!("Sending packet to p2p routing session: {e}"))
                })?;
            return Ok(());
        }
        Err(SessionError::Unexpected(
            "Routing session closed unexpectedly.".to_string(),
        ))
    }
}

/// Interface structure for sending packets to other Nodes.
///
/// Underlying sessions can be closed and reopened, routing can change, but this struct should
/// give stable interface for other layers to communicate between 2 Nodes.
///
/// Struct has direct access to `NodeRouting` to avoid acquiring to many locks.
/// `NodeRouting`, `DirectSession` and `Session` are mostly read-only structs and will be replaced
/// in case connection will change.
#[derive(Clone)]
pub struct RoutingSender {
    /// This can be either secondary or default id.
    target: NodeId,
    node_routing: Weak<NodeRouting>,
    layer: SessionLayer,
}

impl RoutingSender {
    /// Create `RoutingSender` without initialized connection. It will be
    /// create later on demand.
    pub fn empty(target: NodeId, layer: SessionLayer) -> RoutingSender {
        RoutingSender {
            target,
            node_routing: Weak::new(),
            layer,
        }
    }

    pub fn from_node_routing(
        node: NodeId,
        target: Arc<NodeRouting>,
        layer: SessionLayer,
    ) -> RoutingSender {
        RoutingSender {
            target: node,
            node_routing: Arc::downgrade(&target),
            layer,
        }
    }

    /// Sends Payload to target Node. Creates session if it didn't exist.
    /// `transport` is only declaration which will be used to set flags in
    /// `Forward` packet.
    pub async fn send(
        &mut self,
        packet: Payload,
        transport: TransportType,
    ) -> Result<(), SessionError> {
        let routing = match self.node_routing.upgrade() {
            Some(routing) => routing,
            None => match self
                .layer
                .session(self.target)
                .await?
                .node_routing
                .upgrade()
            {
                Some(routing) => routing,
                None => {
                    return Err(SessionError::Unexpected(
                        "Routing session closed unexpectedly.".to_string(),
                    ))
                }
            },
        };
        routing.send(packet, transport).await
    }

    /// Establishes connection on demand if it didn't exist.
    /// This function can be used to prepare connection before later use.
    /// Thanks to this we can return early if Node is unreachable in the network.
    ///
    /// Calling this function doesn't guarantee, that `RoutingSender::send` won't require
    /// waiting for session. Connection can be lost again before we call `send.
    pub async fn connect(&mut self) -> Result<(), SessionError> {
        unimplemented!()
    }

    pub fn target(&self) -> NodeId {
        self.target
    }

    pub fn route(&self) -> NodeId {
        if let Some(routing) = self.node_routing.upgrade() {
            if let Some(route) = routing.route.upgrade() {
                return route.owner.default_id;
            }
        }
        unimplemented!()
    }

    /// Returns id of session used to forward packets.
    pub fn session_id(&self) -> SessionId {
        if let Some(routing) = self.node_routing.upgrade() {
            if let Some(route) = routing.route.upgrade() {
                return route.raw.id;
            }
        }
        unimplemented!()
    }

    pub fn session_type(&self) -> SessionType {
        if let Some(routing) = self.node_routing.upgrade() {
            if let Some(route) = routing.route.upgrade() {
                return match routing.node.default_id.node_id == route.owner.default_id {
                    true => SessionType::P2P,
                    false => SessionType::Relay,
                };
            }
        }
        unimplemented!()
    }
}

impl From<NodeEntry<Identity>> for NodeEntry<NodeId> {
    fn from(value: NodeEntry<Identity>) -> Self {
        NodeEntry::<NodeId> {
            default_id: value.default_id.node_id,
            identities: value.identities.into_iter().map(|id| id.node_id).collect(),
        }
    }
}
