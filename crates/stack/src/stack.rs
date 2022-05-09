use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use smoltcp::iface::{Route, SocketHandle};
use smoltcp::phy::Device;
use smoltcp::socket::*;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion};

use crate::connection::{Connect, Connection, ConnectionMeta, Disconnect, Send};
use crate::interface::*;
use crate::metrics::ChannelMetrics;
use crate::patch_smoltcp::GetSocketSafe;
use crate::protocol::Protocol;
use crate::socket::*;
use crate::{port, NetworkConfig};
use crate::{Error, Result};

#[derive(Clone)]
pub struct Stack<'a> {
    iface: Rc<RefCell<CaptureInterface<'a>>>,
    metrics: Rc<RefCell<HashMap<SocketDesc, ChannelMetrics>>>,
    ports: Rc<RefCell<port::Allocator>>,
    config: Rc<NetworkConfig>,
}

impl<'a> Stack<'a> {
    pub fn new(iface: CaptureInterface<'a>, config: Rc<NetworkConfig>) -> Self {
        Self {
            iface: Rc::new(RefCell::new(iface)),
            metrics: Default::default(),
            ports: Default::default(),
            config,
        }
    }

    pub fn address(&self) -> Result<IpCidr> {
        {
            let iface = self.iface.borrow();
            iface.ip_addrs().iter().next().cloned()
        }
        .ok_or(Error::NetEmpty)
    }

    pub fn addresses(&self) -> Vec<IpCidr> {
        self.iface.borrow().ip_addrs().to_vec()
    }

    pub fn add_address(&self, address: IpCidr) {
        let mut iface = self.iface.borrow_mut();
        add_iface_address(&mut (*iface), address);
    }

    pub fn add_route(&self, net_ip: IpCidr, route: Route) {
        let mut iface = self.iface.borrow_mut();
        add_iface_route(&mut (*iface), net_ip, route);
    }

    #[inline]
    pub(crate) fn iface(&self) -> Rc<RefCell<CaptureInterface<'a>>> {
        self.iface.clone()
    }

    #[inline]
    pub(crate) fn metrics(&self) -> Rc<RefCell<HashMap<SocketDesc, ChannelMetrics>>> {
        self.metrics.clone()
    }

    pub(crate) fn on_sent(&self, desc: &SocketDesc, size: usize) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.entry(*desc).or_default().tx.push(size as f32);
    }

    pub(crate) fn on_received(&self, desc: &SocketDesc, size: usize) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.entry(*desc).or_default().rx.push(size as f32);
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
                    let mut socket = tcp_socket(mtu, self.config.buffer_size_multiplier);
                    socket.listen(ep).map_err(|e| Error::Other(e.to_string()))?;
                    socket.set_defaults();
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Udp => {
                if let SocketEndpoint::Ip(ep) = endpoint {
                    let mut socket = udp_socket(mtu, self.config.buffer_size_multiplier);
                    socket.bind(ep).map_err(|e| Error::Other(e.to_string()))?;
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an IP endpoint".to_string()));
                }
            }
            Protocol::Icmp | Protocol::Ipv6Icmp => {
                if let SocketEndpoint::Icmp(e) = endpoint {
                    let mut socket = icmp_socket(mtu, self.config.buffer_size_multiplier);
                    socket.bind(e).map_err(|e| Error::Other(e.to_string()))?;
                    iface.add_socket(socket)
                } else {
                    return Err(Error::Other("Expected an ICMP endpoint".to_string()));
                }
            }
            _ => {
                let ip_version = {
                    match endpoint {
                        SocketEndpoint::Ip(ep) => match ep.addr {
                            IpAddress::Ipv4(_) => IpVersion::Ipv4,
                            IpAddress::Ipv6(_) => IpVersion::Ipv6,
                            _ => return Err(Error::Other(format!("Invalid address: {}", ep.addr))),
                        },
                        _ => return Err(Error::Other("Expected an IP endpoint".to_string())),
                    }
                };

                let socket = raw_socket(
                    ip_version,
                    map_protocol(protocol)?,
                    mtu,
                    self.config.buffer_size_multiplier,
                );
                iface.add_socket(socket)
            }
        };

        Ok(handle)
    }

    pub fn unbind(
        &self,
        protocol: Protocol,
        endpoint: impl Into<SocketEndpoint>,
    ) -> Result<SocketHandle> {
        let endpoint = endpoint.into();
        let mut iface = self.iface.borrow_mut();
        let mut sockets = iface.sockets_mut();

        let handle = sockets
            .find(|(_, s)| s.local_endpoint() == endpoint)
            .and_then(|(h, _)| match protocol {
                Protocol::Tcp | Protocol::Udp | Protocol::Icmp | Protocol::Ipv6Icmp => Some(h),
                _ => None,
            })
            .ok_or(Error::SocketClosed)?;

        let _ = endpoint.ip_endpoint().map(|e| {
            log::trace!("unbinding {} ({})", e, protocol);
            let mut ports = self.ports.borrow_mut();
            ports.free(protocol, e.port);
        });

        drop(sockets);
        iface.remove_socket(handle);
        Ok(handle)
    }

    pub fn connect(&self, remote: IpEndpoint) -> Result<Connect<'a>> {
        let ip = self.address()?.address();

        let mut iface = self.iface.borrow_mut();
        let mut ports = self.ports.borrow_mut();
        let mtu = iface.device().capabilities().max_transmission_unit;

        let protocol = Protocol::Tcp;
        let handle = iface.add_socket(tcp_socket(mtu, self.config.buffer_size_multiplier));
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

    pub fn disconnect(&self, handle: SocketHandle) -> Disconnect<'a> {
        let mut iface = self.iface.borrow_mut();
        if let Ok(sock) = iface.get_socket_safe::<TcpSocket>(handle) {
            sock.close();
        }
        Disconnect::new(handle, self.iface.clone())
    }

    pub(crate) fn abort(&self, handle: SocketHandle) {
        let mut iface = self.iface.borrow_mut();
        if let Ok(sock) = iface.get_socket_safe::<TcpSocket>(handle) {
            sock.abort();
        }
    }

    pub(crate) fn remove(&self, meta: &ConnectionMeta, handle: SocketHandle) {
        let mut iface = self.iface.borrow_mut();
        let mut metrics = self.metrics.borrow_mut();
        let mut ports = self.ports.borrow_mut();

        if let Some((handle, socket)) = {
            let mut sockets = iface.sockets();
            sockets.find(|(h, _)| h == &handle)
        } {
            log::trace!(
                "removing connection: {}:{}:{}",
                meta.protocol,
                meta.local,
                meta.remote,
            );

            metrics.remove(&socket.desc());
            iface.remove_socket(handle);
            ports.free(meta.protocol, meta.local.port);
        }
    }

    #[inline]
    pub fn send<B: Into<Vec<u8>>, F: Fn() + 'static>(
        &self,
        data: B,
        conn: Connection,
        f: F,
    ) -> Send<'a> {
        Send::new(data.into(), conn, self.iface.clone(), f)
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
