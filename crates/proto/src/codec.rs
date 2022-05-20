use bytes::{Buf, BufMut};
use derive_more::From;

pub use crate::codec::error::*;
use crate::proto::{Forward, Packet};
pub use bytes::BytesMut;

pub mod datagram;
mod error;
pub mod forward;
pub mod stream;

pub const MAX_PACKET_SIZE: u32 = 2097151;
pub const MAX_PARSE_MESSAGE_SIZE: usize = 600;

#[derive(Clone, Debug, PartialEq, From)]
pub enum PacketKind {
    /// Protobuf packet
    Packet(Packet),
    /// Bytes to forward to another node
    Forward(Forward),
    /// Bytes to forward, continuation (stream only)
    ForwardCtd(BytesMut),
}

impl PacketKind {
    pub fn request_id(&self) -> Option<u64> {
        use crate::proto;

        match self {
            PacketKind::Packet(proto::Packet {
                kind: Some(proto::packet::Kind::Response(proto::Response { request_id, .. })),
                ..
            }) => Some(*request_id),
            PacketKind::Packet(proto::Packet {
                kind: Some(proto::packet::Kind::Request(proto::Request { request_id, .. })),
                ..
            }) => Some(*request_id),
            _ => None,
        }
    }

    pub fn session_id(&self) -> Vec<u8> {
        match self {
            PacketKind::Packet(Packet { session_id, .. }) => session_id.clone(),
            PacketKind::Forward(Forward { session_id, .. }) => session_id.to_vec(),
            PacketKind::ForwardCtd(_) => vec![],
        }
    }
}

#[inline(always)]
pub(self) fn read_bytes(buf: &mut BytesMut, max: usize) -> Result<Option<BytesMut>, DecodeError> {
    let (total, off) = peek_size(buf)?;
    read_bytes_inner(buf, total, off, max)
}

#[inline(always)]
pub(self) fn read_datagram(buf: &mut BytesMut) -> Result<Option<BytesMut>, DecodeError> {
    let total = buf.len();
    read_bytes_inner(buf, total, 0, total)
}

pub(self) fn read_bytes_inner(
    buf: &mut BytesMut,
    total: usize,
    off: usize,
    max: usize,
) -> Result<Option<BytesMut>, DecodeError> {
    let tag = peek_tag(&buf[off..])?;
    let available = total.min(buf.len() - off);
    let left = total - available;

    match tag {
        Some(0) => {
            buf.advance(off + available);
            Err(DecodeError::PayloadInvalid { left })
        }
        Some(1) => {
            if buf.len() >= off + Forward::header_size() {
                buf.advance(off);
                let bytes = buf.split_to(available);
                Err(DecodeError::Forward { bytes, left })
            } else {
                Ok(None)
            }
        }
        Some(kind) if kind > 1 => {
            if total > max {
                buf.advance(off + available);
                Err(DecodeError::PayloadTooLong { left })
            } else if buf.len() >= off + total {
                buf.advance(off);
                let bytes = buf.split_to(available);
                Ok(Some(bytes))
            } else {
                Err(DecodeError::PayloadTooShort { left })
            }
        }
        _ => Ok(None),
    }
}

#[inline]
fn peek_tag(buf: &[u8]) -> Result<Option<u32>, DecodeError> {
    let tag = match buf.get(0) {
        Some(tag) => *tag,
        None => return Ok(None),
    };

    if tag < 0x80 {
        return Ok(Some(tag as u32 >> 3));
    }
    Err(DecodeError::PrefixTooLong)
}

// See prost::encoding::decode_varint_slice
#[inline]
pub(crate) fn peek_size(buf: &[u8]) -> Result<(usize, usize), DecodeError> {
    let mut b: u8 = *buf.get(0).ok_or(DecodeError::PrefixTooShort)?;
    let mut part0: u32 = u32::from(b);
    if b < 0x80 {
        return Ok((part0 as usize, 1));
    };
    part0 -= 0x80;
    b = *buf.get(1).ok_or(DecodeError::PrefixTooShort)?;
    part0 += u32::from(b) << 7;
    if b < 0x80 {
        return Ok((part0 as usize, 2));
    };
    part0 -= 0x80 << 7;
    b = *buf.get(2).ok_or(DecodeError::PrefixTooShort)?;
    part0 += u32::from(b) << 14;
    if b < 0x80 {
        return Ok((part0 as usize, 3));
    };

    Err(DecodeError::PrefixTooLong)
}

#[inline]
pub(crate) fn write_size<B: BufMut>(size: u32, buf: &mut B) -> Result<(), EncodeError> {
    match size {
        0 => Err(EncodeError::NoData),
        sz if sz > MAX_PACKET_SIZE => Err(EncodeError::PacketTooLong { size: sz as usize }),
        _ => {
            prost::encoding::encode_varint(size as u64, buf);
            Ok(())
        }
    }
}
