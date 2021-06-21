use prost::Message;

// pub mod datagram;
// pub mod stream;

pub const MAX_REQUEST_MESSAGE_SIZE: usize = 600; // bytes

#[derive(thiserror::Error, Clone, Debug)]
pub enum Error {
    #[error(transparent)]
    Encode(#[from] prost::EncodeError),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
}

pub enum Output<M: Message> {
    Packet(M),
    Forward {
        session_id: Box<[u8]>,
        slot: u32,
        chunk: Box<[u8]>,
    },
    ForwardChunk(Box<[u8]>),
}
