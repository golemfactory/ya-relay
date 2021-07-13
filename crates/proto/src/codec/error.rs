#[derive(thiserror::Error, Clone, Debug)]
pub enum EncodeError {
    #[error("Packet too long: {size} b")]
    PacketTooLong { size: usize },
    #[error("Unsupported packet kind")]
    PacketNotSupported,
    #[error("Empty packet")]
    NoData,
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum DecodeError {
    #[error("Invalid packet format")]
    PacketFormatInvalid,
    #[error("Packet too short")]
    PacketTooShort,
    #[error("Packet length prefix is too short")]
    PrefixTooShort,
    #[error("Packet length prefix is too long")]
    PrefixTooLong,
    #[error("Packet length prefix is invalid")]
    PrefixFormatInvalid,
    #[error("Payload too short ({left} b remaining)")]
    PayloadTooShort { left: usize },
    #[error("Payload too long ({left} b over limit)")]
    PayloadTooLong { left: usize },
    #[error("Invalid payload")]
    PayloadInvalid { left: usize },
    #[error("Forward packet")]
    Forward { left: usize, bytes: bytes::BytesMut },
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    ProtobufEncode(#[from] prost::EncodeError),
    #[error(transparent)]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Channel closed")]
    Send(#[from] futures::channel::mpsc::SendError),
}
