use std::cell::RefCell;
use std::rc::Rc;

use smoltcp::iface::Route;
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
    sockets: Rc<RefCell<SocketSet<'a>>>,
    ports: Rc<RefCell<port::Allocator>>,
}

impl<'a> Stack<'a> {
    pub fn new(net_ip: IpCidr, net_route: Route) -> Self {
        let sockets = SocketSet::new(Vec::with_capacity(8));
        let mut iface = default_iface();
        add_iface_route(&mut iface, net_ip, net_route);

        Self {
            iface: Rc::new(RefCell::new(iface)),
            sockets: Rc::new(RefCell::new(sockets)),
            ports: Default::default(),
        }
    }

    pub fn with(iface: CaptureInterface<'a>) -> Self {
        let sockets = SocketSet::new(Vec::with_capacity(8));
        Self {
            iface: Rc::new(RefCell::new(iface)),
            sockets: Rc::new(RefCell::new(sockets)),
            ports: Default::default(),
        }
    }

    pub fn addresses(&self) -> Vec<IpCidr> {
        self.iface.borrow().ip_addrs().to_vec()
    }

    pub fn address(&self) -> Result<IpCidr> {
        {
            let iface = self.iface.borrow();
            iface.ip_addrs().iter().next().cloned()
        }
        .ok_or_else(|| Error::NetEmpty)
    }

    pub fn add_address(&self, address: IpCidr) {
        let mut iface = self.iface.borrow_mut();
        add_iface_address(&mut (*iface), address);
    }

    pub(crate) fn iface(&self) -> Rc<RefCell<CaptureInterface<'a>>> {
        self.iface.clone()
    }

    pub(crate) fn sockets(&self) -> Rc<RefCell<SocketSet<'a>>> {
        self.sockets.clone()
    }
}

impl<'a> Stack<'a> {
    pub fn bind(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Result<SocketHandle> {
        let endpoint = endpoint.into();
        let mut sockets = self.sockets.borrow_mut();
        let handle = match protocol {
            Protocol::Tcp => {
                if let SocketEndpoint::Ip(ep) = endpoint {
                    let mut socket = tcp_socket();
                    socket.listen(ep).map_err(|e| Error::Other(e.to_string()))?;
                    sockets.add(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Udp => {
                if let SocketEndpoint::Ip(ep) = endpoint {
                    let mut socket = udp_socket();
                    socket.bind(ep).map_err(|e| Error::Other(e.to_string()))?;
                    sockets.add(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Icmp => {
                if let SocketEndpoint::Icmp(e) = endpoint {
                    let mut socket = icmp_socket();
                    socket.bind(e).map_err(|e| Error::Other(e.to_string()))?;
                    sockets.add(socket)
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

                let socket = raw_socket(ip_version, map_protocol(protocol)?);
                sockets.add(socket)
            }
        };
        Ok(handle)
    }

    pub fn connect(&self, remote: IpEndpoint) -> Result<Connect<'a>> {
        let ip = self.address()?.address();

        let mut sockets = self.sockets.borrow_mut();
        let mut ports = self.ports.borrow_mut();

        let handle = sockets.add(tcp_socket());
        let port = ports.next(Protocol::Tcp)?;
        let local: IpEndpoint = (ip, port).into();

        if let Err(e) = {
            let mut socket = sockets.get::<TcpSocket>(handle);
            socket.connect(remote, local)
        } {
            sockets.remove(handle);
            ports.free(Protocol::Tcp, port);
            return Err(Error::ConnectionError(e.to_string()));
        }

        let meta = ConnectionMeta {
            protocol: Protocol::Tcp,
            local,
            remote,
        };

        Ok(Connect::new(
            Connection { handle, meta },
            self.sockets.clone(),
        ))
    }

    pub fn close(&self, protocol: Protocol, handle: SocketHandle) {
        let mut sockets = self.sockets.borrow_mut();
        let mut ports = self.ports.borrow_mut();

        let port = match protocol {
            Protocol::Tcp => sockets.get::<TcpSocket>(handle).local_endpoint().port,
            Protocol::Udp => sockets.get::<UdpSocket>(handle).endpoint().port,
            _ => return,
        };

        ports.free(protocol, port);
        sockets.release(handle);
    }

    pub fn send<B: Into<Vec<u8>>, F: Fn() + 'static>(
        &self,
        data: B,
        meta: Connection,
        f: F,
    ) -> Send<'a> {
        Send::new(data.into(), meta, self.sockets.clone(), f)
    }

    pub fn receive<B: Into<Vec<u8>>>(&self, data: B) {
        let mut iface = self.iface.borrow_mut();
        iface.device_mut().phy_rx(data.into());
    }

    pub fn poll(&self) -> Result<()> {
        let mut iface = self.iface.borrow_mut();
        let mut sockets = self.sockets.borrow_mut();
        iface
            .poll(&mut (*sockets), Instant::now())
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
