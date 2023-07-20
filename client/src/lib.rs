#![allow(unused)]
mod client;
mod config;
mod direct_session;
mod dispatch;
mod encryption;
mod error;
pub mod metrics;
mod raw_session;
mod routing_session;
mod session;
mod transport;

pub use client::{Client, ClientBuilder, FailFast, GenericSender};

pub use ya_relay_core::crypto;

#[cfg(any(test, feature = "mock"))]
pub mod testing;

pub mod model {
    #[doc(inline)]
    pub use ya_relay_core::NodeId;
    #[doc(inline)]
    pub use ya_relay_proto::proto::response::Node;

    #[doc(inline)]
    pub use ya_relay_core::server_session::TransportType;

    #[doc(inline)]
    pub use ya_relay_core::session::{Session, SessionDesc};

    #[doc(inline)]
    pub use ya_relay_proto::proto::Payload;
}

pub mod channels {
    #[doc(inline)]
    pub use crate::client::{ForwardReceiver, ForwardSender, Forwarded};
}
