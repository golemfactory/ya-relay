use bytes::BytesMut;
use futures::{Sink, Stream};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};

use crate::codec::*;

#[derive(Default)]
pub struct Codec;

impl Codec {
    pub fn stream(output: impl AsyncRead) -> impl Stream<Item = Result<PacketKind, Error>> {
        FramedRead::new(output, Self::default())
    }

    pub fn sink(input: impl AsyncWrite) -> impl Sink<PacketKind, Error = Error> {
        FramedWrite::new(input, Self::default())
    }
}

impl Encoder<PacketKind> for Codec {
    type Error = Error;

    fn encode(&mut self, item: PacketKind, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            PacketKind::Packet(pkt) => {
                reserve_and_encode(dst, pkt.encoded_len())?;
                pkt.encode(dst)?;
            }
            PacketKind::Forward(fwd) => {
                reserve_and_encode(dst, fwd.encoded_len())?;
                fwd.encode(dst);
            }
            PacketKind::ForwardCtd(buf) => {
                dst.reserve(buf.len());
                dst.extend(buf.into_iter())
            }
        }
        Ok(())
    }
}

#[inline]
fn reserve_and_encode(dst: &mut BytesMut, len: usize) -> Result<(), Error> {
    dst.reserve(prost::length_delimiter_len(len) + len);
    Ok(write_size(len as u32, dst)?)
}

impl Decoder for Codec {
    type Item = PacketKind;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match read_datagram(src) {
            Ok(Some(bytes)) => Ok(Some(PacketKind::Packet(Packet::decode(bytes)?))),
            Ok(None) => Err(DecodeError::PacketFormatInvalid.into()),
            Err(DecodeError::Forward { bytes, left, .. }) => match left {
                0 => Ok(Some(PacketKind::Forward(Forward::decode(bytes)?))),
                _ => Err(DecodeError::PayloadTooShort { left }.into()),
            },
            Err(err) => Err(err.into()),
        }
    }
}
