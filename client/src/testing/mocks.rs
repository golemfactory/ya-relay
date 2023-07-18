use crate::client::SessionError;
use crate::direct_session::DirectSession;
use crate::session::session_traits::SessionDeregistration;

use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::dispatch::Handler;
use crate::raw_session::RawSession;
use crate::session::SessionLayer;

use ya_relay_core::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::control::{Kind, ReverseConnection};
use ya_relay_proto::proto::{Control, Forward, Request, RequestId};

pub struct NoOpSessionLayer;

#[async_trait(?Send)]
impl SessionDeregistration for NoOpSessionLayer {
    async fn unregister(&self, _node_id: NodeId) {}
    async fn unregister_session(&self, _session: Arc<DirectSession>) {}
    async fn abort_initializations(&self, _remote: SocketAddr) -> Result<(), SessionError> {
        Ok(())
    }
}

/// Implements dispatcher `Handler` interface
/// Passes all packets to `SessionLayer` until it will be requested. Than
/// packets are passed to channel and receiver handles them as he wants.
#[derive(Clone)]
pub struct MockHandler {
    layer: SessionLayer,
    pub captures: Arc<Captures>,
}

#[derive(Default)]
pub struct Captures {
    pub session_request: CaptureChannel<SessionReqParams>,
    pub reverse_connection: CaptureChannel<ReverseConnectionParams>,
}

pub struct CaptureChannel<T> {
    tx: mpsc::Sender<T>,
    rx: std::sync::Mutex<Option<mpsc::Receiver<T>>>,
}

impl<T: 'static> CaptureChannel<T> {
    pub fn new() -> CaptureChannel<T> {
        let (tx, rx) = mpsc::channel(1);
        CaptureChannel {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
        }
    }

    pub fn start_capture(&self) -> mpsc::Receiver<T> {
        self.rx.lock().unwrap().take().unwrap()
    }

    pub fn drop_all(&self) {
        let mut rx = self.start_capture();
        tokio::task::spawn_local(async move { while rx.recv().await.is_some() {} });
    }

    pub fn stop_capture(&self, rx: mpsc::Receiver<T>) {
        *self.rx.lock().unwrap() = Some(rx);
    }

    pub fn should_capture(&self) -> bool {
        self.rx.lock().unwrap().is_none()
    }
}

impl<T: 'static> Default for CaptureChannel<T> {
    fn default() -> Self {
        CaptureChannel::<T>::new()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionReqParams {
    pub session_id: Vec<u8>,
    pub request_id: RequestId,
    pub from: SocketAddr,
    pub request: proto::request::Session,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ReverseConnectionParams {
    pub session_id: Vec<u8>,
    pub from: SocketAddr,
    pub message: ReverseConnection,
}

impl MockHandler {
    pub fn new(layer: SessionLayer) -> Self {
        MockHandler {
            layer,
            captures: Arc::new(Captures::default()),
        }
    }
}

impl Handler for MockHandler {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<RawSession>>> {
        self.layer.dispatcher(from)
    }

    fn session(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<DirectSession>>> {
        Handler::session(&self.layer, from)
    }

    fn on_control(
        self,
        session_id: Vec<u8>,
        control: Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        match control {
            Control {
                kind: Some(Kind::ReverseConnection(message)),
            } if self.captures.reverse_connection.should_capture() => Some(
                async move {
                    self.captures
                        .reverse_connection
                        .tx
                        .send(ReverseConnectionParams {
                            session_id,
                            from,
                            message,
                        })
                        .await
                        .ok();
                }
                .boxed_local(),
            ),
            control => self.layer.on_control(session_id, control, from),
        }
    }

    fn on_request(
        self,
        session_id: Vec<u8>,
        request: Request,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        let fut = async move {
            match request {
                proto::Request {
                    request_id,
                    kind: Some(proto::request::Kind::Session(request)),
                } if self.captures.session_request.should_capture() => {
                    log::info!("Captured `Session` packet from {from}: {request}");
                    self.captures
                        .session_request
                        .tx
                        .send(SessionReqParams {
                            session_id,
                            request_id,
                            from,
                            request,
                        })
                        .await
                        .unwrap();
                }
                request => {
                    self.layer
                        .on_request(session_id, request, from)
                        .map(tokio::task::spawn_local);
                }
            };
        }
        .boxed_local();
        Some(fut)
    }

    fn on_forward(
        self,
        forward: Forward,
        from: SocketAddr,
        session: Option<Arc<DirectSession>>,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        self.layer.on_forward(forward, from, session)
    }
}
