use crate::server::state_decoder::decoder;
use crate::server::CompletionHandler;
use crate::state::slot_manager::SlotManager;
use crate::state::Clock;
use crate::SessionManager;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{request, response, Packet, StatusCode};

mod metric {
    use crate::server::DoneAck;
    use crate::state::Clock;
    use metrics::{recorder, Counter, Histogram, Key};

    static KEY_START: Key = Key::from_static_name("ya-relay.packet.node");
    static KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.node.error");
    static KEY_DONE: Key = Key::from_static_name("ya-relay.packet.node.done");

    static PROCESSING_TIME: Key = Key::from_static_name("ya-relay.packet.node.processing-time");

    #[derive(Clone)]
    pub struct NodeMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
        pub processing: Histogram,
    }

    impl Default for NodeMetric {
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

    impl DoneAck for NodeMetric {
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

pub struct NodeHandler {
    session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    metrics: metric::NodeMetric,
    ack: CompletionHandler,
}

impl NodeHandler {
    pub fn new(session_manager: &Arc<SessionManager>, slot_manager: &Arc<SlotManager>) -> Self {
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics = metric::NodeMetric::default();
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
        param: &request::Node,
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
        let decoder = decoder(&self.session_manager, &self.slot_manager);

        let request_node_id: NodeId = match param.node_id.as_slice().try_into() {
            Ok(node_id) => node_id,
            Err(_) => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::BadRequest,
                        response::Node::default(),
                    ),
                ))
            }
        };

        let node = match self.session_manager.node_session(request_node_id) {
            Some(it) => decoder.to_node_info(&it),
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

        Some((
            self.ack.clone(),
            Packet::response(request_id, session_id.to_vec(), StatusCode::Ok, node),
        ))
    }
}
