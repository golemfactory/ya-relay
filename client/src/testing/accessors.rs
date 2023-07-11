use futures::future::LocalBoxFuture;
use std::net::SocketAddr;

use crate::_session_protocol::SessionInitializer;

/// Give access to private fields for testing purposes.
pub trait SessionLayerPrivate {
    fn get_protocol(&self) -> LocalBoxFuture<anyhow::Result<SessionInitializer>>;
    fn get_test_socket_addr(&self) -> LocalBoxFuture<anyhow::Result<SocketAddr>>;
}
