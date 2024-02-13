use anyhow::anyhow;
use metrics::{counter, increment_counter};
use std::collections::HashMap;
use std::sync::Arc;
use bytes::Bytes;

use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::TransportType;
use ya_relay_core::sync::Actuator;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Forward, Payload, SlotId, FORWARD_SLOT_ID};

use crate::error::SessionError;
use crate::metrics::{RELAY_ID, SOURCE_ID, TARGET_ID};
use crate::raw_session::RawSession;

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
        log::trace!("[add]: node {} to slot {}", node.default_id, slot);
        // Remove previous information about node.
        // We are removing all identities and slot. This is redundant, because in most cases
        // using default id should be enough. This protects from situations, when `NodeEntries`
        // for different Nodes overlap. In theory it shouldn't happen, because only one Node should
        // have access to private key, but it's better to ensure data consistency here.
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
        log::trace!("[remove]: trying to remove node {}", node_id);
        if let Some(slot) = self.nodes.remove(node_id) {
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

    pub fn get_slot(&self, node_id: &NodeId) -> Option<SlotId> {
        self.nodes.get(node_id).cloned()
    }

    pub fn get_by_id(&self, node_id: &NodeId) -> Option<NodeEntry<NodeId>> {
        if let Some(slot) = self.nodes.get(node_id) {
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
    /// It would be preferred to use `NodeEntry<Identity>` here, but current implementation of
    /// relay server doesn't provide public key.
    pub owner: NodeEntry<NodeId>,
    pub raw: Arc<RawSession>,

    /// Nodes allowed to use this session for forwarding.
    /// In case of Relay Server session this corresponds to all Nodes, that we queried
    /// information about.
    /// In case of p2p session these values will be empty. In future we can use other
    /// sessions than Relay to forward packets.
    pub forwards: Arc<std::sync::RwLock<AllowedForwards>>,
    /// Implements limiting forwarding functionality.
    pub(crate) forward_pause: Actuator,
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
            forwards: Arc::new(std::sync::RwLock::new(Default::default())),
            forward_pause: Default::default(),
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
            forwards: Arc::new(std::sync::RwLock::new(Default::default())),
            forward_pause: Default::default(),
        }))
    }

    pub async fn send(
        &self,
        target: NodeId,
        packet: Bytes,
        transport: TransportType,
        encrypted: bool,
    ) -> anyhow::Result<()> {
        let router_id = self.owner.default_id;
        let slot = if router_id == target {
            FORWARD_SLOT_ID
        } else {
            self.find_slot(&target)
                .ok_or(SessionError::Internal(format!(
                    "Session with [{router_id}] doesn't allow to forward packets for [{target}]"
                )))?
        };

        let mut forward = match transport {
            TransportType::Unreliable => Forward::unreliable(self.raw.id, slot, packet),
            TransportType::Reliable => Forward::new(self.raw.id, slot, packet),
            TransportType::Transfer => Forward::new(self.raw.id, slot, packet),
        };

        let size = forward.encoded_len();
        if encrypted {
            forward.set_encrypted();
        }

        self.wait_for_resume().await;
        self.raw.send(forward).await?;

        self.record_outgoing(target, transport, size);
        Ok(())
    }

    pub fn remove_by_slot(&self, id: SlotId) -> anyhow::Result<NodeId> {
        let mut forwards = self.forwards.write().unwrap();
        forwards.remove_by_slot(id).ok_or(anyhow!(
            "Slot {id} not found in session: {} ({})",
            self.raw.id,
            self.owner.default_id
        ))
    }

    pub fn remove(&self, node_id: &NodeId) -> anyhow::Result<NodeEntry<NodeId>> {
        let mut forwards = self.forwards.write().unwrap();
        forwards.remove(node_id).ok_or(anyhow!(
            "Node {node_id} not found in session: {} ({})",
            self.raw.id,
            self.owner.default_id
        ))
    }

    pub fn list(&self) -> Vec<NodeEntry<NodeId>> {
        let forwards = self.forwards.read().unwrap();
        forwards.slots.values().cloned().collect()
    }

    pub fn register(&self, node: NodeEntry<NodeId>, slot: SlotId) {
        let mut forwards = self.forwards.write().unwrap();
        forwards.add(node, slot)
    }

    pub fn get_by_slot(&self, slot: SlotId) -> Option<NodeEntry<NodeId>> {
        let forwards = self.forwards.read().unwrap();
        forwards.get_by_slot(slot)
    }

    pub fn get_by_id(&self, node_id: &NodeId) -> Option<NodeEntry<NodeId>> {
        let forwards = self.forwards.read().unwrap();
        forwards.get_by_id(node_id)
    }

    pub fn find_slot(&self, node_id: &NodeId) -> Option<SlotId> {
        let forwards = self.forwards.read().unwrap();
        forwards.get_slot(node_id)
    }

    #[inline]
    pub async fn pause_forwarding(&self) {
        self.forward_pause.enable();
    }

    #[inline]
    pub async fn resume_forwarding(&self) {
        self.forward_pause.disable();
    }

    /// Other party can pause forwarding for multiple reasons:
    /// 1. It is not able to receive messages so fast (in most cases TCP is enough to avoid this)
    /// 2. It forwards traffic for many other Nodes and we are using to much of bandwidth.
    /// 3. After session initialization we need to wait until other party will be able to receive
    ///    packets
    #[inline]
    pub async fn wait_for_resume(&self) {
        if let Some(resumed) = self.forward_pause.next() {
            log::debug!(
                "Session {} (node = {}) is awaiting a ResumeForwarding message",
                self.raw.id,
                self.owner.default_id
            );

            resumed.await;
        }
    }

    #[inline]
    fn record_outgoing(&self, target: NodeId, transport: TransportType, size: usize) {
        let relay = self.owner.default_id.to_string();
        let target = target.to_string();
        match transport {
            TransportType::Reliable | TransportType::Transfer => {
                counter!("ya-relay.packet.tcp.outgoing.size", size as u64, TARGET_ID => target.clone(), RELAY_ID => relay.clone());
                increment_counter!("ya-relay.packet.tcp.outgoing.num", TARGET_ID => target, RELAY_ID => relay);
            }
            TransportType::Unreliable => {
                counter!("ya-relay.packet.udp.outgoing.size", size as u64, TARGET_ID => target.clone(), RELAY_ID => relay.clone());
                increment_counter!("ya-relay.packet.udp.outgoing.num", TARGET_ID => target, RELAY_ID => relay);
            }
        }
    }

    #[inline]
    pub fn record_incoming(&self, source: NodeId, transport: TransportType, size: usize) {
        let relay = self.owner.default_id.to_string();
        let source = source.to_string();
        match transport {
            TransportType::Unreliable => {
                counter!("ya-relay.packet.udp.incoming.size", size as u64, SOURCE_ID => source.clone(), RELAY_ID => relay.clone());
                increment_counter!("ya-relay.packet.udp.incoming.num", SOURCE_ID => source, RELAY_ID => relay);
            }
            TransportType::Reliable | TransportType::Transfer => {
                counter!("ya-relay.packet.tcp.incoming.size", size as u64, SOURCE_ID => source.clone(), RELAY_ID => relay.clone());
                increment_counter!("ya-relay.packet.tcp.incoming.num", SOURCE_ID => source, RELAY_ID => relay);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ya_relay_core::server_session::SessionId;
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
        let session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1],
        };

        session.register(node.clone(), 4);

        assert_eq!(session.find_slot(&NODE_ID1).unwrap(), 4);

        let entry = session.get_by_slot(4).unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 1);
        assert_eq!(entry.identities[0], node.identities[0]);

        let entry = session.get_by_id(&NODE_ID1).unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 1);
        assert_eq!(entry.identities[0], node.identities[0]);

        session.remove(&NODE_ID1).unwrap();
        assert!(session.get_by_slot(4).is_none());
        assert!(session.get_by_id(&NODE_ID1).is_none());
        assert!(session.find_slot(&NODE_ID1).is_none());
    }

    #[tokio::test]
    async fn test_direct_session_forwards_add_remove_entry_by_slot() {
        let session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1],
        };

        session.register(node, 4);
        session.remove_by_slot(4).unwrap();

        assert!(session.get_by_slot(4).is_none());
        assert!(session.get_by_id(&NODE_ID1).is_none());
        assert!(session.find_slot(&NODE_ID1).is_none());
    }

    #[tokio::test]
    async fn test_direct_session_forwards_secondary_ids() {
        let session = mock_session();
        let node = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1, *NODE_ID2],
        };

        session.register(node.clone(), 4);

        assert_eq!(session.find_slot(&NODE_ID1).unwrap(), 4);
        assert_eq!(session.find_slot(&NODE_ID2).unwrap(), 4);

        let entry = session.get_by_slot(4).unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);

        let entry = session.get_by_id(&NODE_ID1).unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);

        let entry = session.get_by_id(&NODE_ID2).unwrap();
        assert_eq!(entry.default_id, node.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node.identities[0]);
        assert_eq!(entry.identities[1], node.identities[1]);
    }

    #[tokio::test]
    async fn test_direct_session_forwards_many_nodes() {
        let session = mock_session();
        let node1 = NodeEntry::<NodeId> {
            default_id: *NODE_ID1,
            identities: vec![*NODE_ID1, *NODE_ID2],
        };

        let node2 = NodeEntry::<NodeId> {
            default_id: *NODE_ID3,
            identities: vec![*NODE_ID3, *NODE_ID4],
        };

        session.register(node1, 4);
        session.register(node2.clone(), 5);

        session.remove(&NODE_ID1).unwrap();

        assert!(session.get_by_slot(4).is_none());
        assert!(session.get_by_id(&NODE_ID1).is_none());
        assert!(session.get_by_id(&NODE_ID2).is_none());
        assert!(session.find_slot(&NODE_ID1).is_none());
        assert!(session.find_slot(&NODE_ID2).is_none());

        let entry = session.get_by_slot(5).unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);

        let entry = session.get_by_id(&NODE_ID4).unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);

        let entry = session.get_by_id(&NODE_ID3).unwrap();
        assert_eq!(entry.default_id, node2.default_id);
        assert_eq!(entry.identities.len(), 2);
        assert_eq!(entry.identities[0], node2.identities[0]);
        assert_eq!(entry.identities[1], node2.identities[1]);
    }
}
