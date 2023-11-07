use crate::state::slot_manager::SlotManager;
use crate::state::TsDecoder;
use crate::{Session, SessionManager};
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
    pub fn to_node_info(&self, session: &Session) -> NodeInfo {
        let identities = session.keys.iter().map(Into::into).collect();

        NodeInfo {
            identities,
            endpoints: session.endpoint().into_iter().collect(),
            seen_ts: self.ts_decoder.decode(&session.ts),
            slot: self.slot_manager.slot(session.node_id),
            supported_encryptions: session.supported_encryptions.clone(),
        }
    }
}

