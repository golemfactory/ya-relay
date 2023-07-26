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
        FramedRead::with_capacity(output, Self, MAX_PACKET_SIZE as usize)
    }

    pub fn sink(input: impl AsyncWrite) -> impl Sink<PacketKind, Error = Error> {
        FramedWrite::new(input, Self)
    }
}

impl Encoder<PacketKind> for Codec {
    type Error = Error;

    fn encode(&mut self, item: PacketKind, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            PacketKind::Packet(pkt) => {
                dst.reserve(pkt.encoded_len());
                pkt.encode(dst)?;
            }
            PacketKind::Forward(fwd) => {
                dst.reserve(fwd.encoded_len());
                fwd.encode(dst);
            }
            PacketKind::ForwardCtd(_) => return Err(EncodeError::PacketNotSupported.into()),
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use prost::Message;
    use tokio_util::codec::{Decoder, Encoder};

    use crate::codec;
    use crate::codec::datagram::Codec;
    use crate::codec::{DecodeError, Error, MAX_PACKET_SIZE};
    use crate::proto::{self, *};

    const SESSION_ID: [u8; SESSION_ID_SIZE] = [0x0f; SESSION_ID_SIZE];

    fn large_packet() -> request::Kind {
        request::Kind::Session(request::Session {
            challenge_resp: Some(proto::ChallengeResponse {
                solution: vec![0u8; MAX_PACKET_SIZE as usize - 128],
                signatures: vec![],
            }),
            ..Default::default()
        })
    }

    fn challenge_response() -> proto::ChallengeResponse {
        proto::ChallengeResponse {
            solution: vec![0x0d, 0x0e, 0x0a, 0x0d, 0x0b, 0x0e, 0x0e, 0x0f],
            signatures: vec![
                vec![0x0a, 0x0e, 0x0a, 0x0d, 0x0b, 0x0e, 0x0e, 0x0f],
                vec![0x0b, 0x0e, 0x0a, 0x0d, 0x0b, 0x0e, 0x0e, 0x0f],
                vec![0x0c, 0x0e, 0x0a, 0x0d, 0x0b, 0x0e, 0x0e, 0x0f],
            ],
        }
    }

    #[tokio::test]
    async fn decode_datagrams() {
        let packets = vec![
            proto::Packet::request(
                Vec::new(),
                request::Session {
                    challenge_resp: Some(challenge_response()),
                    identities: vec![proto::Identity {
                        node_id: vec![0x0c, 0x00, 0x0f, 0x0f, 0x0e, 0x0e],
                        public_key: vec![0x05, 0x0e, 0x0c],
                    }],
                    ..Default::default()
                },
            )
            .into(),
            proto::Packet::request(
                SESSION_ID.to_vec(),
                request::Session {
                    challenge_resp: Some(challenge_response()),
                    ..Default::default()
                },
            )
            .into(),
            codec::PacketKind::Forward(Forward {
                session_id: SESSION_ID,
                slot: 42,
                flags: 0,
                payload: (0..8192).map(|_| rand::random::<u8>()).collect(),
            }),
            proto::Packet::request(
                SESSION_ID.to_vec(),
                request::Node {
                    node_id: Vec::new(),
                    public_key: true,
                },
            )
            .into(),
        ];

        let mut codec_enc = Codec;
        let mut codec_dec = Codec;

        let decoded = packets
            .iter()
            .cloned()
            .map(|p| {
                let mut bytes = BytesMut::new();
                codec_enc.encode(p, &mut bytes).unwrap();
                bytes
            })
            .map(|mut b| codec_dec.decode(&mut b).unwrap().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(packets, decoded);
    }

    #[test]
    fn decode_size_err() {
        let packet = Packet::request(SESSION_ID.to_vec(), large_packet());
        let len = packet.encoded_len();

        let mut codec = Codec;
        let mut buf = BytesMut::with_capacity(len + prost::length_delimiter_len(len));

        packet
            .encode_length_delimited(&mut buf)
            .expect("serialization failed");

        assert!(matches!(
            codec.decode(&mut buf),
            Err(Error::Decode(DecodeError::PrefixTooLong))
        ))
    }
}
