use anyhow::anyhow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use ya_relay_core::NodeId;
use ya_relay_proto::proto::SlotId;

use crate::session::Session;

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
#[derive(Clone)]
pub struct NodesRegistry {
    state: Arc<RwLock<NodeRegistryState>>,
}

struct NodeRegistryState {
    nodes: HashMap<NodeId, NodeEntry>,
    slots: HashMap<SlotId, NodeId>,
}

impl NodesRegistry {
    pub fn new() -> NodesRegistry {
        NodesRegistry {
            state: Arc::new(RwLock::new(NodeRegistryState {
                nodes: Default::default(),
                slots: Default::default(),
            })),
        }
    }

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

            // Slot 0 is special number for p2p session.
            if slot != 0 {
                state.slots.insert(slot, node_id);
            }
            node
        }
    }

    pub async fn list_nodes(&self) -> Vec<NodeId> {
        self.state.read().await.nodes.keys().cloned().collect()
    }

    pub async fn add_slot_for_node(&self, node_id: NodeId, slot: SlotId) {
        if slot != 0 {
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
    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<NodeEntry> {
        let state = self.state.read().await;
        state.resolve_node(node)
    }

    #[inline]
    pub async fn resolve_slot(&self, slot: SlotId) -> anyhow::Result<NodeEntry> {
        let state = self.state.read().await;
        state.resolve_slot(slot)
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
    pub fn is_p2p(&self) -> bool {
        match self.slot {
            0 => true,
            _ => false,
        }
    }
}

impl NodeRegistryState {
    fn resolve_slot(&self, slot: SlotId) -> anyhow::Result<NodeEntry> {
        let node_id = self
            .slots
            .get(&slot)
            .cloned()
            .ok_or_else(|| anyhow!("NodeEntry for slot {} not found.", slot))?;
        self.resolve_node(node_id)
    }

    fn resolve_node(&self, node: NodeId) -> anyhow::Result<NodeEntry> {
        self.nodes
            .get(&node)
            .cloned()
            .ok_or_else(|| anyhow!("NodeEntry for node id {} not found.", node))
    }
}

impl Default for NodesRegistry {
    fn default() -> Self {
        Self::new()
    }
}
