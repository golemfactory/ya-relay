pub mod client;
mod dispatch;
mod expire;
mod registry;
mod session;
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
pub use dispatch::{dispatch, Dispatcher, Handler};
pub use session::{Session, SessionDesc};
pub use ya_relay_core::session::TransportType;

// Experimental
mod _dispatch;
mod _encryption;
mod _error;
mod _routing_session;
mod _session;
mod _session_guard;
mod _session_init;
mod _session_layer;
mod _transport_layer;
mod _virtual_layer;
