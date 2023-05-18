use anyhow::Error;
use std::net::SocketAddr;
use ya_relay_core::NodeId;

use crate::_session_guard::SessionState;

pub type SessionResult<T> = Result<T, SessionError>;

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum SessionError {
    #[error("Internal Error: {0}")]
    Internal(String),
    #[error("Protocol Error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("Bad Response Error: {0}")]
    BadResponse(String),
    #[error("Network Error: {0}")]
    Network(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Not Found: {0}")]
    NotFound(String),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum ProtocolError {
    #[error("Protocol Error: {0}")]
    InvalidResponse(String),
    #[error("Failed to send response: {0}")]
    SendFailure(#[from] RequestError),
    #[error("Invalid challenge: {0}")]
    InvalidChallenge(String),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum TransitionError {
    #[error("State transition not allowed from: {0} to {1}")]
    InvalidTransition(SessionState, SessionState),
    #[error("Can't change state: Node {0} not found")]
    NodeNotFound(NodeId),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum SessionInitError {
    #[error("Failed to init p2p session with {0}: {1}")]
    P2P(NodeId, SessionError),
    #[error("Failed to init relay server ({0}) session: {1}")]
    Relay(SocketAddr, SessionError),
}

/// Low level error used when sending protool packets.
/// TODO: For now I'm using `Generic` and auto converting from anyhow,
///       but we should do better job with distinguishing errors.
#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum RequestError {
    #[error("Request failed: {0}")]
    Generic(String),
}

impl From<TransitionError> for SessionError {
    fn from(value: TransitionError) -> Self {
        SessionError::Internal(value.to_string())
    }
}

impl From<anyhow::Error> for RequestError {
    fn from(value: Error) -> Self {
        RequestError::Generic(value.to_string())
    }
}
