use crate::_client::SessionError;
use crate::_direct_session::DirectSession;
use crate::_session_traits::SessionDeregistration;

use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;

use ya_relay_core::NodeId;

pub struct NoOpSessionLayer;

#[async_trait(?Send)]
impl SessionDeregistration for NoOpSessionLayer {
    async fn unregister(&self, _node_id: NodeId) {}
    async fn unregister_session(&self, _session: Arc<DirectSession>) {}
    async fn abort_initializations(&self, _remote: SocketAddr) -> Result<(), SessionError> {
        Ok(())
    }
}
