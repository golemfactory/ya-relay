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
        let (session_pub_key, session_key_proof) = if pk {
            if let Some((session_key, proofs)) = &session.session_key {
                let session_pub_key = session_key.bytes().to_vec();
                let session_key_proof = proofs.get(&context).cloned().unwrap_or_default();
                (session_pub_key, session_key_proof)
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
        }
    }
}
