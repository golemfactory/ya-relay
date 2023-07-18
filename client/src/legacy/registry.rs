use anyhow::anyhow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

use ya_relay_core::session::Session;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{SlotId, FORWARD_SLOT_ID};

#[derive(Clone)]
pub struct NodeEntry {
    pub id: NodeId,
    /// Data will be forwarder (or send directly) through this Session.
    /// To change Session, we need to replace whole NodeEntry.
    pub session: Arc<Session>,
    pub slot: SlotId,
    /// Connection lock
    pub conn_lock: Arc<RwLock<()>>,
}

/// Keeps track of Nodes information, for which we asked relay server.
#[derive(Clone, Default)]
pub struct NodesRegistry {
    state: Arc<RwLock<NodeRegistryState>>,
}

#[derive(Default)]
struct NodeRegistryState {
    nodes: HashMap<NodeId, NodeEntry>,
    slots: HashMap<SlotId, NodeId>,
}

impl NodesRegistry {
    pub async fn add_node(
        &self,
        node_id: NodeId,
        session: Arc<Session>,
        slot: SlotId,
    ) -> NodeEntry {
        let node = NodeEntry {
            id: node_id,
            session,
            slot,
            conn_lock: Default::default(),
        };

        {
            let mut state = self.state.write().await;
            state.nodes.insert(node_id, node.clone());

            if slot != FORWARD_SLOT_ID {
                state.slots.insert(slot, node_id);
            }
            node
        }
    }

    pub async fn list_nodes(&self) -> Vec<NodeId> {
        self.state.read().await.nodes.keys().cloned().collect()
    }

    pub async fn add_slot_for_node(&self, node_id: NodeId, slot: SlotId) {
        if slot != FORWARD_SLOT_ID {
            let mut state = self.state.write().await;
            state.slots.insert(slot, node_id);
        }
    }

    pub async fn nodes_using_session(&self, session: Arc<Session>) -> Vec<NodeId> {
        let state = self.state.read().await;
        state
            .nodes
            .iter()
            .filter_map(|(_, entry)| match entry.session.id == session.id {
                true => Some(entry.id),
                false => None,
            })
            .collect()
    }

    #[inline]
    pub async fn get_node(&self, node: NodeId) -> anyhow::Result<NodeEntry> {
        let state = self.state.read().await;
        state.get_node(node)
    }

    #[inline]
    pub async fn get_node_by_slot(&self, slot: SlotId) -> anyhow::Result<NodeEntry> {
        let state = self.state.read().await;
        state.get_node_by_slot(slot)
    }

    pub async fn remove_node(&self, node_id: NodeId) -> anyhow::Result<()> {
        let mut state = self.state.write().await;
        if let Some(entry) = state.nodes.remove(&node_id) {
            state.slots.remove(&entry.slot);
        }
        Ok(())
    }
}

impl NodeEntry {
    #[inline]
    pub fn is_p2p(&self) -> bool {
        self.slot == FORWARD_SLOT_ID
    }

    pub async fn guard(&self) -> (OwnedRwLockWriteGuard<()>, bool) {
        let conn_lock = self.conn_lock.clone();
        match conn_lock.clone().try_write_owned() {
            Ok(guard) => (guard, false),
            Err(_) => (conn_lock.write_owned().await, true),
        }
    }
}

impl NodeRegistryState {
    fn get_node_by_slot(&self, slot: SlotId) -> anyhow::Result<NodeEntry> {
        self.slots
            .get(&slot)
            .and_then(|id| self.get_node(*id).ok())
            .ok_or_else(|| anyhow!("NodeEntry for slot {} not found.", slot))
    }

    fn get_node(&self, node: NodeId) -> anyhow::Result<NodeEntry> {
        self.nodes
            .get(&node)
            .cloned()
            .ok_or_else(|| anyhow!("NodeEntry for node id {} not found.", node))
    }
}
