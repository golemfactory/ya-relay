use crate::server::CompletionHandler;
use crate::state::slot_manager::{SlotId, SlotManager};
use crate::state::Clock;
use crate::SessionManager;
use bytes::BytesMut;

use crate::udp_server::UdpSocket;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use ya_relay_core::server_session::SessionId;

use ya_relay_proto::proto::{control, Forward, Packet, Payload};

mod metric {
    use crate::server::DoneAck;
    use crate::state::Clock;
    use metrics::{recorder, Counter, Key};

    static START: Key = Key::from_static_name("ya-relay.packet.forward");
    static ERROR: Key = Key::from_static_name("ya-relay.packet.forward.error");
    static DONE: Key = Key::from_static_name("ya-relay.packet.forward.done");

    static IN_SIZE: Key = Key::from_static_name("ya-relay.packet.forward.incoming.size");
    static OUT_SIZE: Key = Key::from_static_name("ya-relay.packet.forward.outgoing.size");

    #[derive(Clone)]
    pub struct ForwardMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
        pub in_bytes: Counter,
        pub out_bytes: Counter,
    }

    impl Default for ForwardMetric {
        fn default() -> Self {
            let recorder = recorder();
            let start = recorder.register_counter(&START);
            let done = recorder.register_counter(&DONE);
            let error = recorder.register_counter(&ERROR);
            let in_bytes = recorder.register_counter(&IN_SIZE);
            let out_bytes = recorder.register_counter(&OUT_SIZE);
            Self {
                start,
                done,
                error,
                in_bytes,
                out_bytes,
            }
        }
    }

    impl DoneAck for ForwardMetric {
        fn done(&self, _clock: &Clock) {
            self.done.increment(1);
        }

        fn error(&self, _clock: &Clock) {
            self.error.increment(1);
        }
    }
}

pub struct ForwardHandler {
    session_manager: Arc<SessionManager>,
    slot_manager: Arc<SlotManager>,
    metrics: metric::ForwardMetric,
    ack: CompletionHandler,
    socket: Rc<UdpSocket>,
}

impl ForwardHandler {
    pub fn new(
        session_manager: &Arc<SessionManager>,
        slot_manager: &Arc<SlotManager>,
        socket: &Rc<UdpSocket>,
    ) -> Self {
        let session_manager = Arc::clone(session_manager);
        let slot_manager = slot_manager.clone();
        let metrics = metric::ForwardMetric::default();
        let ack = Rc::new(metrics.clone());
        let socket = Rc::clone(socket);
        Self {
            session_manager,
            slot_manager,
            metrics,
            ack,
            socket,
        }
    }
    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        session_id: SessionId,
        slot: SlotId,
        flags: u16,
        payload: Payload,
    ) -> Option<(CompletionHandler, Packet)> {
        self.metrics.start.increment(1);
        self.metrics.in_bytes.increment(payload.len() as u64);

        let src_info = self
            .session_manager
            .session(&session_id)
            .and_then(|session_ref| {
                if session_ref.peer != src {
                    return None;
                }
                let src_node_id = session_ref.node_id;
                let src_slot = self.slot_manager.slot(src_node_id);
                clock.touch(&session_ref.ts);

                Some((src_node_id, src_slot))
            });
        let dst_info = self.slot_manager.node(slot).and_then(|node_id| {
            let dst_session = self.session_manager.node_session(node_id)?;
            let dst_addr = dst_session.peer;

            Some((dst_addr, dst_session.session_id))
        });

        match (src_info, dst_info) {
            (Some((src_node_id, src_slot)), Some((dst_addr, dst_session_id))) => {
                let payload_size = payload.len();
                let forward = Forward {
                    session_id: dst_session_id.to_array(),
                    slot: src_slot,
                    flags,
                    payload,
                };
                let mut bytes = BytesMut::new();
                bytes.reserve(forward.encoded_len());
                forward.encode(&mut bytes);
                let socket = self.socket.clone();

                let out_bytes = self.metrics.out_bytes.clone();
                let done = self.metrics.done.clone();
                let error = self.metrics.error.clone();

                tokio::task::spawn_local(async move {
                    match socket.send_to(&bytes, dst_addr).await {
                        Ok(v) => {
                            out_bytes.increment(payload_size as u64);
                            done.increment(1);
                            log::debug!("forwarded {} bytes from {src}:{src_node_id}:{src_slot} to {dst_addr}:{slot}", v);
                        }
                        Err(e) => {
                            error.increment(1);
                            log::error!("fail {:?}", e);
                        }
                    }
                });
                None
            }
            (None, _) => Some((
                self.ack.clone(),
                Packet::control(
                    session_id.to_vec(),
                    control::Disconnected {
                        by: control::disconnected::By::SessionId(Default::default()).into(),
                    },
                ),
            )),
            (_, None) => Some((
                self.ack.clone(),
                Packet::control(
                    session_id.to_vec(),
                    ya_relay_proto::proto::control::Disconnected {
                        by: Some(control::disconnected::By::Slot(slot)),
                    },
                ),
            )),
        }
    }
}
