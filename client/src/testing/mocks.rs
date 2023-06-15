use crate::_client::SessionError;
use crate::_direct_session::DirectSession;
use crate::_raw_session::RawSession;
use crate::_session_traits::SessionDeregistration;

use async_trait::async_trait;
use std::sync::Arc;

use ya_relay_core::NodeId;

pub struct NoOpSessionLayer;

#[async_trait(?Send)]
impl SessionDeregistration for NoOpSessionLayer {
    async fn disconnect(&self, _node_id: NodeId) -> Result<(), SessionError> {
        Ok(())
    }

    async fn close_session(&self, _session: Arc<DirectSession>) -> Result<(), SessionError> {
        Ok(())
    }

    async fn abort_initializations(&self, _session: Arc<RawSession>) -> Result<(), SessionError> {
        Ok(())
    }
}
