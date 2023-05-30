use std::collections::HashMap;
use std::convert::TryInto;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::channel::oneshot::{self, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use futures::stream::{Stream, StreamExt};
use tokio::task::spawn_local;
use tokio::time::{Duration, Instant};

use crate::_routing_session::DirectSession;
use crate::_session::RawSession;

use ya_relay_proto::codec;
use ya_relay_proto::proto::{self, RequestId};

pub type ErrorHandler = Box<dyn Fn() -> ErrorHandlerResult + Send>;
pub type ErrorHandlerResult = Pin<Box<dyn Future<Output = ()> + Send>>;
type ResponseSender = Sender<Dispatched<proto::response::Kind>>;

/// Signals receipt of a response packet
pub async fn dispatch<H, S>(handler: H, mut stream: S)
where
    H: Handler + Clone + 'static,
    S: Stream<Item = (codec::PacketKind, SocketAddr, chrono::DateTime<chrono::Utc>)> + Unpin,
{
    while let Some((packet, from, _timestamp)) = stream.next().await {
        let handler = handler.clone();

        // First look for existing session, but if it doesn't exist, maybe we
        // can find dispatcher from temporary session that is being initialized at this moment.
        let session = handler.session(from).await;
        let dispatcher = match session.clone() {
            Some(session) => Some(session.raw.clone()),
            None => handler.dispatcher(from).await,
        };

        if let Some(ref dispatcher) = dispatcher {
            dispatcher.dispatcher.update_seen();
        }

        match packet {
            codec::PacketKind::Packet(proto::Packet {
                session_id,
                kind: Some(kind),
            }) => match kind {
                proto::packet::Kind::Control(control) => {
                    handler
                        .on_control(session_id, control, from)
                        .map(spawn_local);
                }
                proto::packet::Kind::Request(request) => {
                    handler
                        .on_request(session_id, request, from)
                        .map(spawn_local);
                }
                proto::packet::Kind::Response(response) => {
                    match response.kind {
                        Some(kind) => match dispatcher {
                            Some(dispatcher) => dispatcher.dispatcher.dispatch_response(
                                from,
                                response.request_id,
                                session_id,
                                response.code,
                                kind,
                            ),
                            None => log::debug!("Unexpected response from {from}: {kind:?}"),
                        },
                        // TODO: Handle empty packet kind here
                        None => log::debug!("Empty response kind from: {from}"),
                    }
                }
            },
            codec::PacketKind::Forward(forward) => {
                // In case of temporary sessions we shouldn't get `Forward` packets,
                // so we can safely ignore this case.
                if let Some(session) = session {
                    handler
                        .on_forward(forward, from, Some(session))
                        .map(spawn_local);
                }
            }
            _ => log::warn!("Unable to dispatch packet from {from}: not supported"),
        };
    }

    log::info!("Dispatcher stopped");
}

/// Handles incoming packets. Used exclusively by the `dispatch` function
pub trait Handler {
    /// Returns a clone of a `Dispatcher` object for temporary sessions.
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<RawSession>>>;

    /// Returns established session, if it exists. Otherwise returns None.
    /// It doesn't take into account temporary sessions, so you should call `Handler::dispatcher`
    /// later.
    fn session(&self, from: SocketAddr) -> LocalBoxFuture<Option<Arc<DirectSession>>>;

    /// Handles `proto::Control` packets
    fn on_control(
        self,
        session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>>;

    /// Handles `proto::Request` packets
    fn on_request(
        self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> Option<LocalBoxFuture<'static, ()>>;

    /// Handles `proto::Forward` packets
    fn on_forward(
        self,
        forward: proto::Forward,
        from: SocketAddr,
        session: Option<Arc<DirectSession>>,
    ) -> Option<LocalBoxFuture<'static, ()>>;
}

/// Dispatched packet wrapper
pub struct Dispatched<T> {
    pub session_id: Vec<u8>,
    pub code: i32,
    pub packet: T,
}

/// Facility for dispatching and awaiting response packets
#[derive(Clone)]
pub struct Dispatcher {
    seen: Arc<Mutex<Instant>>,
    ping: Arc<Mutex<Duration>>,
    responses: Arc<Mutex<HashMap<u64, ResponseSender>>>,
    error_handlers: Arc<Mutex<HashMap<i32, ErrorHandler>>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Instant::now())),
            ping: Arc::new(Mutex::new(Duration::MAX)),
            responses: Default::default(),
            error_handlers: Default::default(),
        }
    }
}

impl Dispatcher {
    pub fn update_seen(&self) {
        *self.seen.lock().unwrap() = Instant::now();
    }

    pub fn update_ping(&self, ping: Duration) {
        *self.ping.lock().unwrap() = ping;
    }

    pub fn last_seen(&self) -> Instant {
        *self.seen.lock().unwrap()
    }

    pub fn last_ping(&self) -> Duration {
        *self.ping.lock().unwrap()
    }

    /// Registers a response code handler
    pub fn handle_error<F: Fn() -> ErrorHandlerResult + Sync + Send + 'static>(
        &self,
        code: i32,
        exclusive: bool,
        handler: F,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering::SeqCst;

        let handler_fn = Arc::new(Box::new(handler));
        let latch = Arc::new(AtomicBool::new(false));

        let handler = Box::new(move || {
            let handler_fn = handler_fn.clone();
            let latch = latch.clone();

            async move {
                if exclusive && latch.load(SeqCst) {
                    return;
                }

                latch.store(true, SeqCst);
                handler_fn().await;
                latch.store(false, SeqCst);
            }
            .boxed()
        });

        let mut handlers = self.error_handlers.lock().unwrap();
        handlers.insert(code, handler);
    }

    /// Creates a future to await a `T` (response) packet on
    pub fn response<'a, T: 'static>(
        &self,
        request_id: RequestId,
        timeout: Duration,
    ) -> LocalBoxFuture<'a, anyhow::Result<Dispatched<T>>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
    {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        let request_id_ = request_id;
        if self
            .responses
            .lock()
            .unwrap()
            .insert(request_id, tx)
            .is_some()
        {
            log::warn!("Duplicate dispatch request id: {request_id}");
        }

        async move {
            let response = tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| anyhow::anyhow!("Request timed out after {} ms", timeout.as_millis()))?
                .map_err(|_| anyhow::anyhow!("Request cancelled"))?;

            if response.code != proto::StatusCode::Ok as i32 {
                anyhow::bail!("Request failed with code {}", response.code);
            }

            let packet: T = response
                .packet
                .try_into()
                .map_err(|_| anyhow::anyhow!("Unexpected response type"))?;

            Ok(Dispatched {
                session_id: response.session_id,
                code: response.code,
                packet,
            })
        }
        .then(move |result| async move {
            this.responses.lock().unwrap().remove(&request_id_);
            result
        })
        .boxed_local()
    }

    /// Awakes futures awaiting packet variants of `proto::response::Kind`
    pub fn dispatch_response(
        &self,
        from: SocketAddr,
        request_id: RequestId,
        session_id: Vec<u8>,
        code: i32,
        kind: proto::response::Kind,
    ) {
        match { self.responses.lock().unwrap().remove(&request_id) } {
            Some(sender) => {
                if sender
                    .send(Dispatched {
                        session_id,
                        code,
                        packet: kind,
                    })
                    .is_err()
                {
                    log::debug!("Unable to dispatch response (request_id = {request_id}) from {from}: listener is closed");
                }
            }
            None => log::debug!(
                "Unable to dispatch response (request_id = {request_id}) from {from}: listener does not exist. {kind:?}"
            ),
        };

        if code != proto::StatusCode::Ok as i32 {
            let handlers = self.error_handlers.lock().unwrap();
            if let Some(handler) = handlers.get(&code) {
                spawn_local((*handler)());
            }
        }
    }
}
