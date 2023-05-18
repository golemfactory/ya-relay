use futures::future::LocalBoxFuture;

use crate::_session_protocol::SessionProtocol;

/// Give access to private fields for testing purposes.
pub trait SessionLayerPrivate {
    fn get_protocol(&self) -> LocalBoxFuture<anyhow::Result<SessionProtocol>>;
}
