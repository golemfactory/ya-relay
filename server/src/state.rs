use chrono::Utc;
use std::collections::HashMap;
use std::convert::TryInto;
use std::ops::Sub;

use crate::error::{InternalError, ServerResult, Unauthorized};
use crate::session::{NodeSession, SessionId};

use ya_client_model::NodeId;

pub struct NodesState {
    /// Constant time access using slot id optimized for forwarding.
    /// The consequence is, that we must store Option<NodeSession>, because
    /// we can't move elements after removal.
    slots: Vec<Option<NodeSession>>,
    sessions: HashMap<SessionId, u32>,
    nodes: HashMap<NodeId, u32>,
}

impl NodesState {
    pub fn new() -> NodesState {
        NodesState {
            slots: vec![],
            sessions: Default::default(),
            nodes: Default::default(),
        }
    }

    pub fn register(&mut self, mut node: NodeSession) {
        let slot = self.empty_slot();

        if slot as usize >= self.slots.len() {
            self.slots.resize(self.slots.len() + 1024, None);
        }

        self.sessions.insert(node.session, slot);
        self.nodes.insert(node.info.node_id, slot);

        node.info.slot = slot;

        self.slots[slot as usize] = Some(node);
    }

    pub fn unregister(&mut self, pos: u32) {
        if let Some(ns) = self.get_by_slot(pos) {
            self.sessions.remove(&ns.session);
            self.nodes.remove(&ns.info.node_id);
            self.slots[pos as usize] = None;
        };
    }

    pub fn check_timeouts(&mut self, timeout: i64) {
        for (pos, slot) in self.slots.clone().iter().enumerate() {
            if let Some(ns) = slot {
                let deadline = Utc::now().sub(chrono::Duration::seconds(timeout));
                if ns.last_seen < deadline {
                    log::debug!(
                        "Session timeout. node_id: {}, session_id: {}",
                        ns.info.node_id,
                        ns.session
                    );
                    self.unregister(pos.try_into().unwrap());
                }
            }
        }
    }

    pub fn update_seen(&mut self, id: SessionId) -> ServerResult<()> {
        match self.sessions.get(&id) {
            None => return Err(Unauthorized::SessionNotFound(id).into()),
            Some(&slot) => match self.slots.get_mut(slot as usize) {
                Some(Some(node)) => node.last_seen = Utc::now(),
                _ => return Err(InternalError::GettingSessionInfo(id).into()),
            },
        };
        Ok(())
    }

    pub fn get_by_slot(&self, slot: u32) -> Option<NodeSession> {
        self.slots.get(slot as usize).cloned().flatten()
    }

    pub fn get_by_session(&self, id: SessionId) -> Option<NodeSession> {
        match self.sessions.get(&id) {
            None => None,
            Some(&slot) => self.slots.get(slot as usize).cloned().flatten(),
        }
    }

    pub fn get_by_node_id(&self, id: NodeId) -> Option<NodeSession> {
        match self.nodes.get(&id) {
            None => None,
            Some(&slot) => self.slots.get(slot as usize).cloned().flatten(),
        }
    }

    fn empty_slot(&self) -> u32 {
        match self.slots.iter().position(|slot| slot.is_none()) {
            None => self.slots.len() as u32,
            Some(idx) => idx as u32,
        }
    }
}

impl Default for NodesState {
    fn default() -> Self {
        Self::new()
    }
}
