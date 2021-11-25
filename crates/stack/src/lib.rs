pub mod connection;
pub mod device;
mod error;
pub mod interface;
mod network;
pub mod packet;
mod port;
mod protocol;
mod queue;
pub mod socket;
mod stack;

pub use connection::{Connect, Connection, DisconnectReason, Send};
pub use device::CaptureDevice;
pub use error::Error;
pub use network::{Channel, EgressEvent, IngressEvent, Network};
pub use port::Allocator as PortAllocator;
pub use protocol::Protocol;
pub use stack::Stack;

pub use smoltcp;

/// Maximum size of Ethernet II frame + payload
pub const MAX_FRAME_SIZE: usize = 14 + MAX_PACKET_SIZE;
pub const MAX_PACKET_SIZE: usize = 65521;

pub type Result<T> = std::result::Result<T, Error>;
