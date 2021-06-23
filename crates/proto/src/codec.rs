use bytes::{Buf, BufMut, BytesMut};

pub mod datagram;
mod error;
pub mod stream;

use crate::proto::{Forward, Packet};
pub use error::*;

const MAX_PARSED_MESSAGE_BYTES: usize = 600;
const MAX_SIZE_VALUE: u32 = 2097151;

#[derive(Clone, Debug, PartialEq)]
pub enum PacketKind {
    Packet(Packet),
    Forward(Forward),
    ForwardCtd(BytesMut),
}

#[inline(always)]
pub(self) fn read_bytes(buf: &mut BytesMut, max: usize) -> Result<Option<BytesMut>, ParseError> {
    let (total, off) = peek_size(buf.bytes())?;
    read_bytes_inner(buf, total, off, max)
}

#[inline(always)]
pub(self) fn read_datagram(buf: &mut BytesMut) -> Result<Option<BytesMut>, ParseError> {
    let total = buf.len();
    read_bytes_inner(buf, total, 0, total)
}

pub(self) fn read_bytes_inner(
    buf: &mut BytesMut,
    total: usize,
    off: usize,
    max: usize,
) -> Result<Option<BytesMut>, ParseError> {
    let tag = peek_tag(&buf[off..])?;
    let available = total.min(buf.len() - off);
    let left = total - available;

    match tag {
        Some(0) => {
            let _ = buf.split_to(off + available);
            Err(ParseError::PayloadInvalid { total, left })
        }
        Some(1) => {
            if buf.len() >= off + Forward::header_size() {
                let _ = buf.split_to(off);
                let bytes = buf.split_to(available);
                Err(ParseError::Forward { bytes, left })
            } else {
                Ok(None)
            }
        }
        Some(kind) if kind > 1 => {
            if total > max {
                let _ = buf.split_to(off + available);
                Err(ParseError::PayloadTooLong { total, left })
            } else if buf.len() >= off + total {
                let _ = buf.split_to(off);
                let bytes = buf.split_to(available);
                Ok(Some(bytes))
            } else {
                Err(ParseError::PayloadTooShort { total, left })
            }
        }
        _ => Ok(None),
    }
}

#[inline]
fn peek_tag(buf: &[u8]) -> Result<Option<u32>, ParseError> {
    let tag = match buf.get(0) {
        Some(tag) => *tag,
        None => return Ok(None),
    };
    if tag < 0x80 {
        return Ok(Some(tag as u32 >> 3));
    }
    Err(ParseError::PrefixTooLong)
}

// See prost::encoding::decode_varint_slice
#[inline]
pub(crate) fn peek_size(buf: &[u8]) -> Result<(usize, usize), ParseError> {
    let mut b: u8;
    let mut part0: u32;

    b = *buf.get(0).ok_or_else(|| ParseError::PrefixTooShort)?;
    part0 = u32::from(b);
    if b < 0x80 {
        return Ok((part0 as usize, 1));
    };
    part0 -= 0x80;
    b = *buf.get(1).ok_or_else(|| ParseError::PrefixTooShort)?;
    part0 += u32::from(b) << 7;
    if b < 0x80 {
        return Ok((part0 as usize, 2));
    };
    part0 -= 0x80 << 7;
    b = *buf.get(2).ok_or_else(|| ParseError::PrefixTooShort)?;
    part0 += u32::from(b) << 14;
    if b < 0x80 {
        return Ok((part0 as usize, 3));
    };

    Err(ParseError::PrefixTooLong)
}

#[inline]
pub(crate) fn write_size<B: BufMut>(value: u32, buf: &mut B) -> Result<(), ParseError> {
    if value > MAX_SIZE_VALUE {
        return Err(ParseError::InvalidSize(value as usize));
    }
    Ok(prost::encoding::encode_varint(value as u64, buf))
}
