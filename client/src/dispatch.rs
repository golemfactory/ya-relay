use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::net::SocketAddr;
use std::rc::Rc;

use futures::channel::oneshot::{self, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use futures::stream::{Stream, StreamExt};
use tokio::time::{Duration, Instant};

use ya_relay_proto::codec;
use ya_relay_proto::proto::{self, RequestId};

type ResponseSender = Sender<Dispatched<proto::response::Kind>>;

/// Signals receipt of a response packet
pub async fn dispatch<H, S>(handler: H, mut stream: S)
where
    H: Handler + Clone + 'static,
    S: Stream<Item = (codec::PacketKind, SocketAddr)> + Unpin,
{
    while let Some((packet, from)) = stream.next().await {
        let handler = handler.clone();
        tokio::task::spawn_local(async move {
            if let Some(dispatcher) = handler.dispatcher(from).await {
                dispatcher.update_seen();
            }

            match packet {
                codec::PacketKind::Packet(proto::Packet {
                    session_id,
                    kind: Some(kind),
                }) => match kind {
                    proto::packet::Kind::Control(control) => {
                        handler.on_control(session_id, control, from).await;
                    }
                    proto::packet::Kind::Request(request) => {
                        handler.on_request(session_id, request, from).await;
                    }
                    proto::packet::Kind::Response(response) => {
                        dispatch_response(session_id, response, from, &handler).await;
                    }
                },
                codec::PacketKind::Forward(forward) => {
                    handler.on_forward(forward, from).await;
                }
                _ => log::warn!("Unable to dispatch packet from {}: not supported", from),
            }
        });
    }
    log::info!("Client stopped");
}

#[inline(always)]
async fn dispatch_response<H>(
    session_id: Vec<u8>,
    response: proto::Response,
    from: SocketAddr,
    handler: &H,
) where
    H: Handler,
{
    // TODO: We don't handle errors without packet kind here.
    match response.kind {
        Some(kind) => match handler.dispatcher(from).await {
            Some(dispatcher) => {
                dispatcher.dispatch_response(response.request_id, session_id, response.code, kind)
            }
            None => log::warn!("Unexpected response from {}: {:?}", from, kind),
        },
        None => log::debug!("Empty response kind from: {}", from),
    }
}

/// Handles incoming packets. Used exclusively by the `dispatch` function
pub trait Handler {
    /// Returns a clone of a `Dispatcher` object
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>>;

    /// Handles `proto::Control` packets
    fn on_control(
        &self,
        session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> LocalBoxFuture<()>;

    /// Handles `proto::Request` packets
    fn on_request(
        &self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> LocalBoxFuture<()>;

    /// Handles `proto::Forward` packets
    fn on_forward(&self, forward: proto::Forward, from: SocketAddr) -> LocalBoxFuture<()>;
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
    seen: Rc<RefCell<Instant>>,
    responses: Rc<RefCell<HashMap<u64, ResponseSender>>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self {
            seen: Rc::new(RefCell::new(Instant::now())),
            responses: Default::default(),
        }
    }
}

impl Dispatcher {
    pub fn update_seen(&self) {
        *self.seen.borrow_mut() = Instant::now();
    }

    pub fn last_seen(&self) -> Instant {
        *self.seen.borrow()
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
        if self.responses.borrow_mut().insert(request_id, tx).is_some() {
            log::warn!("Duplicate dispatch request id: {}", request_id);
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
            this.responses.borrow_mut().remove(&request_id_);
            result
        })
        .boxed_local()
    }

    /// Awakes futures awaiting packet variants of `proto::response::Kind`
    pub fn dispatch_response(
        &self,
        request_id: RequestId,
        session_id: Vec<u8>,
        code: i32,
        kind: proto::response::Kind,
    ) {
        match { self.responses.borrow_mut().remove(&request_id) } {
            Some(sender) => {
                if sender
                    .send(Dispatched {
                        session_id,
                        code,
                        packet: kind,
                    })
                    .is_err()
                {
                    log::warn!("Unable to dispatch response: listener is closed");
                }
            }
            None => log::warn!(
                "Unable to dispatch response: listener does not exist. {:?}",
                kind
            ),
        }
    }
}
