use std::cell::RefCell;
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::*;
use smoltcp::wire::IpEndpoint;

use crate::interface::CaptureInterface;
use crate::patch_smoltcp::GetSocketSafe;
use crate::socket::{SocketDesc, SocketEndpoint};
use crate::{Error, Protocol, Result};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

        let socket = match iface.get_socket_safe::<TcpSocket>(self.connection.handle) {
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

        let socket = match iface.get_socket_safe::<TcpSocket>(self.handle) {
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
    data: Vec<u8>,
    offset: usize,
    connection: Connection,
    iface: Rc<RefCell<CaptureInterface<'a>>>,
    /// Send completion callback; there may as well have been no data sent
    sent: Box<dyn Fn()>,
}

impl<'a> Send<'a> {
    pub fn new<F: Fn() + 'static>(
        data: Vec<u8>,
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
                        let socket = match iface.get_socket_safe::<TcpSocket>(conn.handle) {
                            Ok(socket) => socket,
                            Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                        };
                        socket.register_send_waker(cx.waker());
                        socket.send_slice(&self.data[self.offset..])
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
                        Err(smoltcp::Error::Exhausted) => Poll::Pending,
                        Err(err) => Poll::Ready(Err(Error::Other(err.to_string()))),
                    };
                }
                Protocol::Udp => {
                    let socket = match iface.get_socket_safe::<UdpSocket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(&self.data, conn.meta.remote)
                }
                Protocol::Icmp | Protocol::Ipv6Icmp => {
                    let socket = match iface.get_socket_safe::<IcmpSocket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(&self.data, conn.meta.remote.addr)
                }
                _ => {
                    let socket = match iface.get_socket_safe::<RawSocket>(conn.handle) {
                        Ok(socket) => socket,
                        Err(e) => return Poll::Ready(Err(Error::Other(e.to_string()))),
                    };
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(&self.data)
                }
            }
        };

        (*self.sent)();

        match result {
            Ok(_) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(Error::Other(err.to_string()))),
        }
    }
}
