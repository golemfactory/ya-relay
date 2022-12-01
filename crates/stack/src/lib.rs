pub mod connection;
pub mod device;
mod error;
pub mod interface;
mod metrics;
mod network;
pub mod packet;
mod patch_smoltcp;
mod port;
mod protocol;
pub mod socket;
mod stack;

pub use connection::{Connect, Connection, DisconnectReason, Send};
pub use device::CaptureDevice;
pub use error::Error;
pub use metrics::{Average, ChannelMetrics, Ewma, Metrics, TimeWindow};
pub use network::{
    Channel, EgressEvent, EgressReceiver, IngressEvent, IngressReceiver, Network, StackConfig,
};
pub use port::Allocator as PortAllocator;
pub use protocol::Protocol;
pub use ya_smoltcp;
pub use socket::{SocketDesc, SocketState};
pub use stack::Stack;

pub type Result<T> = std::result::Result<T, Error>;
