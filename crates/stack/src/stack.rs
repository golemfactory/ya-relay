use std::cell::RefCell;
use std::rc::Rc;

use smoltcp::iface::SocketHandle;
use smoltcp::phy::Device;
use smoltcp::socket::*;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion};

use crate::connection::{Connect, Connection, ConnectionMeta, Send};
use crate::interface::*;
use crate::port;
use crate::protocol::Protocol;
use crate::socket::*;
use crate::{Error, Result};

#[derive(Clone)]
pub struct Stack<'a> {
    iface: Rc<RefCell<CaptureInterface<'a>>>,
    ports: Rc<RefCell<port::Allocator>>,
}

impl<'a> Stack<'a> {
    pub fn new(iface: CaptureInterface<'a>) -> Self {
        Self {
            iface: Rc::new(RefCell::new(iface)),
            ports: Default::default(),
        }
    }

    #[inline]
    pub fn addresses(&self) -> Vec<IpCidr> {
        self.iface.borrow().ip_addrs().to_vec()
    }

    pub fn address(&self) -> Result<IpCidr> {
        {
            let iface = self.iface.borrow();
            iface.ip_addrs().iter().next().cloned()
        }
        .ok_or(Error::NetEmpty)
    }

    pub fn add_address(&self, address: IpCidr) {
        let mut iface = self.iface.borrow_mut();
        add_iface_address(&mut (*iface), address);
    }

    #[inline]
    pub(crate) fn iface(&self) -> Rc<RefCell<CaptureInterface<'a>>> {
        self.iface.clone()
    }
}

impl<'a> Stack<'a> {
    pub fn bind(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Result<SocketHandle> {
        let endpoint = endpoint.into();
        let mut iface = self.iface.borrow_mut();
        let mtu = iface.device().capabilities().max_transmission_unit;

        let handle = match protocol {
            Protocol::Tcp => {
                if let SocketEndpoint::Ip(ep) = endpoint {
                    let mut socket = tcp_socket(mtu);
                    socket.listen(ep).map_err(|e| Error::Other(e.to_string()))?;
                    socket.set_defaults();
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Udp => {
                if let SocketEndpoint::Ip(ep) = endpoint {
                    let mut socket = udp_socket(mtu);
                    socket.bind(ep).map_err(|e| Error::Other(e.to_string()))?;
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Icmp => {
                if let SocketEndpoint::Icmp(e) = endpoint {
                    let mut socket = icmp_socket(mtu);
                    socket.bind(e).map_err(|e| Error::Other(e.to_string()))?;
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an ICMP endpoint".to_string()));
                }
            }
            _ => {
                let ip_version = {
                    match endpoint {
                        SocketEndpoint::Ip(e) => match e.addr {
                            IpAddress::Ipv4(_) => IpVersion::Ipv4,
                            IpAddress::Ipv6(_) => IpVersion::Ipv6,
                            _ => return Err(Error::Other(format!("Invalid address: {}", e.addr))),
                        },
                        _ => return Err(Error::Other("Expected an IP endpoint".to_string())),
                    }
                };

                let socket = raw_socket(ip_version, map_protocol(protocol)?, mtu);
                iface.add_socket(socket)
            }
        };
        Ok(handle)
    }

    pub fn connect(&self, remote: IpEndpoint) -> Result<Connect<'a>> {
        let ip = self.address()?.address();

        let mut iface = self.iface.borrow_mut();
        let mut ports = self.ports.borrow_mut();
        let mtu = iface.device().capabilities().max_transmission_unit;

        let protocol = Protocol::Tcp;
        let handle = iface.add_socket(tcp_socket(mtu));
        let port = ports.next(protocol)?;
        let local: IpEndpoint = (ip, port).into();

        log::trace!("connecting to {} ({})", remote, protocol);

        match {
            let (socket, ctx) = iface.get_socket_and_context::<TcpSocket>(handle);
            socket.connect(ctx, remote, local).map(|_| socket)
        } {
            Ok(socket) => socket.set_defaults(),
            Err(e) => {
                iface.remove_socket(handle);
                ports.free(Protocol::Tcp, port);
                return Err(Error::ConnectionError(e.to_string()));
            }
        }

        let meta = ConnectionMeta {
            protocol,
            local,
            remote,
        };
        Ok(Connect::new(
            Connection { handle, meta },
            self.iface.clone(),
        ))
    }

    pub fn close(&self, meta: &ConnectionMeta, handle: SocketHandle) {
        let mut iface = self.iface.borrow_mut();
        let mut sockets = iface.sockets_mut();

        if let Some((_, socket)) = sockets.find(|(h, _)| h == &handle) {
            log::trace!(
                "closing connection: {}:{}:{}",
                meta.protocol,
                meta.local,
                meta.remote,
            );
            socket.close();
        }
    }

    pub fn remove(&self, meta: &ConnectionMeta, handle: SocketHandle) {
        let mut iface = self.iface.borrow_mut();
        let mut ports = self.ports.borrow_mut();

        if let Some(handle) = {
            let mut sockets = iface.sockets();
            sockets.find(|(h, _)| h == &handle).map(|(h, _)| h)
        } {
            log::trace!(
                "removing connection: {}:{}:{}",
                meta.protocol,
                meta.local,
                meta.remote,
            );

            iface.remove_socket(handle);
            ports.free(meta.protocol, meta.local.port);
        }
    }

    pub fn send<B: Into<Vec<u8>>, F: Fn() + 'static>(
        &self,
        data: B,
        meta: Connection,
        f: F,
    ) -> Send<'a> {
        Send::new(data.into(), meta, self.iface.clone(), f)
    }

    pub fn receive<B: Into<Vec<u8>>>(&self, data: B) {
        let mut iface = self.iface.borrow_mut();
        iface.device_mut().phy_rx(data.into());
    }

    pub fn poll(&self) -> Result<()> {
        let mut iface = self.iface.borrow_mut();
        iface
            .poll(Instant::now())
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
    }
}

fn map_protocol(protocol: Protocol) -> Result<IpProtocol> {
    match protocol {
        Protocol::HopByHop => Ok(IpProtocol::HopByHop),
        Protocol::Icmp => Ok(IpProtocol::Icmp),
        Protocol::Igmp => Ok(IpProtocol::Igmp),
        Protocol::Tcp => Ok(IpProtocol::Tcp),
        Protocol::Udp => Ok(IpProtocol::Udp),
        Protocol::Ipv6Route => Ok(IpProtocol::Ipv6Route),
        Protocol::Ipv6Frag => Ok(IpProtocol::Ipv6Frag),
        Protocol::Ipv6Icmp => Ok(IpProtocol::Icmpv6),
        Protocol::Ipv6NoNxt => Ok(IpProtocol::Ipv6NoNxt),
        Protocol::Ipv6Opts => Ok(IpProtocol::Ipv6Opts),
        _ => Err(Error::ProtocolNotSupported(protocol.to_string())),
    }
}
