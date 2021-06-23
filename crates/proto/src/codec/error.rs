#[derive(thiserror::Error, Clone, Debug)]
pub enum ParseError {
    #[error("Length prefix too short")]
    PrefixTooShort,
    #[error("Length prefix too long")]
    PrefixTooLong,
    #[error("Invalid length prefix")]
    PrefixInvalid,
    #[error("Payload too short ({left}b remaining)")]
    PayloadTooShort { total: usize, left: usize },
    #[error("Payload too long ({left}b over limit)")]
    PayloadTooLong { total: usize, left: usize },
    #[error("Invalid payload")]
    PayloadInvalid { total: usize, left: usize },
    #[error("Forward message")]
    Forward { bytes: bytes::BytesMut, left: usize },
    #[error("Invalid format")]
    InvalidFormat,
    #[error("Invalid size: {0}")]
    InvalidSize(usize),
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
