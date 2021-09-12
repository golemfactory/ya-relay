use derive_more::From;
use managed::ManagedSlice;
use smoltcp::socket::*;
use smoltcp::wire::{IpEndpoint, IpProtocol, IpVersion};
use std::time::Duration;

use crate::{Protocol, MAX_FRAME_SIZE};

pub const TCP_CONN_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_KEEP_ALIVE: Duration = Duration::from_secs(60);

/// Socket quintuplet
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SocketDesc {
    pub protocol: Protocol,
    pub local: SocketEndpoint,
    pub remote: SocketEndpoint,
}

impl SocketDesc {
    pub fn new(
        protocol: Protocol,
        local: impl Into<SocketEndpoint>,
        remote: impl Into<SocketEndpoint>,
    ) -> Self {
        Self {
            protocol,
            local: local.into(),
            remote: remote.into(),
        }
    }
}

/// Socket endpoint kind
#[derive(From, Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SocketEndpoint {
    Ip(IpEndpoint),
    Icmp(IcmpEndpoint),
    Other,
}

impl PartialEq<IpEndpoint> for SocketEndpoint {
    fn eq(&self, other: &IpEndpoint) -> bool {
        match &self {
            Self::Ip(endpoint) => endpoint == other,
            _ => false,
        }
    }
}

impl From<Option<IpEndpoint>> for SocketEndpoint {
    fn from(opt: Option<IpEndpoint>) -> Self {
        match opt {
            Some(endpoint) => Self::Ip(endpoint),
            None => Self::Other,
        }
    }
}

impl From<u16> for SocketEndpoint {
    fn from(ident: u16) -> Self {
        Self::Icmp(IcmpEndpoint::Ident(ident))
    }
}

/// Common interface for various socket types
pub trait SocketExt {
    fn protocol(&self) -> Protocol;
    fn local_endpoint(&self) -> SocketEndpoint;
    fn remote_endpoint(&self) -> SocketEndpoint;

    fn is_open(&self) -> bool;

    fn can_recv(&self) -> bool;
    fn recv(&mut self) -> std::result::Result<Option<(IpEndpoint, Vec<u8>)>, smoltcp::Error>;

    fn can_send(&self) -> bool;
    fn send_capacity(&self) -> usize;
    fn send_queue(&self) -> usize;
}

impl<'a> SocketExt for Socket<'a> {
    fn protocol(&self) -> Protocol {
        match &self {
            Self::Tcp(_) => Protocol::Tcp,
            Self::Udp(_) => Protocol::Udp,
            Self::Icmp(_) => Protocol::Icmp,
            Self::Raw(_) => Protocol::Ethernet,
        }
    }

    fn local_endpoint(&self) -> SocketEndpoint {
        match &self {
            Self::Tcp(s) => s.local_endpoint().into(),
            Self::Udp(s) => s.endpoint().into(),
            _ => SocketEndpoint::Other,
        }
    }

    fn remote_endpoint(&self) -> SocketEndpoint {
        match &self {
            Self::Tcp(s) => s.remote_endpoint().into(),
            _ => SocketEndpoint::Other,
        }
    }

    fn is_open(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.is_open(),
            Self::Udp(s) => s.is_open(),
            Self::Icmp(s) => s.is_open(),
            Self::Raw(_) => true,
        }
    }

    fn can_recv(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.can_recv(),
            Self::Udp(s) => s.can_recv(),
            Self::Icmp(s) => s.can_recv(),
            Self::Raw(s) => s.can_recv(),
        }
    }

    fn recv(&mut self) -> std::result::Result<Option<(IpEndpoint, Vec<u8>)>, smoltcp::Error> {
        let result = match self {
            Self::Tcp(tcp) => tcp
                .recv(|bytes| (bytes.len(), bytes.to_vec()))
                .map(|vec| (tcp.remote_endpoint(), vec)),
            Self::Udp(udp) => udp
                .recv()
                .map(|(bytes, endpoint)| (endpoint, bytes.to_vec())),
            Self::Icmp(icmp) => icmp
                .recv()
                .map(|(bytes, address)| ((address, 0).into(), bytes.to_vec())),
            Self::Raw(raw) => raw
                .recv()
                .map(|bytes| (IpEndpoint::default(), bytes.to_vec())),
        };

        match result {
            Ok(tuple) => Ok(Some(tuple)),
            Err(smoltcp::Error::Exhausted) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn can_send(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.can_send(),
            Self::Udp(s) => s.can_send(),
            Self::Icmp(s) => s.can_send(),
            Self::Raw(s) => s.can_send(),
        }
    }

    fn send_capacity(&self) -> usize {
        match &self {
            Self::Tcp(s) => s.send_capacity(),
            Self::Udp(s) => s.payload_send_capacity(),
            Self::Icmp(s) => s.payload_send_capacity(),
            Self::Raw(s) => s.payload_send_capacity(),
        }
    }

    fn send_queue(&self) -> usize {
        match &self {
            Self::Tcp(s) => s.send_queue(),
            _ => {
                if self.can_send() {
                    self.send_capacity() // mock value
                } else {
                    0
                }
            }
        }
    }
}

pub fn tcp_socket<'a>() -> TcpSocket<'a> {
    let rx_buf = TcpSocketBuffer::new(vec![0; MAX_FRAME_SIZE * 4]);
    let tx_buf = TcpSocketBuffer::new(vec![0; MAX_FRAME_SIZE * 4]);
    let mut socket = TcpSocket::new(rx_buf, tx_buf);
    socket.set_keep_alive(Some(TCP_KEEP_ALIVE.into()));
    socket
}

pub fn udp_socket<'a>() -> UdpSocket<'a> {
    let rx_buf = UdpSocketBuffer::new(meta_storage(), payload_storage());
    let tx_buf = UdpSocketBuffer::new(meta_storage(), payload_storage());
    UdpSocket::new(rx_buf, tx_buf)
}

pub fn icmp_socket<'a>() -> IcmpSocket<'a> {
    let rx_buf = IcmpSocketBuffer::new(meta_storage(), payload_storage());
    let tx_buf = IcmpSocketBuffer::new(meta_storage(), payload_storage());
    IcmpSocket::new(rx_buf, tx_buf)
}

pub fn raw_socket<'a>(ip_version: IpVersion, ip_protocol: IpProtocol) -> RawSocket<'a> {
    let rx_buf = RawSocketBuffer::new(meta_storage(), payload_storage());
    let tx_buf = RawSocketBuffer::new(meta_storage(), payload_storage());
    RawSocket::new(ip_version, ip_protocol, rx_buf, tx_buf)
}

fn meta_storage<'a, T: Clone>() -> ManagedSlice<'a, T> {
    ManagedSlice::Owned(Vec::new())
}

fn payload_storage<T: Default + Clone>() -> Vec<T> {
    vec![Default::default(); MAX_FRAME_SIZE]
}
