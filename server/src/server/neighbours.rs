use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use itertools::Itertools;

use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto::response::Neighbours;
use ya_relay_proto::proto::{request, Packet, StatusCode};

use crate::server::CompletionHandler;
use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::SessionManager;

mod metric {
    use metrics::{recorder, Counter, Histogram, Key};

    use crate::server::DoneAck;
    use crate::state::Clock;

    static KEY_START: Key = Key::from_static_name("ya-relay.packet.neighborhood");
    static KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.neighborhood.error");
    static KEY_DONE: Key = Key::from_static_name("ya-relay.packet.neighborhood.done");

    static PROCESSING_TIME: Key =
        Key::from_static_name("ya-relay.packet.neighborhood.processing-time");

    #[derive(Clone)]
    pub struct NeighboursMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
        pub processing: Histogram,
    }

    impl Default for NeighboursMetric {
        fn default() -> Self {
            let recorder = recorder();
            let start = recorder.register_counter(&KEY_START);
            let done = recorder.register_counter(&KEY_DONE);
            let error = recorder.register_counter(&KEY_ERROR);
            let processing = recorder.register_histogram(&PROCESSING_TIME);
            Self {
                start,
                done,
                error,
                processing,
            }
        }
    }

    impl DoneAck for NeighboursMetric {
        fn done(&self, clock: &Clock) {
            self.done.increment(1);
            self.processing.record(clock.time().elapsed());
        }

        fn error(&self, clock: &Clock) {
            self.error.increment(1);
            self.processing.record(clock.time().elapsed());
        }
    }
}

pub struct NeighboursHandler {
    session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    metrics: metric::NeighboursMetric,
    ack: CompletionHandler,
}

impl NeighboursHandler {
    pub fn new(session_manager: &Arc<SessionManager>, slot_manager: &Arc<SlotManager>) -> Self {
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics = metric::NeighboursMetric::default();
        let ack = Rc::new(metrics.clone());
        Self {
            session_manager,
            slot_manager,
            metrics,
            ack,
        }
    }
    pub fn handle(
        &self,
        _clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: SessionId,
        param: &request::Neighbours,
    ) -> Option<(CompletionHandler, Packet)> {
        self.metrics.start.increment(1);
        let decoder = super::state_decoder::decoder(&self.session_manager, &self.slot_manager);

        let session_ref = match self.session_manager.session(&session_id) {
            Some(session_ref) if session_ref.peer == src => session_ref,
            _ => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Unauthorized,
                        Neighbours::default(),
                    ),
                ))
            }
        };
        let node_id = session_ref.node_id;

        let neighbours = self
            .session_manager
            .neighbours(node_id, param.count as usize);

        let nodes = neighbours
            .into_iter()
            .map(|session_ref| decoder.to_node_info(&session_ref))
            .collect_vec();

        let response = Packet::response(
            request_id,
            session_id.to_vec(),
            StatusCode::Ok,
            Neighbours { nodes },
        );

        Some((self.ack.clone(), response))
    }
}
