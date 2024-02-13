use async_trait::async_trait;
use bytes::Bytes;
use derive_more::From;

use super::tcp_registry::TcpSender;
use crate::error::SenderError;
use crate::routing_session::RoutingSender;

use ya_relay_core::server_session::TransportType;
use ya_relay_proto::codec::forward::encode;
use ya_relay_proto::proto::Payload;

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
}

impl  ForwardSender {
    pub async fn send(&mut self, packet: Bytes) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => {
                Ok(sender.send(packet, TransportType::Unreliable).await?)
            }
            ForwardSender::Reliable(sender) => Ok(sender.send(packet).await?),
            ForwardSender::Framed(sender) => sender.send(packet).await,
        }
    }

    pub async fn connect(&mut self) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => Ok(sender.connect().await?),
            ForwardSender::Reliable(sender) => Ok(sender.connect().await?),
            ForwardSender::Framed(sender) => sender.connect().await,
        }
    }

    pub async fn disconnect(&mut self) -> Result<(), SenderError> {
        match self {
            ForwardSender::Unreliable(sender) => Ok(sender.disconnect().await?),
            ForwardSender::Reliable(sender) => Ok(sender.disconnect().await?),
            ForwardSender::Framed(sender) => sender.disconnect().await,
        }
    }
}


impl  FramedSender {
    async fn send(&mut self, packet: Bytes) -> Result<(), SenderError> {
        Ok(self.sender.send(encode(packet)).await?)
    }

    async fn connect(&mut self) -> Result<(), SenderError> {
        Ok(self.sender.connect().await?)
    }

    async fn disconnect(&mut self) -> Result<(), SenderError> {
        Ok(self.sender.disconnect().await?)
    }
}
