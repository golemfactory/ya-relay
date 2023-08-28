use derive_more::Display;
use ya_smoltcp::socket::icmp::Endpoint;
use std::cell::RefCell;
use std::convert::TryFrom;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use ya_smoltcp::iface::SocketHandle;
use ya_smoltcp::socket::*;
use ya_smoltcp::wire::{IpEndpoint, Ipv4Address, IpListenEndpoint};

use crate::interface::CaptureInterface;
use crate::patch_smoltcp::GetSocketSafe;
use crate::socket::{SocketDesc, SocketEndpoint};
use crate::{CaptureDevice, Error, Protocol, Result};

use ya_relay_util::Payload;

/// Virtual connection teardown reason
#[derive(Copy, Clone, Debug)]
pub enum DisconnectReason {
    SinkClosed,
    SocketClosed,
    ConnectionFinished,
    ConnectionFailed,
    ConnectionTimeout,
}

/// Virtual connection representing 2 endpoints with an existing record
/// of exchanging packets via a known protocol; not necessarily a TCP connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Connection {
    pub handle: SocketHandle,
    pub meta: ConnectionMeta,
}

impl Connection {
    pub fn try_new<T, E>(handle: SocketHandle, t: T) -> Result<Self>
    where
        ConnectionMeta: TryFrom<T, Error = E>,
        Error: From<E>,
    {
        Ok(Self {
            handle,
            meta: ConnectionMeta::try_from(t)?,
        })
    }
}

impl From<Connection> for SocketDesc {
    fn from(c: Connection) -> Self {
        SocketDesc {
            protocol: c.meta.protocol,
            local: c.meta.local.into(),
            remote: c.meta.remote.into(),
        }
    }
}

impl From<Connection> for SocketHandle {
    fn from(c: Connection) -> Self {
        c.handle
    }
}

impl From<Connection> for ConnectionMeta {
    fn from(c: Connection) -> Self {
        c.meta
    }
}

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[display(
    fmt = "ConnectionMeta {{ protocol: {}, local: {}, remote: {} }}",
    protocol,
    local,
    remote
)]
pub struct ConnectionMeta {
    pub protocol: Protocol,
    pub local: IpEndpoint,
    pub remote: IpEndpoint,
}

impl ConnectionMeta {
    pub fn new(protocol: Protocol, local: IpEndpoint, remote: IpEndpoint) -> Self {
        Self {
            protocol,
            local,
            remote,
        }
    }

    pub fn unspecified(protocol: Protocol) -> Self {
        Self {
            protocol,
            local: IpEndpoint { addr: ya_smoltcp::wire::Ipv4Address::UNSPECIFIED.into_address(), port: 0},
            remote: IpEndpoint { addr: ya_smoltcp::wire::Ipv4Address::UNSPECIFIED.into_address(), port: 0},
        }
    }

    #[inline]
    pub fn to_socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.local.addr, self.local.port))
    }
}

impl From<ConnectionMeta> for SocketDesc {
    fn from(c: ConnectionMeta) -> Self {
        SocketDesc {
            protocol: c.protocol,
            local: c.local.into(),
            remote: c.remote.into(),
        }
    }
}

impl<'a> From<&'a ConnectionMeta> for SocketEndpoint {
    fn from(c: &'a ConnectionMeta) -> Self {
        SocketEndpoint::Ip(c.local)
    }
}

impl TryFrom<SocketDesc> for ConnectionMeta {
    type Error = Error;

    fn try_from(desc: SocketDesc) -> std::result::Result<Self, Self::Error> {
        let local = match desc.local {
            SocketEndpoint::Ip(endpoint) => endpoint,
            endpoint => return Err(Error::EndpointInvalid(endpoint)),
        };
        let remote = match desc.remote {
            SocketEndpoint::Ip(endpoint) => endpoint,
            endpoint => return Err(Error::EndpointInvalid(endpoint)),
        };
        Ok(Self {
            protocol: desc.protocol,
            local,
            remote,
        })
    }
}

/// TCP connection future
pub struct Connect<'a> {
    pub connection: Connection,
    iface: Rc<RefCell<CaptureInterface<'a>>>,
}

impl<'a> Connect<'a> {
    pub fn new(connection: Connection, iface: Rc<RefCell<CaptureInterface<'a>>>) -> Self {
        Self { connection, iface }
    }
}

impl<'a> Future for Connect<'a> {
    type Output = Result<Connection>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let iface_rfc = self.iface.clone();
        let mut iface = iface_rfc.borrow_mut();

        let socket = match iface.get_socket_safe::<tcp::Socket>(self.connection.handle) {
            Ok(s) => s,
            Err(_) => return Poll::Ready(Err(Error::SocketClosed)),
        };

        if !socket.is_open() {
            Poll::Ready(Err(Error::SocketClosed))
        } else if socket.can_send() {
            Poll::Ready(Ok(self.connection))
        } else {
            socket.register_send_waker(cx.waker());
            Poll::Pending
        }
    }
}

/// TCP disconnection future
pub struct Disconnect<'a> {
    handle: SocketHandle,
    iface: Rc<RefCell<CaptureInterface<'a>>>,
}

impl<'a> Disconnect<'a> {
    pub fn new(handle: SocketHandle, iface: Rc<RefCell<CaptureInterface<'a>>>) -> Self {
        Self { handle, iface }
    }
}

impl<'a> Future for Disconnect<'a> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let iface_rfc = self.iface.clone();
        let mut iface = iface_rfc.borrow_mut();

        let socket = match iface.get_socket_safe::<tcp::Socket>(self.handle) {
            Ok(s) => s,
            Err(_) => return Poll::Ready(Ok(())),
        };

        if !socket.is_open() {
            Poll::Ready(Ok(()))
        } else {
            socket.register_recv_waker(cx.waker());
            Poll::Pending
        }
    }
}

/// Packet send future
pub struct Send<'a> {
    data: Payload,
    offset: usize,
    connection: Connection,
    iface: Rc<RefCell<CaptureInterface<'a>>>,
    /// Send completion callback; there may as well have been no data sent
    sent: Box<dyn Fn()>,
}

impl<'a> Send<'a> {
    pub fn new<F: Fn() + 'static>(
        data: Payload,
        connection: Connection,
        iface: Rc<RefCell<CaptureInterface<'a>>>,
        sent: F,
    ) -> Self {
        Self {
            data,
            offset: 0,
            connection,
            iface,
            sent: Box::new(sent),
        }
    }
}

impl<'a> Future for Send<'a> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = {
            let mut iface = self.iface.borrow_mut();
            let conn = &self.connection;

            match conn.meta.protocol {
                Protocol::Tcp => {
                    let result = {
                        let socket = match iface.get_socket_safe::<tcp::Socket>(conn.handle) {
                            Ok(socket) => socket,
                            Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                        };
                        socket.register_send_waker(cx.waker());
                        socket.send_slice(&self.data.as_ref()[self.offset..])
                    };

                    drop(iface);
                    (*self.sent)();

                    return match result {
                        Ok(count) => {
                            self.offset += count;
                            if self.offset >= self.data.len() {
                                Poll::Ready(Ok(()))
                            } else {
                                Poll::Pending
                            }
                        }
                        Err(ya_smoltcp::socket::tcp::SendError::InvalidState) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(Error::Other(err.to_string()))),
                    };
                }
                Protocol::Udp => {
                    let socket = match iface.get_socket_safe::<udp::Socket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(self.data.as_ref(), conn.meta.remote)
                        .map_err(|err| err.to_string())
                }
                Protocol::Icmp | Protocol::Ipv6Icmp => {
                    let socket = match iface.get_socket_safe::<icmp::Socket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(self.data.as_ref(), conn.meta.remote.addr)
                        .map_err(|err| err.to_string())
                }
                _ => {
                    let socket = match iface.get_socket_safe::<raw::Socket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(self.data.as_ref())
                    .map_err(|err| err.to_string())
                }
            }
        };

        (*self.sent)();

        match result {
            Ok(_) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(Error::Other(err))),
        }
    }
}
