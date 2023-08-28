use managed::ManagedSlice;
use smoltcp::iface::{Config, Interface, Route, SocketHandle, SocketSet};
use smoltcp::socket::AnySocket;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use std::io::Write;

use crate::device::CaptureDevice;

pub struct CaptureInterface<'a> {
    iface: Interface,
    device: CaptureDevice,
    sockets: SocketSet<'a>,
}

impl<'a> CaptureInterface<'a> {
    pub fn new(iface: Interface, device: CaptureDevice, sockets: SocketSet<'a>) -> Self {
        Self {
            iface,
            device,
            sockets,
        }
    }

    pub fn inner(&self) -> &Interface {
        &self.iface
    }

    pub fn inner_mut(&mut self) -> &mut Interface {
        &mut self.iface
    }

    pub fn device(&self) -> &CaptureDevice {
        &self.device
    }

    pub fn device_mut(&mut self) -> &mut CaptureDevice {
        &mut self.device
    }

    pub fn sockets(&self) -> impl Iterator<Item = (SocketHandle, &smoltcp::socket::Socket<'a>)> {
        self.sockets.iter()
    }

    pub fn sockets_mut(
        &mut self,
    ) -> impl Iterator<Item = (SocketHandle, &mut smoltcp::socket::Socket<'a>)> {
        self.sockets.iter_mut()
    }

    pub fn add_socket<T: AnySocket<'a>>(&mut self, socket: T) -> SocketHandle {
        self.sockets.add(socket)
    }

    pub fn get_socket_and_context<T: AnySocket<'a>>(
        &mut self,
        handle: SocketHandle,
    ) -> (&mut T, &mut smoltcp::iface::Context) {
        let socket = self.sockets.get_mut(handle);
        let ctx = self.iface.context();
        (socket, ctx)
    }

    pub fn remove_socket(&mut self, handle: SocketHandle) -> smoltcp::socket::Socket {
        self.sockets.remove(handle)
    }

    pub fn poll(&mut self, timestamp: Instant) -> bool {
        let sockets = &mut self.sockets;
        let device = &mut self.device;
        self.iface.poll(timestamp, device, sockets)
    }
}

fn iface_and_sockets<'a>(
    addr: HardwareAddress,
    device: &mut CaptureDevice,
) -> (Interface, SocketSet<'a>) {
    let config = Config::new(addr);
    let now = Instant::ZERO;
    let iface = Interface::new(config, device, now);
    let sockets = SocketSet::new(ManagedSlice::Owned(vec![]));
    (iface, sockets)
}

/// Creates a default TAP (Ethernet) network interface
pub fn tap_iface<'a>(mac: HardwareAddress, mtu: usize) -> CaptureInterface<'a> {
    let mut device = CaptureDevice::tap(mtu);
    let (iface, sockets) = iface_and_sockets(mac, &mut device);
    CaptureInterface::new(iface, device, sockets)
}

/// Creates a default TUN (IP) network interface
pub fn tun_iface<'a>(mtu: usize) -> CaptureInterface<'a> {
    let mut device = CaptureDevice::tun(mtu);
    let (iface, sockets) = iface_and_sockets(HardwareAddress::Ip, &mut device);
    CaptureInterface::new(iface, device, sockets)
}

/// Creates a pcap TAP (Ethernet) network interface
pub fn pcap_tap_iface<'a, W>(mac: HardwareAddress, mtu: usize, pcap: W) -> CaptureInterface<'a>
where
    W: Write + 'static,
{
    let mut device = CaptureDevice::pcap_tap(mtu, pcap);
    let (mut iface, sockets) = iface_and_sockets(mac, &mut device);
    iface.set_hardware_addr(mac);
    CaptureInterface::new(iface, device, sockets)
}

/// Creates a pcap TUN (IP) network interface
pub fn pcap_tun_iface<'a, W>(mtu: usize, pcap: W) -> CaptureInterface<'a>
where
    W: Write + 'static,
{
    let mut device = CaptureDevice::pcap_tun(mtu, pcap);
    let (iface, sockets) = iface_and_sockets(HardwareAddress::Ip, &mut device);
    CaptureInterface::new(iface, device, sockets)
}

/// Assigns a new interface IP address
pub fn add_iface_address(iface: &mut CaptureInterface, node_ip: IpCidr) {
    iface.inner_mut().update_ip_addrs(|addrs| {
        if !addrs.iter().any(|ip| *ip == node_ip) {
            if let Err(err) = addrs.push(node_ip) {
                log::error!("Failed to assign new interface IP address: {err}");
            }
        }
    });
}

/// Adds a new IP route
pub fn add_iface_route(iface: &mut CaptureInterface, net_ip: IpCidr, route: Route) {
    iface.inner_mut().routes_mut().update(|routes| {
        for (i, r) in routes.iter().enumerate() {
            if r.cidr == net_ip {
                if let Err(route) = routes.insert(i, route) {
                    log::error!("Failed to replace route at index {i}. Route: {route:?}");
                }
                return;
            }
        }
        if let Err(route) = routes.push(route) {
            log::error!("Failed to add new route: {route:?}");
        }
    });
}

pub fn to_mac(mac: &[u8]) -> HardwareAddress {
    let mut ethernet = if mac.len() >= 6 {
        EthernetAddress::from_bytes(&mac[..6])
    } else {
        EthernetAddress::from_bytes(&rand::random::<[u8; 6]>())
    };

    if !ethernet.is_unicast() {
        ethernet.0[0] &= !0x01;
    }

    HardwareAddress::Ethernet(ethernet)
}

pub fn ip_to_mac(ip: IpAddress) -> HardwareAddress {
    match ip {
        IpAddress::Ipv4(ip) => to_mac(ip.as_bytes()),
        IpAddress::Ipv6(ip) => to_mac(ip.as_bytes()),
    }
}
