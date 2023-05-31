use anyhow::anyhow;
use std::convert::TryFrom;
use std::mem::size_of;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

use bytes::BytesMut;
use prost::encoding::{decode_key, encode_key, WireType};

use crate::codec::DecodeError;

pub use ya_relay_util::Payload;

include!(concat!(env!("OUT_DIR"), "/ya_relay_proto.rs"));

pub const FORWARD_SLOT_ID: SlotId = 0;
pub const FORWARD_TAG: u32 = 1;
pub const MAX_TAG_SIZE: usize = 5;
pub const SESSION_ID_SIZE: usize = 16;
pub const KEY_SIZE: usize = 1;
pub const UNRELIABLE_FLAG: u16 = 0x01;
pub const ENCRYPTED_FLAG: u16 = 0x02;

static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

pub type RequestId = u64;
pub type SlotId = u32;

#[derive(Clone, Default, PartialEq)]
#[repr(C)]
pub struct Forward {
    pub session_id: [u8; SESSION_ID_SIZE],
    pub slot: u32,
    pub flags: u16,
    pub payload: Payload,
}

impl Forward {
    #[inline]
    pub const fn header_size() -> usize {
        KEY_SIZE + SESSION_ID_SIZE + size_of::<u32>() + size_of::<u16>()
    }

    pub fn new(
        session_id: impl Into<[u8; SESSION_ID_SIZE]>,
        slot: u32,
        payload: impl Into<Payload>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            slot,
            flags: 0,
            payload: payload.into(),
        }
    }

    pub fn unreliable(
        session_id: impl Into<[u8; SESSION_ID_SIZE]>,
        slot: u32,
        payload: impl Into<Payload>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            slot,
            flags: UNRELIABLE_FLAG,
            payload: payload.into(),
        }
    }

    #[inline]
    pub fn is_reliable(&self) -> bool {
        self.flags & UNRELIABLE_FLAG != UNRELIABLE_FLAG
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & ENCRYPTED_FLAG != ENCRYPTED_FLAG
    }

    pub fn set_encrypted(&mut self) {
        self.flags &= ENCRYPTED_FLAG
    }

    #[inline]
    pub fn encoded_len(&self) -> usize {
        Self::header_size() + self.payload.len()
    }

    pub fn encode(self, buf: &mut BytesMut) {
        encode_key(FORWARD_TAG, WireType::LengthDelimited, buf);
        buf.extend_from_slice(&self.session_id);
        buf.extend_from_slice(&self.slot.to_be_bytes());
        buf.extend_from_slice(&self.flags.to_be_bytes());
        buf.extend_from_slice(self.payload.as_ref());
    }

    pub fn decode(mut buf: BytesMut) -> Result<Self, DecodeError> {
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
        let slot = u32::from_be_bytes([slot[0], slot[1], slot[2], slot[3]]);
        let flags = buf.split_to(2);
        let flags = u16::from_be_bytes([flags[0], flags[1]]);

        Ok(Forward {
            session_id,
            slot,
            flags,
            payload: buf.into(),
        })
    }
}

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Forward( ")?;
        write!(f, "session_id: {:2x?}, ", self.session_id)?;
        write!(
            f,
            "slot: {}, flags: {:16b}, payload: ({} B) ",
            self.slot,
            self.flags,
            self.payload.len()
        )?;
        write_payload_fmt(f, &self.payload)?;
        write!(f, " )")
    }
}

fn write_payload_fmt(f: &mut std::fmt::Formatter<'_>, buf: impl AsRef<[u8]>) -> std::fmt::Result {
    let buf = buf.as_ref();
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
                // Probably we should send here packet response type matching request that we got.
                // We send at least anything, because client doesn't handle errors with None here.
                kind: Some(response::Kind::Pong(response::Pong {})),
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

impl TryFrom<Endpoint> for SocketAddr {
    type Error = anyhow::Error;

    fn try_from(endpoint: Endpoint) -> anyhow::Result<Self> {
        let ip = IpAddr::from_str(&endpoint.address)
            .map_err(|e| anyhow!("Unable to parse IP address. Error: {}", e))?;

        Ok(SocketAddr::new(ip, endpoint.port as u16))
    }
}

macro_rules! impl_convert_kind {
    ($module:ident, $ident:ident) => {
        impl From<$crate::proto::$module::$ident> for $crate::proto::$module::Kind {
            fn from(item: $crate::proto::$module::$ident) -> Self {
                $crate::proto::$module::Kind::$ident(item)
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
impl_convert_kind!(request, Slot);
impl_convert_kind!(request, Neighbours);
impl_convert_kind!(request, ReverseConnection);
impl_convert_kind!(request, Ping);

impl_convert_kind!(response, Session);
impl_convert_kind!(response, Register);
impl_convert_kind!(response, Node);
impl_convert_kind!(response, Neighbours);
impl_convert_kind!(response, ReverseConnection);
impl_convert_kind!(response, Pong);

impl_convert_kind!(control, ReverseConnection);
impl_convert_kind!(control, PauseForwarding);
impl_convert_kind!(control, ResumeForwarding);
impl_convert_kind!(control, StopForwarding);
impl_convert_kind!(control, Disconnected);
