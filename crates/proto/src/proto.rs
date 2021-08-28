use crate::codec::DecodeError;
use prost::encoding::{decode_key, encode_key, WireType};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

include!(concat!(env!("OUT_DIR"), "/ya_relay_proto.rs"));

pub use control::Kind as ControlKind;
pub use control::{PauseForwarding, ResumeForwarding, ReverseConnection, StopForwarding};
pub use packet::Kind as PacketKind;
pub use request::Kind as RequestKind;
pub use response::Kind as ResponseKind;

pub const SESSION_ID_SIZE: usize = 16;
pub const KEY_SIZE: usize = 1;
pub const FORWARD_TAG: u32 = 1;

const REQUEST_ID: AtomicU64 = AtomicU64::new(0);

pub type RequestId = u64;

#[derive(Clone, Default, PartialEq)]
pub struct Forward {
    pub session_id: [u8; SESSION_ID_SIZE],
    pub slot: u32,
    pub payload: bytes::BytesMut,
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

    pub fn decode(mut buf: bytes::BytesMut) -> Result<Self, DecodeError> {
        if buf.len() < Self::header_size() {
            return Err(DecodeError::PacketTooShort);
        }

        let (tag, _) = decode_key(&mut buf).map_err(|_| DecodeError::PacketFormatInvalid)?;
        if tag != FORWARD_TAG {
            return Err(DecodeError::PacketFormatInvalid);
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

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Forward( ")?;
        write!(f, "session_id: {:2x?}, ", self.session_id)?;
        write!(f, "slot: {}, payload: ({}) ", self.slot, self.payload.len())?;
        write_payload_fmt(f, &self.payload)?;
        write!(f, " )")
    }
}

fn write_payload_fmt(f: &mut std::fmt::Formatter<'_>, buf: &[u8]) -> std::fmt::Result {
    if buf.len() > 16 {
        let idx = 8.min(buf.len() / 2);
        write!(f, "{:02x?}..{:02x?}", &buf[..idx], &buf[buf.len() - idx..])
    } else {
        write!(f, "{:02x?}", &buf)
    }
}

impl Packet {
    pub fn request(session_id: Vec<u8>, kind: impl Into<request::Kind>) -> Self {
        Packet {
            session_id,
            kind: Some(packet::Kind::Request(Request::from(kind))),
        }
    }

    pub fn response(
        request_id: RequestId,
        session_id: Vec<u8>,
        code: impl Into<i32>,
        kind: impl Into<response::Kind>,
    ) -> Self {
        Packet {
            session_id,
            kind: Some(packet::Kind::Response(Response {
                request_id,
                code: code.into(),
                kind: Some(kind.into()),
            })),
        }
    }

    pub fn error(request_id: RequestId, session_id: Vec<u8>, code: impl Into<i32>) -> Self {
        Packet {
            session_id,
            kind: Some(packet::Kind::Response(Response {
                request_id,
                code: code.into(),
                kind: None,
            })),
        }
    }

    pub fn control(session_id: Vec<u8>, kind: impl Into<control::Kind>) -> Self {
        Packet {
            session_id,
            kind: Some(packet::Kind::Control(Control {
                kind: Some(kind.into()),
            })),
        }
    }
}

impl<T> From<T> for Request
where
    T: Into<request::Kind>,
{
    fn from(t: T) -> Self {
        Request {
            request_id: REQUEST_ID.fetch_add(1, SeqCst),
            kind: Some(t.into()),
        }
    }
}

macro_rules! impl_convert_kind {
    ($module:ident, $ident:ident) => {
        impl Into<$crate::proto::$module::Kind> for $crate::proto::$module::$ident {
            fn into(self) -> $crate::proto::$module::Kind {
                $crate::proto::$module::Kind::$ident(self)
            }
        }

        impl std::convert::TryInto<$crate::proto::$module::$ident>
            for $crate::proto::$module::Kind
        {
            type Error = ();

            fn try_into(self) -> Result<$crate::proto::$module::$ident, Self::Error> {
                match self {
                    $crate::proto::$module::Kind::$ident(kind) => Ok(kind),
                    _ => Err(()),
                }
            }
        }
    };
}

impl_convert_kind!(request, Session);
impl_convert_kind!(request, Register);
impl_convert_kind!(request, Node);
impl_convert_kind!(request, Neighbours);
impl_convert_kind!(request, ReverseConnection);
impl_convert_kind!(request, Ping);

impl_convert_kind!(response, Challenge);
impl_convert_kind!(response, Session);
impl_convert_kind!(response, Register);
impl_convert_kind!(response, Node);
impl_convert_kind!(response, Neighbours);
impl_convert_kind!(response, Pong);

impl_convert_kind!(control, ReverseConnection);
impl_convert_kind!(control, PauseForwarding);
impl_convert_kind!(control, ResumeForwarding);
impl_convert_kind!(control, StopForwarding);
