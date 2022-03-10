pub mod connection;
pub mod device;
mod error;
pub mod interface;
mod network;
pub mod packet;
mod patch_smoltcp;
mod port;
mod protocol;
mod queue;
pub mod socket;
mod stack;

pub use connection::{Connect, Connection, DisconnectReason, Send};
pub use device::CaptureDevice;
pub use error::Error;
pub use network::{Channel, EgressEvent, IngressEvent, Network, NetworkConfig};
pub use port::Allocator as PortAllocator;
pub use protocol::Protocol;
pub use stack::Stack;

pub use smoltcp;

pub type Result<T> = std::result::Result<T, Error>;
