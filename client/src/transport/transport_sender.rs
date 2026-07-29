use async_trait::async_trait;
use bytes::Bytes;
use derive_more::From;

use super::tcp_registry::TcpSender;
use crate::error::SenderError;
use crate::routing_session::RoutingSender;

use ya_relay_core::server_session::TransportType;
use ya_relay_proto::codec::forward::encode;
use ya_relay_proto::proto::Payload;

#[async_trait(?Send)]
pub trait GenericSender: Sized {
    /// Sends Payload to target Node. Creates session and/or tcp connection if it didn't exist.
    async fn send(&mut self, packet: Payload) -> Result<(), SenderError>;

    /// Establishes connection on demand if it didn't exist.
    /// This function can be used to prepare connection before later use.
    /// Thanks to this we can return early if Node is unreachable in the network.
    async fn connect(&mut self) -> Result<(), SenderError>;

    /// Closes connection to Node.
    async fn disconnect(&mut self) -> Result<(), SenderError>;
}

/// `TcpSender` processes packets as stream of bytes. `FramedSender` adds frames
/// abstraction to the stream, to distinguish separate packets.
#[derive(Clone)]
pub struct FramedSender {
    sender: TcpSender,
}

/// Wraps different kinds of senders exposing common interface.
#[derive(From, Clone)]
pub enum ForwardSender {
    Unreliable(RoutingSender),
    Reliable(TcpSender),
    Framed(FramedSender),
}

impl ForwardSender {
    pub fn framed(self) -> ForwardSender {
        match self {
            ForwardSender::Reliable(sender) => FramedSender { sender }.into(),
            // `SenderKind::Unreliable` won't be converted.
            // `SenderKind::Framed` is already ok.
            sender => sender,
        }
    }

    /// Sends a payload to the target node.
    pub async fn send(&mut self, packet: Payload) -> Result<(), SenderError> {
        self.send_payload(packet).await
    }

    /// Sends an immutable, cheaply cloneable byte buffer to the target node.
    pub async fn send_bytes(&mut self, packet: Bytes) -> Result<(), SenderError> {
        self.send_payload(packet.into()).await
    }

    /// Establishes the underlying connection on demand.
    pub async fn connect(&mut self) -> Result<(), SenderError> {
        self.connect_inner().await
    }

    /// Closes the underlying connection.
    pub async fn disconnect(&mut self) -> Result<(), SenderError> {
        self.disconnect_inner().await
    }

    async fn send_payload(&mut self, packet: Payload) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => {
                Ok(sender.send(packet, TransportType::Unreliable).await?)
            }
            ForwardSender::Reliable(sender) => Ok(sender.send(packet).await?),
            ForwardSender::Framed(sender) => GenericSender::send(sender, packet).await,
        }
    }

    async fn connect_inner(&mut self) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => Ok(sender.connect().await?),
            ForwardSender::Reliable(sender) => Ok(sender.connect().await?),
            ForwardSender::Framed(sender) => GenericSender::connect(sender).await,
        }
    }

    async fn disconnect_inner(&mut self) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => Ok(sender.disconnect().await?),
            ForwardSender::Reliable(sender) => Ok(sender.disconnect().await?),
            ForwardSender::Framed(sender) => GenericSender::disconnect(sender).await,
        }
    }
}

#[async_trait(?Send)]
impl GenericSender for ForwardSender {
    async fn send(&mut self, packet: Payload) -> Result<(), SenderError> {
        self.send_payload(packet).await
    }

    async fn connect(&mut self) -> Result<(), SenderError> {
        self.connect_inner().await
    }

    async fn disconnect(&mut self) -> Result<(), SenderError> {
        self.disconnect_inner().await
    }
}

#[async_trait(?Send)]
impl GenericSender for FramedSender {
    async fn send(&mut self, packet: Payload) -> Result<(), SenderError> {
        Ok(self.sender.send(encode(packet)).await?)
    }

    async fn connect(&mut self) -> Result<(), SenderError> {
        Ok(self.sender.connect().await?)
    }

    async fn disconnect(&mut self) -> Result<(), SenderError> {
        Ok(self.sender.disconnect().await?)
    }
}
