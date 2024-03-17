use ya_relay_core::NodeId;

pub mod session_manager;
pub mod slot_manager;

pub(crate) mod control;
mod last_seen;

pub use last_seen::*;

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
