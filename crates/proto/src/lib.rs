#[cfg(feature = "codec")]
pub mod codec;

pub mod proto {
    use crate::codec::ParseError;
    use prost::encoding::{decode_key, encode_key, WireType};

    include!(concat!(env!("OUT_DIR"), "/ya_relay_proto.rs"));

    pub const KEY_SIZE: usize = 1;
    pub const SESSION_ID_SIZE: usize = 16;
    pub const FORWARD_TAG: u32 = 1;

    #[derive(Clone, Default, PartialEq)]
    pub struct Forward {
        pub session_id: [u8; SESSION_ID_SIZE],
        pub slot: u32,
        pub payload: bytes::BytesMut,
    }

    impl std::fmt::Debug for Forward {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Forward( ")?;
            write!(f, "session_id: {:2x?}, ", self.session_id)?;
            write!(f, "slot: {}, payload: ({}) ", self.slot, self.payload.len())?;
            write_payload_fmt(f, &self.payload)?;
            write!(f, " )")
        }
    }

    impl Forward {
        #[inline]
        pub const fn header_size() -> usize {
            KEY_SIZE + SESSION_ID_SIZE + std::mem::size_of::<u32>()
        }

        #[inline]
        pub fn encoded_len(&self) -> usize {
            Self::header_size() + self.payload.len()
        }

        pub fn encode(self, buf: &mut bytes::BytesMut) {
            encode_key(FORWARD_TAG, WireType::LengthDelimited, buf);
            buf.extend_from_slice(&self.session_id);
            buf.extend_from_slice(&self.slot.to_le_bytes());
            buf.extend(self.payload.into_iter());
        }

        pub fn decode(mut buf: bytes::BytesMut) -> Result<Self, ParseError> {
            if buf.len() < Self::header_size() {
                return Err(ParseError::InvalidSize(buf.len()));
            }

            let (tag, _) = decode_key(&mut buf).map_err(|_| ParseError::InvalidFormat)?;
            if tag != FORWARD_TAG {
                return Err(ParseError::InvalidFormat);
            }

            let mut session_id = [0u8; SESSION_ID_SIZE];
            session_id.copy_from_slice(&buf.split_to(SESSION_ID_SIZE));

            let slot = buf.split_to(4);
            let slot = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);

            Ok(Forward {
                session_id,
                slot,
                payload: buf,
            })
        }
    }

    fn write_payload_fmt(f: &mut std::fmt::Formatter<'_>, buf: &[u8]) -> std::fmt::Result {
        if buf.len() > 32 {
            let idx = 16.min(buf.len() / 2);
            write!(f, "{:02x?}..{:02x?}", &buf[..idx], &buf[buf.len() - idx..])
        } else {
            write!(f, "{:02x?}", &buf)
        }
    }
}
