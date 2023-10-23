use std::collections::HashMap;
use std::sync::Arc;
use ::metrics::Counter;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use ya_relay_core::NodeId;

pub type SlotId = u32;

struct Inner {
    nodes : HashMap<NodeId, SlotId>,
    slots : Vec<NodeId>
}

pub struct SlotManager {
    inner : RwLock<Inner>,
    created_counter : Counter
}

impl SlotManager {

    pub fn new() -> Arc<Self> {
        let mut inner = Inner {
            nodes: Default::default(),
            slots: Default::default()
        };

        let node_id = Default::default();
        inner.slots.push(node_id);
        inner.nodes.insert(node_id, 0);

        Arc::new(Self {
            inner: RwLock::new(inner),
            created_counter: metrics::created_counter()
        })
    }

    pub fn slot(&self, node_id : NodeId) -> SlotId {
        let g=  self.inner.upgradable_read();
        if let Some(slot_id) = g.nodes.get(&node_id) {
            return *slot_id;
        }
        let mut gw = RwLockUpgradableReadGuard::upgrade(g);
        let slot_id = gw.slots.len() as SlotId;
        gw.slots.push(node_id);
        gw.nodes.insert(node_id, slot_id);
        debug_assert_eq!(gw.slots.len(), gw.nodes.len());
        drop(gw);
        self.created_counter.increment(1);
        slot_id
    }

    pub fn len(&self) -> usize {
        self.inner.read().slots.len()
    }
}


mod metrics {
    use metrics::{Counter, Key, recorder};

    const SLOT_CREATED : &str = "ya-relay.slot.created";

    static KEY_SLOT_CREATED : Key = Key::from_static_name(SLOT_CREATED);

    pub fn created_counter() -> Counter {
        recorder().register_counter(&KEY_SLOT_CREATED)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::prelude::*;

    #[test]
    fn test_init_slot() {
        let m = SlotManager::new();

        assert_eq!(m.slot(NodeId::default()), 0);
    }

    #[test]
    fn test_random_slots() {
        let m = SlotManager::new();
        let mut rng = thread_rng();

        let mut slots = (1..10).into_iter().map(|_| {
            let node_id : NodeId= rng.gen::<[u8;20]>().into();

            (node_id, m.slot(node_id))
        }).collect::<Vec<_>>();

        assert_eq!(m.slot(NodeId::default()), 0);
        for _ in 0..100 {
            slots.shuffle(&mut rng);
            for &(node_id, slot_id) in &slots {
                assert_eq!(m.slot(node_id), slot_id);
            }
            assert_eq!(m.len(), 10);
        }
    }


}