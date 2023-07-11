use chrono::Utc;
use itertools::Itertools;
use metrics::counter;
use std::collections::HashMap;
use std::ops::Sub;

use crate::error::{InternalError, ServerResult, Unauthorized};

use ya_relay_core::server_session::{NodeSession, SessionId};
use ya_relay_core::NodeId;
use ya_relay_proto::proto::RequestId;

#[derive(Clone)]
pub enum Slot<T: Clone> {
    Free,
    Some(T),
    Purgatory(T),
}

pub struct NodesState {
    /// Constant time access using slot id optimized for forwarding.
    /// The consequence is, that we must store Option<NodeSession>, because
    /// we can't move elements after removal.
    slots: Vec<Slot<NodeSession>>,
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
        // We don't want to store the same Node multiple times.
        if let Some(node) = self.get_by_node_id(node.info.default_node_id()) {
            self.remove_session(node.info.slot);
        }

        let slot = self.empty_slot();

        if slot as usize >= self.slots.len() {
            self.slots.resize(self.slots.len() + 1024, Slot::Free);
        }

        self.sessions.insert(node.session, slot);
        self.nodes.insert(node.info.default_node_id(), slot);
        for ident in node.info.identities.iter() {
            self.nodes.insert(ident.node_id, slot);
        }

        node.info.slot = slot;

        self.slots[slot as usize] = Slot::Some(node);

        counter!("ya-relay.session.created", 1);
    }

    pub fn neighbours(&self, id: SessionId, count: u32) -> ServerResult<Vec<NodeSession>> {
        let slot = *self
            .sessions
            .get(&id)
            .ok_or(Unauthorized::SessionNotFound(id))?;

        let ref_node_id = self.slots[slot as usize]
            .clone()
            .ok_or(InternalError::GettingSessionInfo(id))?
            .info
            .default_node_id();

        // Sort all nodes by hamming distance between node ids (number of differing bits).
        // Neighbourhood of each node should differ as much as possible, because
        // when it will be used for broadcasts, messages should reach whole network
        // with as low number of steps as possible.
        let neighbours: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| match entry {
                Slot::Free => None,
                Slot::Some(entry) => Some((idx, entry.info.default_node_id())),
                Slot::Purgatory(_) => None,
            })
            .sorted_by(|(_, id1), (_, id2)| {
                Ord::cmp(
                    &hamming_distance(*id1, ref_node_id),
                    &hamming_distance(*id2, ref_node_id),
                )
            })
            .map(|(idx, _)| idx)
            .collect();

        // First node will be always the node for which we are computing neighbourhood, because
        // it has hamming distance 0 from himself.
        let count = std::cmp::min(neighbours.len() - 1, count as usize);
        let neighbours = neighbours[1..=count]
            .iter()
            .filter_map(|&slot| self.slots[slot].active())
            .collect();

        Ok(neighbours)
    }

    pub fn check_timeouts(&mut self, timeout: chrono::Duration, purge_timeout: chrono::Duration) {
        self.slots
            .iter()
            .filter_map(|slot| {
                if let Slot::Some(ns) = slot {
                    let deadline = Utc::now().sub(timeout);
                    if ns.last_seen.time() > deadline {
                        return None;
                    }
                    log::debug!(
                        "Session timeout. node_id: {}, session_id: {}",
                        ns.info.default_node_id(),
                        ns.session
                    );
                    Some(ns.info.slot)
                } else {
                    None
                }
            })
            .collect::<Vec<u32>>()
            .into_iter()
            .for_each(|slot| self.remove_session(slot));

        let deadline = Utc::now().sub(purge_timeout);
        for slot in &mut self.slots {
            if let Slot::Purgatory(session) = slot {
                if session.last_seen.time() < deadline {
                    log::debug!(
                        "Purging not active session with node_id: {}, session_id: {}",
                        session.info.default_node_id(),
                        session.session
                    );

                    slot.purge();
                }
            }
        }
    }

    pub fn remove_session(&mut self, slot: u32) {
        if let Slot::Some(session) = &self.slots[slot as usize] {
            self.sessions.remove(&session.session);
            self.nodes.remove(&session.info.default_node_id());
            for ident in session.info.identities.iter() {
                self.nodes.remove(&ident.node_id);
            }

            self.slots[slot as usize] = Slot::Purgatory(session.clone());

            counter!("ya-relay.session.removed", 1);
        }
    }

    pub fn update_seen(&self, id: SessionId) -> ServerResult<()> {
        match self.sessions.get(&id) {
            None => return Err(Unauthorized::SessionNotFound(id).into()),
            Some(&slot) => match self.slots.get(slot as usize) {
                Some(Slot::Some(node)) => node.last_seen.update(Utc::now()),
                _ => return Err(InternalError::GettingSessionInfo(id).into()),
            },
        };
        Ok(())
    }

    pub fn check_request_duplicate(
        &self,
        id: SessionId,
        request_id: RequestId,
    ) -> ServerResult<bool> {
        match self.sessions.get(&id) {
            None => Err(Unauthorized::SessionNotFound(id).into()),
            Some(&slot) => match self.slots.get(slot as usize) {
                Some(Slot::Some(node)) => {
                    let result = node.request_history.contains(request_id);
                    node.request_history.push(request_id);

                    Ok(result)
                }
                _ => Err(InternalError::GettingSessionInfo(id).into()),
            },
        }
    }

    pub fn get_by_slot(&self, slot: u32) -> Option<NodeSession> {
        self.slots
            .get(slot as usize)
            .cloned()
            .and_then(|entry| entry.active())
    }

    pub fn get_by_session(&self, id: SessionId) -> Option<NodeSession> {
        match self.sessions.get(&id) {
            None => None,
            Some(&slot) => self
                .slots
                .get(slot as usize)
                .cloned()
                .and_then(|entry| entry.active()),
        }
    }

    pub fn get_by_node_id(&self, id: NodeId) -> Option<NodeSession> {
        match self.nodes.get(&id) {
            None => None,
            Some(&slot) => self
                .slots
                .get(slot as usize)
                .cloned()
                .and_then(|entry| entry.active()),
        }
    }

    fn empty_slot(&self) -> u32 {
        // Slot 0 reserved for direct communication will not be used.
        1 + match self.slots.iter().skip(1).position(|slot| slot.is_free()) {
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

pub fn hamming_distance(id1: NodeId, id2: NodeId) -> u32 {
    let id1 = id1.into_array();
    let id2 = id2.into_array();

    let mut hamming = 0;
    for i in 0..id1.len() {
        // Count different bits
        let diff = id1[i] ^ id2[i];
        hamming += diff.count_ones();
    }

    hamming
}

impl<T> Slot<T>
where
    T: Clone,
{
    pub fn ok_or<E>(self, err: E) -> Result<T, E> {
        match self {
            Slot::Free => Err(err),
            Slot::Some(entry) => Ok(entry),
            Slot::Purgatory(_) => Err(err),
        }
    }

    pub fn is_free(&self) -> bool {
        matches!(self, Slot::Free)
    }

    pub fn active(&self) -> Option<T> {
        match self {
            Slot::Free => None,
            Slot::Some(slot) => Some(slot.clone()),
            Slot::Purgatory(_) => None,
        }
    }

    pub fn purge(&mut self) {
        if let Slot::Purgatory(_) = self {
            *self = Slot::Free;
            counter!("ya-relay.session.purged", 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::state::hamming_distance;
    use std::str::FromStr;
    use ya_relay_core::NodeId;

    #[test]
    fn test_hamming() {
        let id1 = NodeId::from_str("0xe9ff07613f3a953627e4ce7b41e16a982ae8b471").unwrap();
        let id2 = NodeId::from_str("0xe90007613f3a953627e4ce7b41e16a982ae8b471").unwrap();
        let id3 = NodeId::from_str("0xe90007613f3a953627e4ce7b41e16a982ae8b470").unwrap();

        assert_eq!(hamming_distance(id1, id1), 0);
        assert_eq!(hamming_distance(id2, id2), 0);
        assert_eq!(hamming_distance(id3, id3), 0);

        assert_eq!(hamming_distance(id2, id3), 1);
        assert_eq!(hamming_distance(id1, id2), 8);
        assert_eq!(hamming_distance(id1, id3), 9);
    }
}
