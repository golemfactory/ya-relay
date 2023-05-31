#![allow(dead_code)]
#![allow(unused)]

use anyhow::{anyhow, bail};
use derive_more::Display;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::RwLock;

use ya_relay_core::identity::Identity;
use ya_relay_core::session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::SlotId;

use crate::_encryption::Encryption;
use crate::_error::SessionError;
use crate::_session::{RawSession, SessionType};

/// Describes Node identity.
/// `IdType` Could be either plain `NodeId` or `Identity` structure containing
/// public key.
#[derive(Clone)]
pub struct NodeEntry<IdType> {
    pub default_id: IdType,
    /// Vector contains all identities including default.
    pub identities: Vec<IdType>,
}

/// Nodes to SlotIds mapping that should be used to identify forwarding calls.
/// TODO: Consider storing here `NodeRouting`. This would save us a need to query
///       this struct based on NodeId from `SessionLayer` when receiving packets.
///       Only `NodeRouting` contains information necessary for decrypting packets.
#[derive(Clone, Default)]
pub struct AllowedForwards {
    slots: HashMap<SlotId, NodeEntry<NodeId>>,
    nodes: HashMap<NodeId, SlotId>,
}

impl AllowedForwards {
    pub fn add(&mut self, node: NodeEntry<NodeId>, slot: SlotId) {
        // Remove previous information about node.
        // We are removing all identities and slot. This is redundant, because in most cases
        // using default id should be enough. This protects from situations, when `NodeEntries`
        // for different Nodes overlap. In theory it shouldn't happen, because only one Node should
        // have access to private key, but I think it's better to ensure data consistency here.
        self.remove_by_slot(slot);
        self.remove(&node.default_id);
        for id in &node.identities {
            self.remove(id);
        }

        for id in &node.identities {
            self.nodes.insert(*id, slot);
        }
        self.slots.insert(slot, node);
    }

    pub fn remove(&mut self, node_id: &NodeId) -> Option<NodeEntry<NodeId>> {
        if let Some(slot) = self.nodes.remove(&node_id) {
            if let Some(entry) = self.slots.remove(&slot) {
                for id in &entry.identities {
                    self.nodes.remove(id);
                }
                return Some(entry);
            }
        }
        None
    }

    pub fn remove_by_slot(&mut self, slot: SlotId) -> Option<NodeId> {
        if let Some(default_id) = self.slots.get(&slot).map(|node| node.default_id) {
            self.remove(&default_id);
            return Some(default_id);
        }
        None
    }

    pub fn get_by_slot(&self, slot: SlotId) -> Option<NodeEntry<NodeId>> {
        self.slots.get(&slot).cloned()
    }

    pub fn get_by_id(&self, node_id: &NodeId) -> Option<NodeEntry<NodeId>> {
        if let Some(slot) = self.nodes.get(&node_id) {
            return self.get_by_slot(*slot);
        }
        None
    }
}

/// Higher level session abstraction that handles Node identification
/// and public keys and maps it to low-level `Session`.
/// `DirectSession` can be used to forward packets to other Nodes,
/// than session owner.
#[derive(Clone)]
pub struct DirectSession {
    /// I would prefer to use `NodeEntry<Identity>` here, but current implementation of
    /// relay server doesn't provide public key.
    pub owner: NodeEntry<NodeId>,
    pub raw: Arc<RawSession>,

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
        session: Arc<RawSession>,
    ) -> anyhow::Result<Arc<DirectSession>> {
        let identities = identities
            .into_iter()
            .map(|identity| identity.node_id)
            .collect::<Vec<_>>();
        let default_id = identities
            .iter()
            .find(|ident| ident == &&node_id)
            .cloned()
            .ok_or(anyhow!(
                "DirectSession constructor expects default id on identities list."
            ))?;

        Ok(Arc::new(DirectSession {
            owner: NodeEntry {
                default_id,
                identities,
            },
            raw: session,
            forwards: Arc::new(RwLock::new(Default::default())),
        }))
    }

    /// Relay doesn't return identities list, so normal function for creating
    /// `DirectSession` won't work. We should remove differences between p2p sessions
    /// and relay session. In the future this function should disappear.
    pub fn new_relay(
        node_id: NodeId,
        session: Arc<RawSession>,
    ) -> anyhow::Result<Arc<DirectSession>> {
        Ok(Arc::new(DirectSession {
            owner: NodeEntry {
                default_id: node_id,
                identities: vec![],
            },
            raw: session,
            forwards: Arc::new(RwLock::new(Default::default())),
        }))
    }

    pub async fn remove_by_slot(&self, id: SlotId) -> anyhow::Result<NodeId> {
        let mut forwards = self.forwards.write().await;
        if let Some(node_id) = forwards.remove_by_slot(id) {
            return Ok(node_id);
        }
        bail!(
            "Slot {id} not found in session: {} ({})",
            self.raw.id,
            self.owner.default_id
        )
    }

    pub async fn remove(&self, node_id: &NodeId) -> anyhow::Result<NodeEntry<NodeId>> {
        let mut forwards = self.forwards.write().await;
        match forwards.remove(&node_id) {
            Some(node_id) => Ok(node_id),
            None => bail!(
                "Node {node_id} not found in session: {} ({})",
                self.raw.id,
                self.owner.default_id
            ),
        }
    }

    pub async fn register(&self, node: NodeEntry<NodeId>, slot: SlotId) {
        let mut forwards = self.forwards.write().await;
        forwards.add(node, slot)
    }

    pub async fn get_by_slot(&self, slot: SlotId) -> Option<NodeEntry<NodeId>> {
        let mut forwards = self.forwards.read().await;
        forwards.get_by_slot(slot)
    }

    pub async fn get_by_id(&self, node_id: &NodeId) -> Option<NodeEntry<NodeId>> {
        let mut forwards = self.forwards.read().await;
        forwards.get_by_id(node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ya_relay_proto::codec::PacketKind;

    use lazy_static::lazy_static;
    use std::net::SocketAddr;
    use std::str::FromStr;

    lazy_static! {
        static ref NODE_ID0: NodeId =
            NodeId::from_str("0x0000000000000000000000000000000000000000").unwrap();
        static ref NODE_ID1: NodeId =
            NodeId::from_str("0xf3a6a007194f987ad3f6db45da2f3f7ab1332c86").unwrap();
        static ref NODE_ID2: NodeId =
            NodeId::from_str("0x22e0a9f42cc335f660c8738849d8df59d3a175a7").unwrap();
        static ref NODE_ID3: NodeId =
            NodeId::from_str("0x84d7918c49ec6cfbb236d819d543db618e90aa9c").unwrap();
        static ref NODE_ID4: NodeId =
            NodeId::from_str("0xffba21c81f6318319206209868ab7d33b8a72a07").unwrap();
    }

    fn mock_session() -> Arc<DirectSession> {
        let addr = SocketAddr::from_str("127.0.0.1:8000").unwrap();
        let (sink, _) = futures::channel::mpsc::channel::<(PacketKind, SocketAddr)>(1);

        let raw = RawSession::new(addr, SessionId::generate(), sink);
        DirectSession::new_relay(*NODE_ID0, raw).unwrap()
    }

    #[tokio::test]
    async fn test_direct_session_forwards_add_remove_entry() {
        let mut session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1],
        };

        session.register(node.clone(), 4).await;

        let entry = session.get_by_slot(4).await.unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 1);
        assert_eq!(entry.identities[0], node.identities[0]);

        let entry = session.get_by_id(&*NODE_ID1).await.unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 1);
        assert_eq!(entry.identities[0], node.identities[0]);

        session.remove(&*NODE_ID1).await;
        assert!(session.get_by_slot(4).await.is_none());
        assert!(session.get_by_id(&*NODE_ID1).await.is_none());
    }

    #[tokio::test]
    async fn test_direct_session_forwards_add_remove_entry_by_slot() {
        let mut session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1],
        };

        session.register(node.clone(), 4).await;
        session.remove_by_slot(4).await;

        assert!(session.get_by_slot(4).await.is_none());
        assert!(session.get_by_id(&*NODE_ID1).await.is_none());
    }

    #[tokio::test]
    async fn test_direct_session_forwards_secondary_ids() {
        let mut session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1, *NODE_ID2],
        };

        session.register(node.clone(), 4).await;

        let entry = session.get_by_slot(4).await.unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);

        let entry = session.get_by_id(&*NODE_ID1).await.unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);

        let entry = session.get_by_id(&*NODE_ID2).await.unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);
    }

    #[tokio::test]
    async fn test_direct_session_forwards_many_nodes() {
        let mut session = mock_session();
        let node1 = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1, *NODE_ID2],
        };

        let node2 = NodeEntry::<NodeId> {
            default_id: *NODE_ID3,
            identities: vec![*NODE_ID3, *NODE_ID4],
        };

        session.register(node1.clone(), 4).await;
        session.register(node2.clone(), 5).await;

        session.remove(&*NODE_ID1).await;

        assert!(session.get_by_slot(4).await.is_none());
        assert!(session.get_by_id(&*NODE_ID1).await.is_none());
        assert!(session.get_by_id(&*NODE_ID2).await.is_none());

        let entry = session.get_by_slot(5).await.unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);

        let entry = session.get_by_id(&*NODE_ID4).await.unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);

        let entry = session.get_by_id(&*NODE_ID3).await.unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);
    }
}
