use derive_more::Display;
use std::hash::{Hash, Hasher};

use derive_more::From;
use ya_smoltcp::socket::*;
use ya_smoltcp::storage::PacketMetadata;
use ya_smoltcp::time::Duration;
use ya_smoltcp::wire::{IpAddress, IpEndpoint, IpProtocol, IpVersion};

use crate::{Error, Protocol};

pub const ENV_VAR_TCP_TIMEOUT: &str = "YA_NET_TCP_TIMEOUT_MS";
pub const ENV_VAR_TCP_KEEP_ALIVE: &str = "YA_NET_TCP_KEEP_ALIVE_MS";
pub const ENV_VAR_TCP_ACK_DELAY: &str = "YA_NET_TCP_ACK_DELAY_MS";
pub const ENV_VAR_TCP_NAGLE: &str = "YA_NET_TCP_ACK_DELAY";

pub const TCP_CONN_TIMEOUT: Duration = Duration::from_secs(8);
pub const TCP_DISCONN_TIMEOUT: Duration = Duration::from_secs(2);
const META_STORAGE_SIZE: usize = 1024;

lazy_static::lazy_static! {
    pub static ref TCP_NAGLE_ENABLED: bool = env_opt(ENV_VAR_TCP_NAGLE, |v| v != 0)
        .flatten()
        .unwrap_or(false);
    pub static ref TCP_TIMEOUT: Option<Duration> = env_opt(ENV_VAR_TCP_TIMEOUT, Duration::from_millis)
        .unwrap_or(Some(Duration::from_secs(120)));
    pub static ref TCP_KEEP_ALIVE: Option<Duration> = env_opt(ENV_VAR_TCP_KEEP_ALIVE, Duration::from_millis)
        .unwrap_or(Some(Duration::from_secs(30)));
    pub static ref TCP_ACK_DELAY: Option<Duration> = env_opt(ENV_VAR_TCP_ACK_DELAY, Duration::from_millis)
        .unwrap_or(Some(Duration::from_millis(40)));
}

fn env_opt<T, F: FnOnce(u64) -> T>(var: &str, f: F) -> Option<Option<T>> {
    std::env::var(var)
        .ok()
        .map(|v| v.parse::<u64>().map(f).ok())
}

/// Socket quintuplet
#[derive(Clone, Display, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[display(
    fmt = "SocketDesc {{ protocol: {}, local: {}, remote: {} }}",
    protocol,
    local,
    remote
)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketState<T> {
    Tcp { state: TcpState, inner: T },
    Other { inner: T },
}

impl<T> SocketState<T> {
    pub fn inner_mut(&mut self) -> &mut T {
        match self {
            Self::Tcp { inner, .. } | Self::Other { inner } => inner,
        }
    }

    pub fn set_inner(&mut self, value: T) {
        match self {
            Self::Tcp { inner, .. } | Self::Other { inner } => *inner = value,
        }
    }
}

impl<T: Default> From<TcpState> for SocketState<T> {
    fn from(state: TcpState) -> Self {
        Self::Tcp {
            state,
            inner: Default::default(),
        }
    }
}

impl<T: Default> Default for SocketState<T> {
    fn default() -> Self {
        SocketState::Other {
            inner: Default::default(),
        }
    }
}

impl<T> ToString for SocketState<T> {
    fn to_string(&self) -> String {
        match self {
            Self::Tcp { state, .. } => format!("{:?}", state),
            _ => String::default(),
        }
    }
}

/// Socket endpoint kind
#[derive(From, Display, Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SocketEndpoint {
    Ip(IpEndpoint),
    #[display(fmt = "{:?}", _0)]
    Icmp(IcmpEndpoint),
    Other,
}

impl SocketEndpoint {
    #[inline]
    pub fn ip_endpoint(&self) -> Result<IpEndpoint, Error> {
        match self {
            SocketEndpoint::Ip(endpoint) => Ok(*endpoint),
            other => Err(Error::EndpointInvalid(*other)),
        }
    }

    #[inline]
    pub fn is_specified(&self) -> bool {
        match self {
            Self::Ip(ip) => ip.is_specified(),
            Self::Icmp(icmp) => icmp.is_specified(),
            Self::Other => false,
        }
    }

    pub fn addr_repr(&self) -> String {
        match self {
            Self::Ip(ip) => format!("{}", ip.addr),
            _ => Default::default(),
        }
    }

    pub fn port_repr(&self) -> String {
        match self {
            Self::Ip(ip) => format!("{}", ip.port),
            Self::Icmp(icmp) => match icmp {
                IcmpEndpoint::Unspecified => "*".to_string(),
                endpoint => format!("{:?}", endpoint),
            },
            Self::Other => Default::default(),
        }
    }
}

#[allow(clippy::derived_hash_with_manual_eq)]
impl Hash for SocketEndpoint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Ip(ip) => {
                state.write_u8(1);
                ip.hash(state);
            }
            Self::Icmp(icmp) => {
                state.write_u8(2);
                match icmp {
                    IcmpEndpoint::Unspecified => state.write_u8(1),
                    IcmpEndpoint::Udp(ip) => {
                        state.write_u8(2);
                        ip.hash(state);
                    }
                    IcmpEndpoint::Ident(id) => {
                        state.write_u8(3);
                        id.hash(state);
                    }
                }
            }
            Self::Other => state.write_u8(3),
        }
    }
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

impl<T: Into<IpAddress>> From<(T, u16)> for SocketEndpoint {
    fn from((t, port): (T, u16)) -> Self {
        let endpoint: IpEndpoint = (t, port).into();
        Self::from(endpoint)
    }
}

/// Common interface for various socket types
pub trait SocketExt {
    fn protocol(&self) -> Protocol;
    fn local_endpoint(&self) -> SocketEndpoint;
    fn remote_endpoint(&self) -> SocketEndpoint;

    fn is_closed(&self) -> bool;
    fn close(&mut self);

    fn can_recv(&self) -> bool;
    fn recv(&mut self) -> std::result::Result<Option<(IpEndpoint, Vec<u8>)>, ya_smoltcp::Error>;

    fn can_send(&self) -> bool;
    fn send_capacity(&self) -> usize;
    fn send_queue(&self) -> usize;

    fn state<T: Default>(&self) -> SocketState<T>;
    fn desc(&self) -> SocketDesc;
}

impl<'a> SocketExt for Socket<'a> {
    fn protocol(&self) -> Protocol {
        match &self {
            Self::Tcp(_) => Protocol::Tcp,
            Self::Udp(_) => Protocol::Udp,
            Self::Icmp(_) => Protocol::Icmp,
            Self::Raw(_) => Protocol::Ethernet,
            Self::Dhcpv4(_) => Protocol::None,
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

    fn is_closed(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.state() == TcpState::Closed,
            Self::Udp(s) => !s.is_open(),
            Self::Icmp(s) => !s.is_open(),
            Self::Raw(_) => false,
            Self::Dhcpv4(_) => false,
        }
    }

    fn close(&mut self) {
        match self {
            Self::Tcp(s) => s.close(),
            Self::Udp(s) => s.close(),
            _ => (),
        }
    }

    fn can_recv(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.can_recv(),
            Self::Udp(s) => s.can_recv(),
            Self::Icmp(s) => s.can_recv(),
            Self::Raw(s) => s.can_recv(),
            Self::Dhcpv4(_) => false,
        }
    }

    fn recv(&mut self) -> std::result::Result<Option<(IpEndpoint, Vec<u8>)>, ya_smoltcp::Error> {
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
            Self::Dhcpv4(_) => Err(ya_smoltcp::Error::Exhausted),
        };

        match result {
            Ok(tuple) => Ok(Some(tuple)),
            Err(ya_smoltcp::Error::Exhausted) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn can_send(&self) -> bool {
        match &self {
            Self::Tcp(s) => s.can_send(),
            Self::Udp(s) => s.can_send(),
            Self::Icmp(s) => s.can_send(),
            Self::Raw(s) => s.can_send(),
            Self::Dhcpv4(_) => false,
        }
    }

    fn send_capacity(&self) -> usize {
        match &self {
            Self::Tcp(s) => s.send_capacity(),
            Self::Udp(s) => s.payload_send_capacity(),
            Self::Icmp(s) => s.payload_send_capacity(),
            Self::Raw(s) => s.payload_send_capacity(),
            Self::Dhcpv4(_) => 0,
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

    fn state<T: Default>(&self) -> SocketState<T> {
        match &self {
            Self::Tcp(s) => SocketState::from(s.state()),
            _ => SocketState::Other {
                inner: Default::default(),
            },
        }
    }

    fn desc(&self) -> SocketDesc {
        SocketDesc {
            protocol: self.protocol(),
            local: self.local_endpoint(),
            remote: self.remote_endpoint(),
        }
    }
}

pub trait TcpSocketExt {
    fn set_defaults(&mut self);
}

impl<'a> TcpSocketExt for TcpSocket<'a> {
    fn set_defaults(&mut self) {
        self.set_nagle_enabled(*TCP_NAGLE_ENABLED);
        self.set_timeout(*TCP_TIMEOUT);
        self.set_keep_alive(*TCP_KEEP_ALIVE);
        self.set_ack_delay(*TCP_ACK_DELAY);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SocketMemory {
    pub tx: Memory,
    pub rx: Memory,
}

impl SocketMemory {
    pub fn default_tcp() -> Self {
        Self {
            rx: Memory::default_tcp_rx(),
            tx: Memory::default_tcp_tx(),
        }
    }

    pub fn default_udp() -> Self {
        Self {
            rx: Memory::default_udp_rx(),
            tx: Memory::default_udp_tx(),
        }
    }

    pub fn default_icmp() -> Self {
        Self::default_udp()
    }

    pub fn default_raw() -> Self {
        Self::default_tcp()
    }
}

/// Buffer size bounds used in auto tuning.
/// Currently, only `max` is used; other values are reserved for future use
#[derive(Clone, Copy, Debug)]
pub struct Memory {
    min: usize,
    default: usize,
    max: usize,
}

impl Memory {
    pub fn new(min: usize, default: usize, max: usize) -> Result<Self, Error> {
        if default < min || default > max {
            return Err(Error::Other(format!(
                "Invalid memory bounds: {min} <= {default} <= {max}",
            )));
        }
        Ok(Self { min, default, max })
    }

    pub fn set_min(&mut self, min: usize) -> Result<(), Error> {
        if min > self.default {
            return Err(Error::Other(format!(
                "Invalid min memory bound: {min} <= {}",
                self.default
            )));
        }

        self.min = min;
        Ok(())
    }

    pub fn set_default(&mut self, default: usize) -> Result<(), Error> {
        if default < self.min || default > self.max {
            return Err(Error::Other(format!(
                "Invalid default memory size: {} <= {default} <= {}",
                self.min, self.max,
            )));
        }

        self.default = default;
        Ok(())
    }

    pub fn set_max(&mut self, max: usize) -> Result<(), Error> {
        if max < self.default {
            return Err(Error::Other(format!(
                "Invalid max memory bound: {} <= {max}",
                self.default
            )));
        }

        self.max = max;
        Ok(())
    }

    pub fn default_tcp_rx() -> Self {
        // 5.15.0-39-generic #42-Ubuntu SMP
        // net.ipv4.tcp_rmem = 4096	131072	6291456
        Self::new(4 * 1024, 128 * 1024, 4 * 1024 * 1024).expect("Invalid TCP recv buffer bounds")
    }

    pub fn default_tcp_tx() -> Self {
        // 5.15.0-39-generic #42-Ubuntu SMP
        // net.ipv4.tcp_wmem = 4096	16384	4194304
        Self::new(4 * 1024, 16 * 1024, 128 * 1024).expect("Invalid TCP send buffer bounds")
    }

    pub fn default_udp_rx() -> Self {
        // 5.15.0-39-generic #42-Ubuntu SMP
        // net.ipv4.udp_mem = 763233	1017647	1526466
        // net.ipv4.udp_rmem_min = 4096
        Self::new(10 * 1024, 128 * 1024, 1490 * 1024).expect("Invalid UDP recv buffer bounds")
    }

    pub fn default_udp_tx() -> Self {
        // 5.15.0-39-generic #42-Ubuntu SMP
        // net.ipv4.udp_mem = 763233	1017647	1526466
        // net.ipv4.udp_wmem_min = 4096
        Self::new(10 * 1024, 128 * 1024, 1490 * 1024).expect("Invalid UDP send buffer bounds")
    }
}

pub fn tcp_socket<'a>(rx_mem: Memory, tx_mem: Memory) -> TcpSocket<'a> {
    let rx_buf = TcpSocketBuffer::new(vec![0; rx_mem.max]);
    let tx_buf = TcpSocketBuffer::new(vec![0; tx_mem.max]);
    let mut socket = TcpSocket::new(rx_buf, tx_buf);
    socket.set_defaults();
    socket
}

pub fn udp_socket<'a>(rx_mem: Memory, tx_mem: Memory) -> UdpSocket<'a> {
    let rx_buf = UdpSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(rx_mem.max));
    let tx_buf = UdpSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(tx_mem.max));
    UdpSocket::new(rx_buf, tx_buf)
}

pub fn icmp_socket<'a>(rx_mem: Memory, tx_mem: Memory) -> IcmpSocket<'a> {
    let rx_buf =
        IcmpSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(rx_mem.max));
    let tx_buf =
        IcmpSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(tx_mem.max));
    IcmpSocket::new(rx_buf, tx_buf)
}

pub fn raw_socket<'a>(
    ip_version: IpVersion,
    ip_protocol: IpProtocol,
    rx_mem: Memory,
    tx_mem: Memory,
) -> RawSocket<'a> {
    let rx_buf = RawSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(rx_mem.max));
    let tx_buf = RawSocketBuffer::new(meta_storage(META_STORAGE_SIZE), payload_storage(tx_mem.max));
    RawSocket::new(ip_version, ip_protocol, rx_buf, tx_buf)
}

#[inline]
fn meta_storage<H: Clone>(size: usize) -> Vec<PacketMetadata<H>> {
    vec![PacketMetadata::EMPTY; size]
}

#[inline]
fn payload_storage<T: Default + Clone>(size: usize) -> Vec<T> {
    vec![Default::default(); size]
}
