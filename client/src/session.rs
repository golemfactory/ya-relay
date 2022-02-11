use futures::future::LocalBoxFuture;
use futures::SinkExt;
use std::convert::TryInto;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

use crate::dispatch::{Dispatched, Dispatcher};

use ya_relay_core::session::SessionId;
use ya_relay_core::udp_stream::OutStream;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{RequestId, SlotId};
use ya_relay_proto::{codec, proto};

const REGISTER_REQUEST_TIMEOUT: Duration = Duration::from_millis(5000);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);

/// Udp connection to other Node. It is either peer-to-peer connection
/// or central server session. Implements functions for sending
/// protocol messages or forwarding generic messages.
#[derive(Clone)]
pub struct Session {
    pub remote: SocketAddr,
    pub id: SessionId,

    sink: OutStream,
    pub(crate) dispatcher: Dispatcher,
}

impl Session {
    pub fn new(remote_addr: SocketAddr, id: SessionId, sink: OutStream) -> Arc<Self> {
        Arc::new(Self {
            remote: remote_addr,
            id,
            sink,
            dispatcher: Dispatcher::default(),
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
        self.request::<proto::response::Pong>(
            packet.into(),
            self.id.to_vec(),
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await?;

        Ok(())
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
            // Ignoring error, because in such a case, we should disconnect anyway.
            self.ping().await.ok();
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

    /// Will send `Disconnect` message to other Node, to close end Session
    /// more gracefully.  
    pub async fn close(&self) -> anyhow::Result<()> {
        self.disconnect().await
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

        Ok(response.await?)
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

    pub async fn send(&self, packet: impl Into<codec::PacketKind>) -> anyhow::Result<()> {
        let mut sink = self.sink.clone();
        Ok(sink.send((packet.into(), self.remote)).await?)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::trace!("Dropping Session {} ({}).", self.id, self.remote);
    }
}
