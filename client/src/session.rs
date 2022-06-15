use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt};
use std::cell::RefCell;
use std::convert::TryInto;
use std::fmt;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

use crate::dispatch::{Dispatched, Dispatcher};

use ya_relay_core::session::SessionId;
use ya_relay_core::sync::Actuator;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{RequestId, SlotId};
use ya_relay_proto::{codec, proto};

const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);
const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(2);

pub type SessionResult<T> = Result<T, SessionError>;
pub type DropHandler = Box<dyn FnOnce()>;

/// Udp connection to other Node. It is either peer-to-peer connection
/// or central server session. Implements functions for sending
/// protocol messages or forwarding generic messages.
#[derive(Clone)]
pub struct Session {
    pub remote: SocketAddr,
    pub id: SessionId,
    pub created: Instant,

    sink: OutStream,
    pub(crate) dispatcher: Dispatcher,
    pub(crate) drop_handler: Rc<RefCell<Option<DropHandler>>>,
    pub(crate) forward_pause: Actuator,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionDesc {
    pub remote: SocketAddr,
    pub id: SessionId,
    pub last_seen: std::time::Instant,
    pub last_ping: std::time::Duration,
    pub created: std::time::Instant,
}

impl<'a> From<&'a Session> for SessionDesc {
    fn from(session: &'a Session) -> Self {
        SessionDesc {
            remote: session.remote,
            id: session.id,
            last_seen: session.dispatcher.last_seen().into_std(),
            last_ping: session.dispatcher.last_ping(),
            created: session.created.into_std(),
        }
    }
}

impl Session {
    pub fn new(remote_addr: SocketAddr, id: SessionId, sink: OutStream) -> Arc<Self> {
        Arc::new(Self {
            remote: remote_addr,
            id,
            sink,
            created: Instant::now(),
            dispatcher: Dispatcher::default(),
            drop_handler: Default::default(),
            forward_pause: Default::default(),
        })
    }

    pub fn dispatcher(&self) -> Dispatcher {
        self.dispatcher.clone()
    }

    pub async fn register_endpoints(
        &self,
        endpoints: Vec<proto::Endpoint>,
    ) -> anyhow::Result<Vec<proto::Endpoint>> {
        log::info!("[{}] registering endpoints.", self.id);

        let response = self
            .request::<proto::response::Register>(
                proto::request::Register { endpoints }.into(),
                self.id.to_vec(),
                REGISTER_REQUEST_TIMEOUT,
            )
            .await?
            .packet;

        log::info!("[{}] endpoints registration finished.", self.id);

        Ok(response.endpoints)
    }

    pub async fn find_node(&self, node_id: NodeId) -> anyhow::Result<proto::response::Node> {
        log::debug!(
            "Finding Node info [{}], using session {}.",
            node_id,
            self.id
        );

        let packet = proto::request::Node {
            node_id: node_id.into_array().to_vec(),
            public_key: true,
        };
        self.find_node_by(packet).await
    }

    pub async fn find_slot(&self, slot: SlotId) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Slot {
            slot,
            public_key: true,
        };
        self.find_node_by(packet).await
    }

    async fn find_node_by(
        &self,
        packet: impl Into<proto::Request>,
    ) -> anyhow::Result<proto::response::Node> {
        let response = self
            .request::<proto::response::Node>(
                packet.into(),
                self.id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?
            .packet;
        Ok(response)
    }

    pub async fn neighbours(&self, count: u32) -> anyhow::Result<proto::response::Neighbours> {
        let packet = proto::request::Neighbours {
            count,
            public_key: true,
        };
        let neighbours = self
            .request::<proto::response::Neighbours>(
                packet.into(),
                self.id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?
            .packet;
        Ok(neighbours)
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let packet = proto::request::Ping {};
        let ping_ts = Instant::now();

        let (ping, result) = match self
            .request::<proto::response::Pong>(packet.into(), self.id.to_vec(), DEFAULT_PING_TIMEOUT)
            .await
        {
            result @ Ok(_) => (Instant::now() - ping_ts, result),
            result @ Err(_) => (Instant::now() - self.dispatcher.last_seen(), result),
        };

        self.dispatcher.update_ping(ping);
        result.map(|_| ())
    }

    pub async fn reverse_connection(&self, node_id: NodeId) -> anyhow::Result<()> {
        let packet = proto::request::ReverseConnection {
            node_id: node_id.into_array().to_vec(),
        };
        self.request::<proto::response::ReverseConnection>(
            packet.into(),
            self.id.to_vec(),
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await?;

        Ok(())
    }

    /// Check if any packet was seen during expiration period.
    /// If it wasn't, ping will be sent.
    /// Function returns timestamp of last seen packet from remote Node,
    /// including ping that can be sent.
    pub async fn keep_alive(&self, expiration: Duration) -> Instant {
        let last_seen = self.dispatcher.last_seen();

        if last_seen + expiration < Instant::now() {
            // Sending 3 pings after each other to avoid lost UDP packets.
            // We need only one response.
            // Note: futures are asynchronous, because we shouldn't wait for ping timeout
            //       in case of lost packets.
            futures::future::select_ok((0..3).into_iter().map(|i| {
                async move {
                    tokio::time::sleep(Duration::from_millis(200 * i)).await;
                    self.ping().await
                }
                .boxed_local()
            }))
            .await
            // Ignoring error, because in such a case, we should disconnect anyway.
            .ok();
        }

        // Could be updated after ping.
        self.dispatcher.last_seen()
    }

    pub async fn disconnect(&self) -> anyhow::Result<()> {
        // Don't use temporary session, because we don't want to initialize session
        // with this address, nor receive the response.
        let control_packet = proto::Packet::control(
            self.id.to_vec(),
            ya_relay_proto::proto::control::Disconnected {
                by: Some(proto::control::disconnected::By::SessionId(
                    self.id.to_vec(),
                )),
            },
        );
        self.send(control_packet).await
    }

    #[inline]
    pub async fn pause_forwarding(&self) {
        self.forward_pause.enable();
    }

    #[inline]
    pub async fn resume_forwarding(&self) {
        self.forward_pause.disable();
    }

    /// Will send `Disconnect` message to other Node, to close end Session
    /// more gracefully.  
    pub async fn close(&self) -> anyhow::Result<()> {
        self.disconnect().await
    }

    /// Add a function that will be executed on session drop
    pub fn on_drop<F>(&self, f: F)
    where
        F: FnOnce() + 'static,
    {
        self.drop_handler.borrow_mut().replace(Box::new(f));
    }
}

impl Session {
    pub(crate) async fn request<T>(
        &self,
        request: proto::Request,
        session_id: Vec<u8>,
        timeout: Duration,
    ) -> anyhow::Result<Dispatched<T>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let response = self.response::<T>(request.request_id, timeout);
        let packet = proto::Packet {
            session_id,
            kind: Some(proto::packet::Kind::Request(request)),
        };
        self.send(packet).await?;

        response.await
    }

    #[inline(always)]
    pub(crate) fn response<'a, T>(
        &self,
        request_id: RequestId,
        timeout: Duration,
    ) -> LocalBoxFuture<'a, anyhow::Result<Dispatched<T>>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        self.dispatcher.clone().response::<T>(request_id, timeout)
    }

    #[inline]
    pub async fn send(&self, packet: impl Into<codec::PacketKind>) -> anyhow::Result<()> {
        let mut sink = self.sink.clone();
        Ok(sink.send((packet.into(), self.remote)).await?)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::trace!("Dropping Session {} ({}).", self.id, self.remote);

        let on_drop = self.drop_handler.borrow_mut().take();
        if let Some(f) = on_drop {
            f()
        }
    }
}

#[derive(thiserror::Error, Clone)]
pub enum SessionError {
    Drop(String),
    Retry(String),
}

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        Self::Retry(e.to_string())
    }
}

impl fmt::Debug for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drop(e) => {
                write!(f, "Drop({:?})", e)
            }
            Self::Retry(e) => {
                write!(f, "Retry({:?})", e)
            }
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drop(e) | Self::Retry(e) => e.fmt(f),
        }
    }
}
