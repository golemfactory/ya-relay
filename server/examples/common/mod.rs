use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let ip = u32::from(ip);

    ![
        (0x0000_0000, 8),  // current network
        (0x0a00_0000, 8),  // private
        (0x6440_0000, 10), // shared address space
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local
        (0xac10_0000, 12), // private
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // documentation
        (0xc0a8_0000, 16), // private
        (0xc612_0000, 15), // benchmarking
        (0xc633_6400, 24), // documentation
        (0xcb00_7100, 24), // documentation
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved and limited broadcast
    ]
    .into_iter()
    .any(|(network, prefix)| in_ipv4_network(ip, network, prefix))
}

fn in_ipv4_network(ip: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX << (32 - prefix);
    ip & mask == network
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();

    // IPv4-mapped IPv6 addresses inherit the IPv4 classification.
    if segments[..6] == [0, 0, 0, 0, 0, 0xffff] {
        let ipv4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return is_public_ipv4(ipv4);
    }

    // Public node addresses are expected to be in the global unicast 2000::/3
    // range. Exclude documentation, benchmarking, and ORCHIDv2 prefixes.
    segments[0] & 0xe000 == 0x2000
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0)
        && !(segments[0] == 0x2001 && segments[1] & 0xfff0 == 0x0020)
}

#[cfg(test)]
mod tests {
    use super::is_public_ip;

    #[test]
    fn recognizes_public_ipv4_addresses() {
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip("192.0.2.1".parse().unwrap()));
        assert!(!is_public_ip("224.0.0.1".parse().unwrap()));
    }

    #[test]
    fn recognizes_public_ipv6_addresses() {
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fd00::1".parse().unwrap()));
        assert!(!is_public_ip("2001:db8::1".parse().unwrap()));
        assert!(is_public_ip("::ffff:8.8.8.8".parse().unwrap()));
        assert!(!is_public_ip("::ffff:192.168.1.1".parse().unwrap()));
    }
}
