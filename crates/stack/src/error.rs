use crate::socket::SocketEndpoint;
use futures::channel::mpsc::SendError;
use futures::channel::oneshot::Canceled;
use std::net::{AddrParseError, IpAddr};

#[derive(thiserror::Error, Clone, Debug)]
pub enum Error {
    #[error("Invalid IP address: {0}")]
    IpAddr(#[from] AddrParseError),
    #[error("IP address not allowed: {0}")]
    IpAddrNotAllowed(IpAddr),
    #[error("IP address is malformed: {0}")]
    IpAddrMalformed(String),
    #[error("IP address is taken: {0}")]
    IpAddrTaken(IpAddr),
    #[error("Invalid network IP address: {0}")]
    NetAddr(String),
    #[error("Network IP address taken: {0}")]
    NetAddrTaken(IpAddr),
    #[error("Network not found for IP address: {0}")]
    NetAddrMismatch(IpAddr),
    #[error("Network is empty")]
    NetEmpty,
    #[error("Network not found")]
    NetNotFound,
    #[error("Invalid network CIDR: {0}/{1}")]
    NetCidr(IpAddr, u8),
    #[error("Network ID taken: {0}")]
    NetIdTaken(String),
    #[error("Invalid gateway address: {0}")]
    GatewayMismatch(IpAddr),
    #[error("Packet malformed: {0}")]
    PacketMalformed(String),
    #[error("Protocol not supported: {0}")]
    ProtocolNotSupported(String),
    #[error("Unknown protocol")]
    ProtocolUnknown,
    #[error("Endpoint taken: {0:?}")]
    EndpointTaken(SocketEndpoint),
    #[error("Invalid endpoint: {0:?}")]
    EndpointInvalid(SocketEndpoint),
    #[error("Socket closed")]
    SocketClosed,
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("Connection timed out")]
    ConnectionTimeout,
    #[error("Forbidden")]
    Forbidden,
    #[error("Cancelled")]
    Cancelled,

    #[error("Queue closed")]
    QueueClosed,
    #[error("{0}")]
    Other(String),
}

impl From<Canceled> for Error {
    fn from(_: Canceled) -> Self {
        Self::Cancelled
    }
}

impl From<SendError> for Error {
    fn from(_: SendError) -> Self {
        Self::Cancelled
    }
}
