#![allow(unused)]
#![cfg_attr(not(test), deny(unused_crate_dependencies))]
//#![deny(missing_docs)]

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

pub use client::{Client, ClientBuilder, FailFast, GenericSender, SessionError};

/// This module is a public re-export cryptographic abstractions.
pub use ya_relay_core::crypto;

#[cfg(any(test, feature = "test-utils"))]
#[allow(missing_docs)]
pub mod testing;

/// Provides a unified interface to the various model types used in `ya_relay`.
pub mod model {
    #[doc(inline)]
    pub use ya_relay_core::NodeId;
    #[doc(inline)]
    pub use ya_relay_proto::proto::response::Node;

    #[doc(inline)]
    pub use ya_relay_stack::{SocketDesc, SocketState};

    #[doc(inline)]
    pub use ya_relay_core::server_session::TransportType;

    #[doc(inline)]
    pub use ya_relay_core::session::Session;

    pub use crate::raw_session::SessionDesc;

    pub use ya_relay_core::server_session::SessionId;

    #[doc(inline)]
    pub use ya_relay_proto::proto::Payload;
}

/// Re-exports several channel related items from the client module and proto.
pub mod channels {
    #[doc(inline)]
    pub use crate::client::{ForwardReceiver, ForwardSender, Forwarded};

    #[doc(inline)]
    pub use ya_relay_proto::codec::forward::PrefixedStream;
}

#[cfg(feature="test-utils")]
pub mod test_utils {
    pub use crate::config::ClientConfig;
    pub use crate::session::session_initializer::SessionInitializer;
    pub use crate::session::SessionLayer;
    pub use crate::session::network_view::{NetworkView, SessionLock, SessionPermit};
    pub use crate::transport::tcp_registry::VirtNode;
}
