use crate::server::CompletionHandler;
use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::{SessionManager, SessionState};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{request, response, Identity, Packet, StatusCode};

mod metric {
    use crate::server::DoneAck;
    use crate::state::Clock;
    use metrics::{recorder, Counter, Histogram, Key};

    const KEY_START: Key = Key::from_static_name("ya-relay.packet.slot-info");
    const KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.slot-info.error");
    const KEY_DONE: Key = Key::from_static_name("ya-relay.packet.slot-info.done");
    const PROCESSING_TIME: Key = Key::from_static_name("ya-relay.packet.slot-info.processing-time");

    #[derive(Clone)]
    pub struct SlotMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
        pub processing: Histogram,
    }

    impl Default for SlotMetric {
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

    impl DoneAck for SlotMetric {
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

pub struct SlotHandler {
    session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    metrics: metric::SlotMetric,
    ack: CompletionHandler,
}

impl SlotHandler {
    pub fn new(session_manager: &Arc<SessionManager>, slot_manager: &Arc<SlotManager>) -> Self {
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics = metric::SlotMetric::default();
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
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: SessionId,
        param: &request::Slot,
    ) -> Option<(CompletionHandler, Packet)> {
        self.metrics.start.increment(1);
        let session_ref = match self.session_manager.session(&session_id) {
            Some(session_ref) if session_ref.peer == src => session_ref,
            _ => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Unauthorized,
                        response::Node::default(),
                    ),
                ))
            }
        };
        clock.touch(&session_ref.ts);

        let request_node_id: NodeId = self.slot_manager.node(param.slot)?;

        let (keys, found_node_id, endpoints) =
            match self.session_manager.node_session(request_node_id) {
                Some(it) => match &*it.state.lock() {
                    SessionState::Est {
                        keys,
                        node_id,
                        addr_status,
                        ..
                    } => (keys.clone(), *node_id, addr_status.endpoints(it.peer)),
                    _ => {
                        return Some((
                            self.ack.clone(),
                            Packet::response(
                                request_id,
                                session_id.to_vec(),
                                StatusCode::NotFound,
                                response::Node::default(),
                            ),
                        ))
                    }
                },
                None => {
                    return Some((
                        self.ack.clone(),
                        Packet::response(
                            request_id,
                            session_id.to_vec(),
                            StatusCode::NotFound,
                            response::Node::default(),
                        ),
                    ))
                }
            };

        let node = response::Node {
            identities: keys.into_iter().map(Into::into).collect(),
            endpoints,
            slot: param.slot,
            supported_encryptions: vec![],
            ..Default::default()
        };
        Some((
            self.ack.clone(),
            Packet::response(request_id, session_id.to_vec(), StatusCode::Ok, node),
        ))
    }
}
