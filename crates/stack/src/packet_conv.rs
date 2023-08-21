use crate::packet::{EtherField, IpPacket, PeekPacket};
use crate::Error;

pub fn packet_ip_wrap_to_ether(
    frame: &[u8],
    src_mac: Option<&[u8; 6]>,
    dst_mac: Option<&[u8; 6]>,
) -> Result<Vec<u8>, Error> {
    if frame.is_empty() {
        return Err(Error::Other(
            "Error when wrapping IP packet: Empty packet".into(),
        ));
    }
    if let Err(err) = IpPacket::peek(frame) {
        return Err(Error::PacketMalformed(format!(
            "Error when wrapping IP packet {err}"
        )));
    }

    let mut eth_packet = vec![0u8; frame.len() + 14];
    if let Some(dst_mac) = dst_mac {
        eth_packet[EtherField::DST_MAC].copy_from_slice(dst_mac);
    } else {
        const DEFAULT_DST_MAC: &[u8; 6] = &[0x32, 0x32, 0x32, 0x32, 0x32, 0x32];
        eth_packet[EtherField::DST_MAC].copy_from_slice(DEFAULT_DST_MAC);
    }
    if let Some(src_mac) = src_mac {
        eth_packet[EtherField::SRC_MAC].copy_from_slice(src_mac);
    } else {
        const DEFAULT_SRC_MAC: &[u8; 6] = &[0x22, 0x22, 0x22, 0x22, 0x22, 0x22];
        eth_packet[EtherField::SRC_MAC].copy_from_slice(DEFAULT_SRC_MAC);
    }
    match IpPacket::packet(frame) {
        IpPacket::V4(_pkt) => {
            const ETHER_TYPE_IPV4: &[u8; 2] = &[0x08, 0x00];
            eth_packet[EtherField::ETHER_TYPE].copy_from_slice(ETHER_TYPE_IPV4);
        }
        IpPacket::V6(_pkt) => {
            const ETHER_TYPE_IPV6: &[u8; 2] = &[0x86, 0xdd];
            eth_packet[EtherField::ETHER_TYPE].copy_from_slice(ETHER_TYPE_IPV6);
        }
    };
    eth_packet[EtherField::PAYLOAD].copy_from_slice(&frame[0..]);
    Ok(eth_packet)
}

pub fn packet_ether_to_ip_slice(frame: &[u8]) -> Result<&[u8], Error> {
    const MIN_IP_HEADER_LENGTH: usize = 20;
    if frame.len() <= 14 + MIN_IP_HEADER_LENGTH {
        return Err(Error::Other(format!(
            "Error when creating IP packet from ether packet: Packet too short. Packet length {}",
            frame.len()
        )));
    }
    let ip_frame = &frame[EtherField::PAYLOAD];
    if let Err(err) = IpPacket::peek(ip_frame) {
        return Err(Error::PacketMalformed(format!(
            "Error when creating IP packet from ether packet {err}"
        )));
    }
    Ok(ip_frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_ether_to_ip() {
        let valid_ether_packet = hex::decode("51bd2c1e5c202423d4418ef108004500002800010000401175ba0d0f1112717375765b941a850014476f48656c6c6f205061636b6574").unwrap();
        let valid_ip_packet = hex::decode(
            "4500002800010000401175ba0d0f1112717375765b941a850014476f48656c6c6f205061636b6574",
        )
        .unwrap();

        assert_eq!(
            hex::encode(valid_ip_packet),
            hex::encode(packet_ether_to_ip_slice(&valid_ether_packet).unwrap())
        );
    }

    #[test]
    fn test_packet_ip_to_ether() {
        let valid_ip_packet = hex::decode(
            "4500002800010000401175ba0d0f1112717375765b941a850014476f48656c6c6f205061636b6574",
        )
        .unwrap();
        let valid_ether_packet = hex::decode("32323232323222222222222208004500002800010000401175ba0d0f1112717375765b941a850014476f48656c6c6f205061636b6574").unwrap();

        assert_eq!(
            hex::encode(&valid_ether_packet),
            hex::encode(packet_ip_wrap_to_ether(&valid_ip_packet, None, None).unwrap())
        );

        let valid_ether_packet2 = hex::decode("51bd2c1e5c202423d4418ef108004500002800010000401175ba0d0f1112717375765b941a850014476f48656c6c6f205061636b6574").unwrap();

        const SRC_MAC: &[u8; 6] = &[0x24, 0x23, 0xd4, 0x41, 0x8e, 0xf1];
        const DST_MAC: &[u8; 6] = &[0x51, 0xbd, 0x2c, 0x1e, 0x5c, 0x20];
        assert_eq!(
            hex::encode(&valid_ether_packet2),
            hex::encode(
                packet_ip_wrap_to_ether(&valid_ip_packet, Some(SRC_MAC), Some(DST_MAC)).unwrap()
            )
        );
    }
}
