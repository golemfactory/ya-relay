use std::collections::BTreeMap;
use std::io::Write;

use managed::{ManagedMap, ManagedSlice};
use smoltcp::iface::{Interface, InterfaceBuilder, NeighborCache, Route, Routes};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use crate::device::CaptureDevice;

pub type CaptureInterface<'a> = Interface<'a, CaptureDevice>;

/// Network interface builder
pub fn iface_builder<'a>(device: CaptureDevice) -> InterfaceBuilder<'a, CaptureDevice> {
    let sockets = Vec::new();
    let routes = Routes::new(BTreeMap::new());
    let addrs = Vec::new();

    InterfaceBuilder::new(device, sockets)
        .ip_addrs(addrs)
        .routes(routes)
}

/// Creates a default TAP (Ethernet) network interface
pub fn tap_iface<'a>(mac: HardwareAddress, mtu: usize) -> CaptureInterface<'a> {
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    iface_builder(CaptureDevice::tap(mtu))
        .neighbor_cache(neighbor_cache)
        .hardware_addr(mac)
        .finalize()
}

/// Creates a default TUN (IP) network interface
pub fn tun_iface<'a>(mtu: usize) -> CaptureInterface<'a> {
    iface_builder(CaptureDevice::tun(mtu)).finalize()
}

/// Creates a pcap TAP (Ethernet) network interface
pub fn pcap_tap_iface<'a, W>(mac: HardwareAddress, mtu: usize, pcap: W) -> CaptureInterface<'a>
where
    W: Write + 'static,
{
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    iface_builder(CaptureDevice::pcap_tap(mtu, pcap))
        .neighbor_cache(neighbor_cache)
        .hardware_addr(mac)
        .finalize()
}

/// Creates a pcap TUN (IP) network interface
pub fn pcap_tun_iface<'a, W>(mtu: usize, pcap: W) -> CaptureInterface<'a>
where
    W: Write + 'static,
{
    iface_builder(CaptureDevice::pcap_tun(mtu, pcap)).finalize()
}

/// Assigns a new interface IP address
pub fn add_iface_address(iface: &mut CaptureInterface, node_ip: IpCidr) {
    iface.update_ip_addrs(|addrs| match addrs {
        ManagedSlice::Owned(ref mut vec) => vec.push(node_ip),
        ManagedSlice::Borrowed(ref slice) => {
            let mut vec = slice.to_vec();
            vec.push(node_ip);
            *addrs = vec.into();
        }
    });
}

/// Adds a new IP route
pub fn add_iface_route(iface: &mut CaptureInterface, net_ip: IpCidr, route: Route) {
    iface.routes_mut().update(|routes| match routes {
        ManagedMap::Owned(ref mut map) => {
            map.insert(net_ip, route);
        }
        ManagedMap::Borrowed(ref map) => {
            let mut map: BTreeMap<IpCidr, Route> = map.iter().filter_map(|e| *e).collect();
            map.insert(net_ip, route);
            *routes = map.into();
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
        _ => to_mac(&rand::random::<[u8; 6]>()),
    }
}
