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
use crate::socket::{SocketDesc, SocketEndpoint, TcpSocketExt};
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
    type Error = ();

    fn try_from(desc: SocketDesc) -> std::result::Result<Self, Self::Error> {
        let local = match desc.local {
            SocketEndpoint::Ip(endpoint) => endpoint,
            _ => return Err(()),
        };
        let remote = match desc.remote {
            SocketEndpoint::Ip(endpoint) => endpoint,
            _ => return Err(()),
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
        // TODO: we need to loop because `.get_socket()` panics when connection was dropped
        //let mut socket = interface.get_socket(self.connection.handle);
        let socket = match iface
            .sockets_mut()
            .find(|(handle, _socket)| handle == &self.connection.handle)
            .map(|(_, socket)| TcpSocket::downcast(socket))
            .flatten()
        {
            Some(s) => s,
            None => return Poll::Ready(Err(Error::SocketClosed)),
        };

        if !socket.is_open() {
            Poll::Ready(Err(Error::SocketClosed))
        } else if socket.can_send() {
            socket.set_defaults();
            Poll::Ready(Ok(self.connection))
        } else {
            socket.register_send_waker(cx.waker());
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
                        let socket = iface.get_socket::<TcpSocket>(conn.handle);
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
                    let socket = iface.get_socket::<UdpSocket>(conn.handle);
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(&self.data, conn.meta.remote)
                }
                Protocol::Icmp => {
                    let socket = iface.get_socket::<IcmpSocket>(conn.handle);
                    socket.register_send_waker(cx.waker());
                    socket.send_slice(&self.data, conn.meta.remote.addr)
                }
                _ => {
                    let socket = iface.get_socket::<RawSocket>(conn.handle);
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
