#[derive(thiserror::Error, Clone, Debug)]
pub enum ParseError {
    #[error("Length prefix too short")]
    PrefixTooShort,
    #[error("Length prefix too long")]
    PrefixTooLong,
    #[error("Invalid length prefix")]
    PrefixInvalid,
    #[error("Payload too short ({left} b remaining)")]
    PayloadTooShort { left: usize },
    #[error("Payload too long ({left} b over limit)")]
    PayloadTooLong { left: usize },
    #[error("Invalid payload")]
    PayloadInvalid { left: usize },
    #[error("Forward message")]
    Forward { left: usize, bytes: bytes::BytesMut },
    #[error("Invalid size: {size} b")]
    InvalidSize { size: usize },
    #[error("Invalid format")]
    InvalidFormat,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Encode(#[from] prost::EncodeError),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Channel closed")]
    Send(#[from] futures::channel::mpsc::SendError),
}
