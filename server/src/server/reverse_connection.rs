use crate::server::CompletionHandler;

use crate::state::Clock;
use crate::udp_server::UdpSocket;
use crate::{SessionManager};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{control, request, response, Message, Packet, StatusCode};

mod metric {
    use metrics::{recorder, Counter, Key};

    static KEY_START: Key = Key::from_static_name("ya-relay.packet.reverse-connection");
    static KEY_ERROR: Key = Key::from_static_name("ya-relay.packet.reverse-connection.error");
    static KEY_DONE: Key = Key::from_static_name("ya-relay.packet.reverse-connection.done");

    #[derive(Clone)]
    pub struct RcMetric {
        pub start: Counter,
        pub done: Counter,
        pub error: Counter,
    }

    impl Default for RcMetric {
        fn default() -> Self {
            let recorder = recorder();
            let start = recorder.register_counter(&KEY_START);
            let done = recorder.register_counter(&KEY_DONE);
            let error = recorder.register_counter(&KEY_ERROR);

            Self { start, done, error }
        }
    }
}

pub struct RcHandler {
    session_manager: Arc<SessionManager>,
    metrics: metric::RcMetric,
    ack: CompletionHandler,
    socket: Rc<UdpSocket>,
}

impl RcHandler {
    pub fn new(session_manager: &Arc<SessionManager>, socket: &Rc<UdpSocket>) -> Self {
        let session_manager = Arc::clone(session_manager);
        let metrics = metric::RcMetric::default();
        let ack = super::counter_ack(&metrics.done, &metrics.error);
        let socket = socket.clone();
        Self {
            session_manager,
            metrics,
            ack,
            socket,
        }
    }

    pub fn handle(
        &self,
        clock: &Clock,
        src: SocketAddr,
        request_id: u64,
        session_id: SessionId,
        param: &request::ReverseConnection,
    ) -> Option<(CompletionHandler, Packet)> {
        self.metrics.start.increment(1);
        log::debug!("1");
        let session_ref = match self.session_manager.session(&session_id) {
            Some(session_ref) if session_ref.peer == src => session_ref,
            _ => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::Unauthorized,
                        response::ReverseConnection::default(),
                    ),
                ))
            }
        };

        log::debug!("2");
        let request_node_id: NodeId = match param.node_id.as_slice().try_into() {
            Ok(node_id) => node_id,
            Err(_) => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::BadRequest,
                        response::ReverseConnection::default(),
                    ),
                ))
            }
        };

        clock.touch(&session_ref.ts);
        log::debug!("3");
        let node_id = session_ref.node_id;
        let endpoints = match session_ref.endpoint() {
            Some(e) => vec![e],
            None => {
                log::debug!("[{src}] rejecting reverse connection. reason: no public ip");
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::BadRequest,
                        response::ReverseConnection::default(),
                    ),
                ));
            }
        };

        let (dst_addr, dst_session_id) = match self.session_manager.node_session(request_node_id) {
            Some(it) => (it.peer, it.session_id),
            None => {
                return Some((
                    self.ack.clone(),
                    Packet::response(
                        request_id,
                        session_id.to_vec(),
                        StatusCode::NotFound,
                        response::ReverseConnection::default(),
                    ),
                ))
            }
        };

        let socket = self.socket.clone();
        let bytes = Packet::control(
            dst_session_id.to_vec(),
            control::ReverseConnection {
                node_id: node_id.into_array().to_vec(),
                endpoints,
            },
        )
        .encode_to_vec();
        let h = tokio::task::spawn_local(async move {
            log::debug!("[{src} sending reverse connection to {dst_addr}");
            socket.send_to(&bytes, dst_addr).await.ok();
        });
        drop(h);

        Some((
            self.ack.clone(),
            Packet::response(
                request_id,
                session_id.to_vec(),
                StatusCode::Ok,
                response::ReverseConnection {},
            ),
        ))
    }
}
