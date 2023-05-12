use anyhow::anyhow;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::RwLock;

use ya_relay_core::identity::Identity;
use ya_relay_core::session::{SessionId, TransportType};
use ya_relay_core::NodeId;
use ya_relay_proto::codec;
use ya_relay_proto::proto::{Payload, SlotId};

use crate::_encryption::Encryption;
use crate::_error::SessionError;
use crate::_session::SystemSession;
use crate::_session_layer::SessionLayer;

/// Describes Node identity.
#[derive(Clone)]
pub struct NodeEntry<IdType> {
    pub default_id: IdType,
    /// TODO: should `identities` vector contain `default_id`?
    pub identities: Vec<IdType>,
}

/// Nodes to SlotIds mapping that should be used to identify forwarding calls.
#[derive(Clone, Default)]
pub struct AllowedForwards {
    pub slots: HashMap<SlotId, NodeEntry<NodeId>>,
    pub nodes: HashMap<NodeId, SlotId>,
}

/// Higher level session abstraction that handles Node identification
/// and public keys and maps it to low-level `Session`.
/// `DirectSession` can be used to forward packets to other Nodes,
/// than session owner.
#[derive(Clone)]
pub struct DirectSession {
    pub owner: NodeEntry<Identity>,
    pub session: Arc<SystemSession>,

    /// Nodes allowed to use this session for forwarding.
    /// In case of Relay Server session this corresponds to all Nodes, that we queried
    /// information about.
    /// In case of p2p session these values will be empty. In future we can use other
    /// sessions than Relay to forward packets.
    pub forwards: Arc<RwLock<AllowedForwards>>,
}

impl DirectSession {
    pub fn new(
        node_id: NodeId,
        identities: impl IntoIterator<Item = Identity>,
        session: Arc<SystemSession>,
    ) -> anyhow::Result<Arc<DirectSession>> {
        let identities = identities.into_iter().collect::<Vec<_>>();
        let default_id = identities
            .iter()
            .find(|ident| ident.node_id == node_id)
            .cloned()
            .ok_or(anyhow!(
                "DirectSession constructor expects default id on identities list."
            ))?;

        Ok(Arc::new(DirectSession {
            owner: NodeEntry {
                default_id,
                identities,
            },
            session,
            forwards: Arc::new(RwLock::new(Default::default())),
        }))
    }
}

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

    pub async fn send(&self, packet: Payload) -> Result<(), SessionError> {
        unimplemented!()
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

    /// Sends Payload to target Node. Creates session if it didn't exist.
    /// `transport` is only declaration which will be used to set flags in
    /// `Forward` packet.
    pub async fn send(
        &mut self,
        packet: Payload,
        transport: TransportType,
    ) -> Result<(), SessionError> {
        unimplemented!()
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
                route.owner.default_id.clone()
            }
        }
        unimplemented!()
    }

    /// Returns id of session used to forward packets.
    pub fn session_id(&self) -> SessionId {
        if let Some(routing) = self.node_routing.upgrade() {
            if let Some(route) = routing.route.upgrade() {
                route.session.id
            }
        }
        unimplemented!()
    }
}
