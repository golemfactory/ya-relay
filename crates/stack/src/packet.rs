#![allow(unused)]

use self::field::*;
use std::convert::TryFrom;
use std::ops::Deref;

use crate::{Error, Protocol};

pub const ETHERNET_HDR_SIZE: usize = 14;

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

    pub fn payload(&self) -> &[u8] {
        &self.as_ref()[EtherField::PAYLOAD]
    }

    pub fn reply(&self, mut payload: Vec<u8>) -> Vec<u8> {
        let frame: &[u8] = self.as_ref();
        payload.reserve(EtherField::PAYLOAD.start);
        payload.splice(0..0, frame[EtherField::ETHER_TYPE].iter().cloned());
        payload.splice(0..0, frame[EtherField::DST_MAC].iter().cloned());
        payload.splice(0..0, frame[EtherField::SRC_MAC].iter().cloned());
        payload
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

    pub fn protocol(&self) -> u8 {
        match self {
            Self::V4(ip) => ip.protocol,
            Self::V6(ip) => ip.protocol,
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

pub struct Ipv4Field;
impl Ipv4Field {
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
}

impl<'a> IpV4Packet<'a> {
    pub const MIN_HEADER_LEN: usize = 20;
}

impl<'a> PeekPacket<'a> for IpV4Packet<'a> {
    fn peek(data: &'a [u8]) -> Result<(), Error> {
        let data_len = data.len();
        if data_len < Self::MIN_HEADER_LEN {
            return Err(Error::PacketMalformed("IPv4: header too short".into()));
        }

        let len = ntoh_u16(&data[Ipv4Field::TOTAL_LEN]).unwrap() as usize;
        let payload_off = Self::MIN_HEADER_LEN + 4 * get_bit_field(data, Ipv4Field::IHL) as usize;
        if data_len < len || len < payload_off {
            return Err(Error::PacketMalformed("IPv4: payload too short".into()));
        }
        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload_off = get_bit_field(data, Ipv4Field::IHL) as usize * 4 + 20;
        Self {
            src_address: &data[Ipv4Field::SRC_ADDR],
            dst_address: &data[Ipv4Field::DST_ADDR],
            protocol: data[Ipv4Field::PROTOCOL][0],
            payload: &data[payload_off..],
        }
    }
}

pub struct Ipv6Field;
impl Ipv6Field {
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

        let len = Self::MIN_HEADER_LEN + ntoh_u16(&data[Ipv6Field::PAYLOAD_LEN]).unwrap() as usize;
        if data_len < len as usize {
            return Err(Error::PacketMalformed("IPv6: payload too short".into()));
        } else if len == Self::MIN_HEADER_LEN {
            return Err(Error::ProtocolNotSupported("IPv6: jumbogram".into()));
        }

        if data[Ipv6Field::PROTOCOL][0] == 58 {
            IcmpV6Packet::peek(&data[Ipv6Field::PAYLOAD])?;
        };

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let payload = &data[Ipv6Field::PAYLOAD];
        let protocol = data[Ipv6Field::PROTOCOL][0];
        let next_header = match protocol {
            58 => IpV6NextHeader::IcmpV6(IcmpV6Packet::packet(payload)),
            _ => IpV6NextHeader::Other,
        };

        Self {
            src_address: &data[Ipv6Field::SRC_ADDR],
            dst_address: &data[Ipv6Field::DST_ADDR],
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
            135 | 136 => {
                if data_len < Self::MIN_HEADER_SIZE + 16 {
                    return Err(Error::PacketMalformed("ICMPv6: payload too short".into()));
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        let type_ = data[IcmpV6Field::TYPE][0];
        let payload = &data[IcmpV6Field::PAYLOAD];
        Self {
            type_,
            message: match type_ {
                135 => IcmpV6Message::NdpNeighborSolicitation {
                    dst_address: &payload[NdpField::TARGET_ADDRESS],
                },
                _ => IcmpV6Message::Other,
            },
        }
    }
}

pub enum IcmpV6Message<'a> {
    NdpNeighborSolicitation { dst_address: &'a [u8] },
    Other,
}

pub struct NdpField;
impl NdpField {
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

    pub fn mirror(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ArpField::TPA.end);
        buf.extend(self.inner[0..ArpField::OP.start].iter().cloned());
        buf.extend(self.mirror_op().iter().cloned());
        buf.extend(self.get_field(ArpField::THA).iter().cloned());
        buf.extend(self.get_field(ArpField::TPA).iter().cloned());
        buf.extend(self.get_field(ArpField::SHA).iter().cloned());
        buf.extend(self.get_field(ArpField::SPA).iter().cloned());
        buf
    }

    fn mirror_op(&self) -> [u8; 2] {
        let op = self.get_field(ArpField::OP);
        // request
        if op == [0x00, 0x01] {
            // reply
            [0x00, 0x02]
        } else if op == [0x00, 0x02] {
            [0x00, 0x01]
        } else {
            let mut ret = [0u8; 2];
            ret.copy_from_slice(op);
            ret
        }
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

        let off = get_bit_field(data, TcpField::DATA_OFF) as usize;
        if data.len() < off {
            return Err(Error::PacketMalformed("TCP: packet too short".into()));
        }

        Ok(())
    }

    fn packet(data: &'a [u8]) -> Self {
        Self {
            src_port: &data[TcpField::SRC_PORT],
            dst_port: &data[TcpField::DST_PORT],
        }
    }
}

#[inline(always)]
fn get_bit_field(data: &[u8], bit_field: BitField) -> u8 {
    (data[bit_field.0] << bit_field.1.start) >> (bit_field.1.start + (8 - bit_field.1.end))
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
