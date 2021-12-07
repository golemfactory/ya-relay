use anyhow::anyhow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use ya_client_model::NodeId;
use ya_relay_proto::proto::SlotId;

use crate::session::Session;

#[derive(Clone)]
pub struct NodeEntry {
    pub id: NodeId,
    /// Data will be forwarder (or send directly) through this Session.
    /// To change Session, we need to replace whole NodeEntry.
    pub session: Arc<Session>,
    pub slot: SlotId,
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
    ) -> anyhow::Result<NodeEntry> {
        let node = NodeEntry {
            id: node_id,
            session,
            slot,
        };

        {
            let mut state = self.state.write().await;
            state.nodes.insert(node_id, node.clone());
            state.slots.insert(slot, node_id);
        }
        Ok(node)
    }

    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<NodeEntry> {
        let state = self.state.read().await;
        state.resolve_node(node)
    }

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
