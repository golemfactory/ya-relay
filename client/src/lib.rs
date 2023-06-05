pub mod client;
mod expire;
mod registry;
mod session_guard;
mod session_manager;
mod session_start;
pub mod testing;
mod virtual_layer;

pub use client::{Client, ClientBuilder, ForwardReceiver};
pub use ya_relay_core::crypto;

pub use ya_relay_proto::*;
pub use ya_relay_stack::*;

// TODO: Exposed for ya-relay-server. Should be made private after we merge implementations.
pub use ya_relay_core::dispatch::{dispatch, Dispatcher, Handler};
pub use ya_relay_core::server_session::TransportType;
pub use ya_relay_core::session::{Session, SessionDesc};

// Experimental
pub mod _client;
mod _config;
mod _direct_session;
mod _dispatch;
mod _encryption;
mod _error;
mod _expire;
mod _metrics;
mod _routing_session;
mod _session;
mod _session_layer;
mod _session_protocol;
mod _session_registry;
mod _tcp_registry;
mod _transport_layer;
mod _virtual_layer;
