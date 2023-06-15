use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;

use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;

use crate::_client::SessionError;
use crate::_direct_session::DirectSession;
use crate::_raw_session::RawSession;
use crate::_routing_session::NodeRouting;

/// Trait for decoupling `SessionProtocol` from `SessionLayer`.
/// Thanks to this we can test `SessionProtocol` independently.
#[async_trait(?Send)]
pub(crate) trait SessionRegistration: 'static {
    async fn register_session(
        &self,
        addr: SocketAddr,
        id: SessionId,
        node_id: NodeId,
        identities: Vec<Identity>,
    ) -> anyhow::Result<Arc<DirectSession>>;

    async fn register_routing(&self, routing: Arc<NodeRouting>) -> anyhow::Result<()>;
}

/// Trait for decoupling `SessionPermit` from `SessionLayer`.
#[async_trait(?Send)]
pub trait SessionDeregistration: 'static {
    async fn disconnect(&self, node_id: NodeId) -> Result<(), SessionError>;
    async fn close_session(&self, session: Arc<DirectSession>) -> Result<(), SessionError>;
    async fn abort_initializations(&self, session: Arc<RawSession>) -> Result<(), SessionError>;
}
