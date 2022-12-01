#![allow(unused)]

use self::field::*;
use ya_smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};
use std::convert::TryFrom;
use std::ops::Deref;

use crate::{Error, Protocol};

pub const ETHERNET_HDR_SIZE: usize = 14;
pub const IP4_HDR_SIZE: usize = 20;
pub const IP6_HDR_SIZE: usize = 40;
pub const TCP_HDR_SIZE: usize = 20;
pub const UDP_HDR_SIZE: usize = 20;

mod field {
    /// Field slice range within packet bytes
    pub type Field = std::ops::Range<usize>;
    /// Field bit range within a packet byte
    pub type BitField = (usize, std::ops::Range<usize>);
    /// Unhandled packet data range
    pub type Rest = std::ops::RangeFrom<usize>;
}

pub struct EtherField;
impl EtherField {
    pub const DST_MAC: Field = 0..6;
    pub const SRC_MAC: Field = 6..12;
    pub const ETHER_TYPE: Field = 12..14;
    pub const PAYLOAD: Rest = 14..;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum EtherType {
    Ip,
    Arp,
}

#[non_exhaustive]
pub enum EtherFrame {
    /// EtherType IP
    Ip(Box<[u8]>),
    /// EtherType ARP
    Arp(Box<[u8]>),
}

impl EtherFrame {
    pub fn peek_type(data: &[u8]) -> Result<EtherType, Error> {
        if data.len() < ETHERNET_HDR_SIZE {
            return Err(Error::PacketMalformed("Ethernet: frame too short".into()));
        }

        let proto = &data[EtherField::ETHER_TYPE];
        match proto {
            &[0x08, 0x00] | &[0x86, 0xdd] => {
                IpPacket::peek(&data[ETHERNET_HDR_SIZE..])?;
                Ok(EtherType::Ip)
            }
            &[0x08, 0x06] => {
                ArpPacket::peek(&data[ETHERNET_HDR_SIZE..])?;
                Ok(EtherType::Arp)
            }
            _ => Err(Error::ProtocolNotSupported(format!("0x{:02x?}", proto))),
        }
    }

    pub fn peek_payload(data: &[u8]) -> Result<&[u8], Error> {
        if data.len() < ETHERNET_HDR_SIZE {
            return Err(Error::PacketMalformed("Ethernet: frame too short".into()));
        }
        Ok(&data[EtherField::PAYLOAD])
    }

    pub const fn payload_off() -> usize {
        ETHERNET_HDR_SIZE
    }

    pub fn payload(&self) -> &[u8] {
        &self.as_ref()[EtherField::PAYLOAD]
    }
}

impl TryFrom<Box<[u8]>> for EtherFrame {
    type Error = Error;

    fn try_from(data: Box<[u8]>) -> Result<Self, Self::Error> {
        match Self::peek_type(&data)? {
            EtherType::Ip => Ok(EtherFrame::Ip(data)),
            EtherType::Arp => Ok(EtherFrame::Arp(data)),
        }
    }
}

impl TryFrom<Vec<u8>> for EtherFrame {
    type Error = Error;

    #[inline]
    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Self::try_from(value.into_boxed_slice())
    }
}

impl From<EtherFrame> for Box<[u8]> {
    fn from(frame: EtherFrame) -> Self {
        match frame {
            EtherFrame::Ip(b) | EtherFrame::Arp(b) => b,
        }
    }
}

impl From<EtherFrame> for Vec<u8> {
    fn from(frame: EtherFrame) -> Self {
        Into::<Box<[u8]>>::into(frame).into()
    }
}

impl AsRef<Box<[u8]>> for EtherFrame {
    fn as_ref(&self) -> &Box<[u8]> {
        match self {
            Self::Ip(b) | Self::Arp(b) => b,
        }
    }
}

impl std::fmt::Display for EtherFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EtherFrame::Ip(_) => write!(f, "IP"),
            EtherFrame::Arp(_) => write!(f, "ARP"),
        }
    }
}

pub trait PeekPacket<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error>;
    fn packet(data: &'a [u8]) -> Self;
}

pub enum IpPacket<'a> {
    V4(IpV4Packet<'a>),
    V6(IpV6Packet<'a>),
}

impl<'a> IpPacket<'a> {
    pub fn protocol(&self) -> u8 {
        match self {
            Self::V4(ip) => ip.protocol,
            Self::V6(ip) => ip.protocol,
        }
    }

    pub fn src_address(&self) -> &'a [u8] {
        match self {
            Self::V4(ip) => ip.src_address,
            Self::V6(ip) => ip.src_address,
        }
    }

    pub fn dst_address(&self) -> &'a [u8] {
        match self {
            Self::V4(ip) => ip.dst_address,
            Self::V6(ip) => ip.inferred_dst_address(),
        }
    }

    pub fn payload(&self) -> &'a [u8] {
        match self {
            Self::V4(ip) => ip.payload,
            Self::V6(ip) => ip.payload,
        }
    }

    pub fn payload_off(&self) -> usize {
        match self {
            Self::V4(ip) => ip.payload_off(),
            Self::V6(ip) => ip.payload_off(),
        }
    }

    pub fn to_tcp(&self) -> Option<TcpPacket> {
        match self.protocol() {
            6 => Some(TcpPacket::packet(self.payload())),
            _ => None,
        }
    }

    pub fn is_broadcast(&self) -> bool {
        match self {
            Self::V4(ip) => ip.dst_address[0..4] == [255, 255, 255, 255],
            Self::V6(_) => false,
        }
    }

    pub fn is_multicast(&self) -> bool {
        match self {
            Self::V4(ip) => ip.dst_address[0] & 0xf0 == 224,
            Self::V6(ip) => ip.dst_address[0] == 0xff,
        }
    }
}

impl<'a> PeekPacket<'a> for IpPacket<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        match data[0] >> 4 {
            4 => IpV4Packet::peek(data),
            6 => IpV6Packet::peek(data),
            _ => Err(Error::PacketMalformed("IP: invalid version".into())),
        }
    }

    fn packet(data: &'a [u8]) -> Self {
        if data[0] >> 4 == 4 {
            Self::V4(IpV4Packet::packet(data))
        } else {
            Self::V6(IpV6Packet::packet(data))
        }
    }
}

pub struct IpV4Field;
impl IpV4Field {
    pub const IHL: BitField = (0, 4..8);
    pub const TOTAL_LEN: Field = 2..4;
    pub const PROTOCOL: Field = 9..10;
    pub const SRC_ADDR: Field = 12..16;
    pub const DST_ADDR: Field = 16..20;
}

pub struct IpV4Packet<'a> {
    pub src_address: &'a [u8],
    pub dst_address: &'a [u8],
    pub protocol: u8,
    pub payload: &'a [u8],
    pub payload_off: usize,
}

impl<'a> IpV4Packet<'a> {
    pub const MIN_HEADER_LEN: usize = 20;

    #[inline]
    pub fn payload_off(&self) -> usize {
        self.payload_off
    }

    fn read_header_len(data: &'a [u8]) -> usize {
        let ihl = get_bit_field(data, IpV4Field::IHL) as usize;
        if ihl >= 5 {
            4 * ihl
        } else {
            Self::MIN_HEADER_LEN
        }
    }
}

impl<'a> PeekPacket<'a> for IpV4Packet<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        let data_len = data.len();
        if data_len < Self::MIN_HEADER_LEN {
            return Err(Error::PacketMalformed("IPv4: header too short".into()));
        }

        let len = ntoh_u16(&data[IpV4Field::TOTAL_LEN]).unwrap() as usize;
        let payload_off = Self::read_header_len(data);

        if len < payload_off {
            return Err(Error::PacketMalformed("IPv4: payload too short".into()));
        }
        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload_off = Self::read_header_len(data);

        Self {
            src_address: &data[IpV4Field::SRC_ADDR],
            dst_address: &data[IpV4Field::DST_ADDR],
            protocol: data[IpV4Field::PROTOCOL][0],
            payload: &data[payload_off..],
            payload_off,
        }
    }
}

pub struct IpV6Field;
impl IpV6Field {
    pub const PAYLOAD_LEN: Field = 4..6;
    pub const PROTOCOL: Field = 6..7;
    pub const SRC_ADDR: Field = 8..24;
    pub const DST_ADDR: Field = 24..40;
    pub const PAYLOAD: Rest = 40..; // extension headers are not supported
}

pub struct IpV6Packet<'a> {
    pub src_address: &'a [u8],
    dst_address: &'a [u8],
    pub protocol: u8,
    pub payload: &'a [u8],
    pub next_header: IpV6NextHeader<'a>,
}

impl<'a> IpV6Packet<'a> {
    pub const MIN_HEADER_LEN: usize = 40;

    #[inline]
    pub fn payload_off(&self) -> usize {
        IpV6Field::PAYLOAD.start
    }

    pub fn inferred_dst_address(&self) -> &'a [u8] {
        match &self.next_header {
            IpV6NextHeader::IcmpV6(icmp_v6) => icmp_v6.dst_address().unwrap_or(self.dst_address),
            IpV6NextHeader::Other => self.dst_address,
        }
    }
}

impl<'a> PeekPacket<'a> for IpV6Packet<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        let data_len = data.len();
        if data_len < Self::MIN_HEADER_LEN {
            return Err(Error::PacketMalformed("IPv6: header too short".into()));
        }

        let len = Self::MIN_HEADER_LEN + ntoh_u16(&data[IpV6Field::PAYLOAD_LEN]).unwrap() as usize;
        if data_len < len as usize {
            return Err(Error::PacketMalformed("IPv6: payload too short".into()));
        } else if len == Self::MIN_HEADER_LEN {
            return Err(Error::ProtocolNotSupported("IPv6: jumbogram".into()));
        }

        if data[IpV6Field::PROTOCOL][0] == 58 {
            IcmpV6Packet::peek(&data[IpV6Field::PAYLOAD])?;
        };

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload = &data[IpV6Field::PAYLOAD];
        let protocol = data[IpV6Field::PROTOCOL][0];
        let next_header = match protocol {
            58 => IpV6NextHeader::IcmpV6(IcmpV6Packet::packet(payload)),
            _ => IpV6NextHeader::Other,
        };

        Self {
            src_address: &data[IpV6Field::SRC_ADDR],
            dst_address: &data[IpV6Field::DST_ADDR],
            protocol,
            payload,
            next_header,
        }
    }
}

pub enum IpV6NextHeader<'a> {
    IcmpV6(IcmpV6Packet<'a>),
    Other,
}

pub struct IcmpV6Field;
impl IcmpV6Field {
    pub const TYPE: Field = 0..1;
    pub const CODE: Field = 1..2;
    pub const CHECKSUM: Field = 2..4;
    pub const PAYLOAD: Rest = 4..;
}

pub struct IcmpV6Packet<'a> {
    pub type_: u8,
    pub message: IcmpV6Message<'a>,
}

impl<'a> IcmpV6Packet<'a> {
    pub const MIN_HEADER_SIZE: usize = 4;

    pub fn dst_address(&self) -> Option<&'a [u8]> {
        match &self.message {
            IcmpV6Message::NdpNeighborSolicitation { dst_address, .. } => Some(dst_address),
            _ => None,
        }
    }
}

impl<'a> PeekPacket<'a> for IcmpV6Packet<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        let data_len = data.len();
        if data_len < Self::MIN_HEADER_SIZE {
            return Err(Error::PacketMalformed("ICMPv6: packet too short".into()));
        }

        match data[IcmpV6Field::TYPE][0] {
            135 => {
                if data_len < Self::MIN_HEADER_SIZE + 16 {
                    return Err(Error::PacketMalformed("ICMPv6: payload too short".into()));
                }
            }
            _ => {
                if data_len < Self::MIN_HEADER_SIZE {
                    return Err(Error::PacketMalformed("ICMPv6: payload too short".into()));
                }
            }
        }

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let type_ = data[IcmpV6Field::TYPE][0];
        let payload = &data[IcmpV6Field::PAYLOAD];
        Self {
            type_,
            message: match type_ {
                IcmpV6Message::NDP_ROUTER_SOLICITATION => IcmpV6Message::NdpRouterSolicitation {},
                IcmpV6Message::NDP_ROUTER_ADVERTISEMENT => IcmpV6Message::NdpRouterAdvertisement {},
                IcmpV6Message::NDP_NEIGHBOR_SOLICITATION => {
                    IcmpV6Message::NdpNeighborSolicitation {
                        dst_address: &payload[NdpNeighborSolicitationField::TARGET_ADDRESS],
                    }
                }
                IcmpV6Message::NDP_NEIGHBOR_ADVERTISEMENT => {
                    IcmpV6Message::NdpNeighborAdvertisement {}
                }
                IcmpV6Message::NDP_REDIRECT => IcmpV6Message::NdpRedirect {},
                _ => IcmpV6Message::Other,
            },
        }
    }
}

pub enum IcmpV6Message<'a> {
    NdpRouterSolicitation {},
    NdpRouterAdvertisement {},
    NdpNeighborSolicitation { dst_address: &'a [u8] },
    NdpNeighborAdvertisement {},
    NdpRedirect {},
    Other,
}

impl<'a> IcmpV6Message<'a> {
    pub const NDP_ROUTER_SOLICITATION: u8 = 133;
    pub const NDP_ROUTER_ADVERTISEMENT: u8 = 134;
    pub const NDP_NEIGHBOR_SOLICITATION: u8 = 135;
    pub const NDP_NEIGHBOR_ADVERTISEMENT: u8 = 136;
    pub const NDP_REDIRECT: u8 = 137;
}

pub struct NdpNeighborSolicitationField;
impl NdpNeighborSolicitationField {
    pub const TARGET_ADDRESS: Field = 4..20;
}

pub struct NdpRouterAdvertisementField;
impl NdpRouterAdvertisementField {
    /// Hop count to set
    pub const CUR_HOP_LIMIT: Field = 0..1;
    /// Managed address configuration flag
    pub const M: BitField = (1, 0..1);
    /// Other configuration flag
    pub const O: BitField = (1, 1..2);
    /// Router lifetime in seconds
    pub const ROUTER_LIFETIME: Field = 2..4;
    /// The time, in milliseconds, that a node assumes a neighbor is
    /// reachable after having received a reachability confirmation
    pub const REACHABLE_TIME: Field = 4..8;
    /// The time, in milliseconds, between retransmitted Neighbor Solicitation messages
    pub const RETRANS_TIMER: Field = 8..16;
}

pub struct NdpNeighborAdvertisementField;
impl NdpNeighborAdvertisementField {
    /// Router flag
    pub const R: BitField = (0, 0..1);
    /// Solicited flag
    pub const S: BitField = (0, 1..2);
    /// Override flag
    pub const O: BitField = (0, 2..3);
    /// Target address
    pub const TARGET_ADDRESS: Field = 4..20;
}

pub struct ArpField;
impl ArpField {
    /// Hardware type
    pub const HTYPE: Field = 0..2;
    /// Protocol type
    pub const PTYPE: Field = 2..4;
    /// Hardware length
    pub const HLEN: Field = 4..5;
    /// Protocol length
    pub const PLEN: Field = 5..6;
    /// Operation
    pub const OP: Field = 6..8;
    /// Sender hardware address
    pub const SHA: Field = 8..14;
    /// Sender protocol address
    pub const SPA: Field = 14..18;
    /// Target hardware address
    pub const THA: Field = 18..24;
    /// Target protocol address
    pub const TPA: Field = 24..28;
}

pub struct ArpPacket<'a> {
    inner: &'a [u8],
}

impl<'a> ArpPacket<'a> {
    #[inline(always)]
    pub fn get_field(&self, field: Field) -> &[u8] {
        &self.inner[field]
    }
}

impl<'a> PeekPacket<'a> for ArpPacket<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        if data.len() < 28 {
            return Err(Error::PacketMalformed("ARP: packet too short".into()));
        }
        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        Self { inner: data }
    }
}

pub struct ArpPacketMut<'a> {
    inner: &'a mut [u8],
}

impl<'a> ArpPacketMut<'a> {
    pub fn set_field(&mut self, field: Field, value: &[u8]) {
        let value = &value[..field.end];
        self.inner[field].copy_from_slice(value);
    }

    pub fn freeze(self) -> ArpPacket<'a> {
        ArpPacket { inner: self.inner }
    }
}

pub struct TcpField;
impl TcpField {
    pub const SRC_PORT: Field = 0..2;
    pub const DST_PORT: Field = 2..4;
    pub const DATA_OFF: BitField = (12, 0..4);
}

pub struct TcpPacket<'a> {
    pub src_port: &'a [u8],
    pub dst_port: &'a [u8],
    pub payload_off: usize,
    pub payload_size: usize,
}

impl<'a> TcpPacket<'a> {
    pub fn src_port(&self) -> u16 {
        ntoh_u16(self.src_port).unwrap()
    }

    pub fn dst_port(&self) -> u16 {
        ntoh_u16(self.dst_port).unwrap()
    }
}

impl<'a> PeekPacket<'a> for TcpPacket<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        if data.len() < 20 {
            return Err(Error::PacketMalformed("TCP: packet too short".into()));
        }

        let payload_off = get_bit_field(data, TcpField::DATA_OFF) as usize;
        if data.len() < payload_off {
            return Err(Error::PacketMalformed("TCP: packet too short".into()));
        }

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload_off = get_bit_field(data, TcpField::DATA_OFF) as usize;
        let payload_size = data.len().saturating_sub(payload_off);
        Self {
            src_port: &data[TcpField::SRC_PORT],
            dst_port: &data[TcpField::DST_PORT],
            payload_off,
            payload_size,
        }
    }
}

pub struct UdpField;
impl UdpField {
    pub const SRC_PORT: Field = 0..2;
    pub const DST_PORT: Field = 2..4;
    pub const LEN: Field = 4..6;
    pub const PAYLOAD: Rest = 8..;
}

pub struct UdpPacket<'a> {
    pub src_port: &'a [u8],
    pub dst_port: &'a [u8],
    pub payload_size: usize,
}

impl<'a> UdpPacket<'a> {
    pub fn src_port(&self) -> u16 {
        ntoh_u16(self.src_port).unwrap()
    }

    pub fn dst_port(&self) -> u16 {
        ntoh_u16(self.dst_port).unwrap()
    }
}

impl<'a> PeekPacket<'a> for UdpPacket<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        if data.len() < 8 {
            return Err(Error::PacketMalformed("UDP: packet too short".into()));
        }

        let len = ntoh_u16(&data[UdpField::LEN]).unwrap() as usize;
        if data.len() < len {
            return Err(Error::PacketMalformed("UDP: packet too short".into()));
        }

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload_size = data.len().saturating_sub(UdpField::PAYLOAD.start);
        Self {
            src_port: &data[UdpField::SRC_PORT],
            dst_port: &data[UdpField::DST_PORT],
            payload_size,
        }
    }
}

/// Convert `IpAddress` to boxed bytes
#[inline(always)]
pub fn ip_hton(ip: IpAddress) -> Box<[u8]> {
    match ip {
        IpAddress::Ipv4(ip) => ip.as_bytes().into(),
        IpAddress::Ipv6(ip) => ip.as_bytes().into(),
        _ => vec![0, 0, 0, 0].into(),
    }
}

/// Convert a byte slice to `IpAddress`
#[inline(always)]
pub fn ip_ntoh(data: &[u8]) -> Option<IpAddress> {
    if data.len() == 4 {
        Some(IpAddress::v4(data[0], data[1], data[2], data[3]))
    } else if data.len() == 16 {
        Some(IpAddress::Ipv6(Ipv6Address::from_bytes(data)))
    } else {
        None
    }
}

pub fn write_field(data: &mut [u8], field: Field, value: &[u8]) -> bool {
    if value.len() == field.len() && data.len() >= field.end {
        data[field].copy_from_slice(value);
        true
    } else {
        false
    }
}

/// Read a bit field spanning over a single byte
#[inline(always)]
pub fn read_bit_field(data: &[u8], bit_field: BitField) -> Option<u8> {
    if data.len() >= bit_field.0 && bit_field.1.len() <= 8 {
        Some(get_bit_field(data, bit_field))
    } else {
        None
    }
}

/// Write a bit field spanning over a single byte
#[inline(always)]
pub fn write_bit_field(data: &mut [u8], bit_field: BitField, value: u8) -> bool {
    if data.len() >= bit_field.0 && bit_field.1.len() <= 8 {
        set_bit_field(data, bit_field, value);
        true
    } else {
        false
    }
}

#[inline(always)]
fn get_bit_field(data: &[u8], bit_field: BitField) -> u8 {
    (data[bit_field.0] << bit_field.1.start) >> (8 - bit_field.1.len())
}

#[inline(always)]
fn set_bit_field(data: &mut [u8], bit_field: BitField, value: u8) {
    let bit_len = bit_field.1.len();
    let mask = (0xff_u8 << (8 - bit_len)) >> bit_field.1.start;
    data[bit_field.0] &= !mask;
    data[bit_field.0] |= (value << (8 - bit_len)) >> bit_field.1.start;
}

macro_rules! impl_ntoh_n {
    ($ident:ident, $ty:ty, $n:tt) => {
        fn $ident(data: &[u8]) -> Option<$ty> {
            match data.len() {
                $n => {
                    let mut result = [0u8; $n];
                    result.copy_from_slice(&data[0..$n]);
                    Some(<$ty>::from_be_bytes(result))
                }
                _ => None,
            }
        }
    };
}

impl_ntoh_n!(ntoh_u16, u16, 2);
impl_ntoh_n!(ntoh_u32, u32, 4);
impl_ntoh_n!(ntoh_u64, u64, 8);

#[cfg(test)]
mod tests {
    use crate::packet::{get_bit_field, set_bit_field};

    #[test]
    fn change_bit_field() {
        for len in 1..8 {
            for off in 0..8 {
                let mut bytes = [0u8; 1];
                let range = off..8.min(off + len);
                println!("range: {:?}", range);

                set_bit_field(&mut bytes, (0, range.clone()), 0xff);
                println!("byte: {:#010b}", bytes[0]);

                assert_eq!(
                    get_bit_field(&bytes, (0, range.clone())),
                    0xff_u8 >> (8 - range.len())
                );
            }
        }
    }
}
