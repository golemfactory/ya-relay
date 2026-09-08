use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use ya_relay_core::crypto::PublicKey;

use ya_relay_core::identity::Identity;
use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;

use super::network_view::NodeSnapshot;
use crate::client::SessionError;
use crate::direct_session::DirectSession;
use crate::routing_session::NodeRouting;

pub(crate) struct SessionRegistrationInfo {
    pub addr: SocketAddr,
    pub id: SessionId,
    pub node_id: NodeId,
    pub identities: Vec<Identity>,
    pub supported_encryptions: Vec<String>,
    pub session_key: Option<PublicKey>,
}

/// Trait for decoupling `SessionProtocol` from `SessionLayer`.
/// Thanks to this we can test `SessionProtocol` independently.
#[async_trait(?Send)]
pub(crate) trait SessionRegistration: 'static {
    async fn register_session(
        &self,
        info: SessionRegistrationInfo,
        owner: &NodeSnapshot,
    ) -> anyhow::Result<Arc<DirectSession>>;

    async fn register_routing(&self, routing: Arc<NodeRouting>) -> anyhow::Result<()>;
}

/// Trait for decoupling `SessionPermit` from `SessionLayer`.
#[async_trait(?Send)]
pub trait SessionDeregistration: 'static {
    /// Unregisters all Node's data. Function can't fail, it has to handle errors internally.
    async fn unregister(&self, node_id: NodeId);
    async fn unregister_generation(&self, node: NodeSnapshot) {
        if node.is_current() {
            self.unregister(node.id).await;
        }
    }
    async fn unregister_session(&self, session: Arc<DirectSession>);
    async fn abort_initializations(&self, remote: SocketAddr) -> Result<(), SessionError>;
}
