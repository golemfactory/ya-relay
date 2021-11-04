use std::collections::BTreeMap;

use managed::{ManagedMap, ManagedSlice};
use smoltcp::iface::{EthernetInterface, EthernetInterfaceBuilder, NeighborCache, Route, Routes};
use smoltcp::wire::{EthernetAddress, IpCidr};

use crate::device::CaptureDevice;

pub type CaptureInterface<'a> = EthernetInterface<'a, CaptureDevice>;

/// Creates a default network interface
pub fn default_iface<'a>() -> CaptureInterface<'a> {
    default_iface_builder().finalize()
}

/// Creates a default network interface builder
pub fn default_iface_builder<'a>() -> EthernetInterfaceBuilder<'a, CaptureDevice> {
    let neighbor_cache = NeighborCache::new(BTreeMap::new());
    let routes = Routes::new(BTreeMap::new());
    let addrs = Vec::new();

    let ethernet_addr = loop {
        let addr = EthernetAddress(rand::random());
        if addr.is_unicast() {
            break addr;
        }
    };

    EthernetInterfaceBuilder::new(CaptureDevice::default())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(addrs)
        .routes(routes)
}

/// Assigns a new interface IP address
pub fn add_iface_address(iface: &mut CaptureInterface, node_ip: IpCidr) {
    iface.update_ip_addrs(|addrs| match addrs {
        ManagedSlice::Owned(ref mut vec) => {
            if !vec.iter().any(|ip| *ip == node_ip) {
                vec.push(node_ip);
            }
        }
        ManagedSlice::Borrowed(ref slice) => {
            let mut vec = slice.to_vec();
            if !vec.iter().any(|ip| *ip == node_ip) {
                vec.push(node_ip);
            }
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
