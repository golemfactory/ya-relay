use crate::channels::ForwardSender;
use crate::client::TransportLayer;
use crate::session::session_initializer::SessionInitializer;
use crate::session::SessionLayer;
use crate::transport::tcp_registry::{TcpConnection, TcpSender};
use crate::transport::virtual_layer::TcpLayer;
use anyhow::bail;
use futures::future::LocalBoxFuture;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use ya_relay_stack::smoltcp::wire::IpEndpoint;

/// Give access to private fields for testing purposes.
pub trait SessionLayerPrivate {
    fn get_protocol(&self) -> anyhow::Result<SessionInitializer>;
    fn get_test_socket_addr(&self) -> anyhow::Result<SocketAddr>;
}

/// Give access to private fields for testing purposes.
pub trait ClientPrivate {
    fn get_transport_layer(&self) -> TransportLayer;
    fn get_session_layer(&self) -> SessionLayer;
    fn get_tcp_layer(&self) -> TcpLayer;
}

/// Give access to private fields for testing purposes.
pub trait TcpSenderPrivate {
    fn get_connection(&self) -> anyhow::Result<Arc<TcpConnection>>;
    fn get_local_addr(&self) -> anyhow::Result<IpEndpoint>;
    fn get_remote_addr(&self) -> anyhow::Result<IpEndpoint>;
}

impl TcpSenderPrivate for ForwardSender {
    fn get_connection(&self) -> anyhow::Result<Arc<TcpConnection>> {
        match self {
            ForwardSender::Reliable(sender) => match sender.connection.clone().upgrade() {
                None => bail!("Expected connection to be established"),
                Some(connection) => Ok(connection),
            },
            _ => bail!("Expected TcpSender"),
        }
    }

    fn get_local_addr(&self) -> anyhow::Result<IpEndpoint> {
        Ok(self.get_connection()?.conn.meta.local)
    }

    fn get_remote_addr(&self) -> anyhow::Result<IpEndpoint> {
        Ok(self.get_connection()?.conn.meta.remote)
    }
}
