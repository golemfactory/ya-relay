use crate::state::slot_manager::SlotManager;
use crate::state::TsDecoder;
use crate::{Session, SessionManager};
use ya_relay_core::NodeId;
use ya_relay_proto::proto::response::Node as NodeInfo;

pub struct Decoder<'a, 'b> {
    _session_manager: &'a SessionManager,
    slot_manager: &'b SlotManager,
    ts_decoder: TsDecoder,
}

pub fn decoder<'a, 'b>(
    _session_manager: &'a SessionManager,
    slot_manager: &'b SlotManager,
) -> Decoder<'a, 'b> {
    let ts_decoder = TsDecoder::new();

    Decoder {
        _session_manager,
        slot_manager,
        ts_decoder,
    }
}

impl<'a, 'b> Decoder<'a, 'b> {
    pub fn to_node_info(&self, session: &Session, context: NodeId, pk: bool) -> NodeInfo {
        let (session_pub_key, session_key_proof, session_key_proofs) = if pk {
            if let Some((session_key, proofs)) = &session.session_key {
                let session_pub_key = session_key.bytes().to_vec();
                // `NodeInfo` keeps the default identity first, so the client verifies
                // the session-key proof against that identity even when `context` is
                // one of the node's aliases.
                let session_key_proof = session
                    .keys
                    .first()
                    .and_then(|identity| proofs.get(&identity.node_id))
                    .cloned()
                    .unwrap_or_default();
                let session_key_proofs = session
                    .keys
                    .iter()
                    .map(|identity| proofs.get(&identity.node_id).cloned().unwrap_or_default())
                    .collect();
                (session_pub_key, session_key_proof, session_key_proofs)
            } else {
                Default::default()
            }
        } else {
            Default::default()
        };

        NodeInfo {
            identities: session.keys.iter().map(Into::into).collect::<Vec<_>>(),
            endpoints: session.endpoint().into_iter().collect(),
            seen_ts: self.ts_decoder.decode(&session.ts),
            slot: self.slot_manager.slot(context),
            supported_encryptions: session.supported_encryptions.clone(),
            session_pub_key,
            session_key_proof,
            session_key_proofs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use parking_lot::Mutex;
    use ya_relay_core::crypto::SecretKey;
    use ya_relay_core::identity::Identity;
    use ya_relay_core::server_session::SessionId;

    use crate::state::session_manager::AddrStatus;
    use crate::state::slot_manager::SlotManager;
    use crate::state::LastSeen;
    use crate::{Session, SessionManager};

    use super::decoder;

    #[test]
    fn alias_lookup_uses_default_identity_session_key_proof() {
        let default_identity = Identity::from(SecretKey::from_raw(&[1; 32]).unwrap().public());
        let alias_identity = Identity::from(SecretKey::from_raw(&[2; 32]).unwrap().public());
        let session_key = SecretKey::from_raw(&[3; 32]).unwrap().public();
        let default_proof = vec![1, 2, 3];
        let alias_proof = vec![4, 5, 6];
        let proofs = HashMap::from([
            (default_identity.node_id, default_proof.clone()),
            (alias_identity.node_id, alias_proof.clone()),
        ]);
        let session = Session {
            session_id: SessionId::generate(),
            peer: "127.0.0.1:1234".parse().unwrap(),
            ts: LastSeen::now(),
            node_id: default_identity.node_id,
            keys: vec![default_identity, alias_identity.clone()],
            supported_encryptions: vec!["Aes256GcmSiv".to_owned()],
            addr_status: Mutex::new(AddrStatus::Unknown),
            session_key: Some((session_key, proofs)),
        };
        let session_manager = SessionManager::new();
        let slot_manager = SlotManager::new();

        let node = decoder(&session_manager, &slot_manager).to_node_info(
            &session,
            alias_identity.node_id,
            true,
        );

        assert_eq!(node.session_key_proof, default_proof);
        assert_eq!(node.session_key_proofs, vec![default_proof, alias_proof]);
    }
}
