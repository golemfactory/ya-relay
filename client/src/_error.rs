use anyhow::Error;
use std::net::SocketAddr;

use ya_relay_core::server_session::SessionId;
use ya_relay_core::NodeId;

use crate::_session_state::SessionState;
use crate::_tcp_registry::TcpState;

pub type SessionResult<T> = Result<T, SessionError>;

/// TODO: Organize this error better. We should be able to make decision
///       if we can recover from error or we should give up. I see at least
///       2 contexts for this:
///       - p2p session initialization has to decide if it should continue
///         testing other public addresses.
///       - Library user should decide if he wants to try send message again    
#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum SessionError {
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("Protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("Bad Response error: {0}")]
    BadResponse(String),
    #[error("Bad Request: {0}")]
    BadRequest(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Aborted: {0}")]
    Aborted(String),
    #[error("Not Found: {0}")]
    NotFound(String),
    #[error("Relay error: {0}")]
    Relay(String),
    #[error("Unexpected error: {0}")]
    Unexpected(String),
    #[error("Programming error: {0}")]
    ProgrammingError(String),
    #[error("{0}")]
    /// This is not an error itself, but prevents from completing operation.
    NotApplicable(String),
    #[error("{0}")]
    Generic(String),
}

/// Error indicates that other Node failed to stick to protocol.
#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum ProtocolError {
    #[error("Protocol invalid response: {0}")]
    InvalidResponse(String),
    #[error("Invalid challenge: {0}")]
    InvalidChallenge(String),
    #[error("Failed to recover NodeId from request: {0}")]
    RecoverId(String),
    /// In this case it can be either other party fault (malicious behavior)
    /// or we closed Session but other party still thinks it exists.
    #[error("Unknown SessionId: {0}")]
    SessionNotFound(SessionId),
    #[error("Request with invalid SessionId {0:x?}. Error: {1}")]
    InvalidSessionId(Vec<u8>, String),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum TransitionError {
    #[error("State transition not allowed from: {0} to {1}")]
    InvalidTransition(SessionState, SessionState),
    #[error("Can't change state: Node {0} not found")]
    NodeNotFound(NodeId),
}

/// TODO: Do we really need this error. In most places it is converted back to `SessionError`.
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

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum EncryptionError {
    #[error("{0}")]
    Generic(String),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum TcpError {
    #[error("{0}")]
    Generic(String),
    #[error("Programming error: {0}")]
    ProgrammingError(String),
}

#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum TcpTransitionError {
    #[error("State transition not allowed from: {0} to {1}")]
    InvalidTransition(TcpState, TcpState),
    #[error("Channel not found")]
    ProgrammingError,
}

impl From<TransitionError> for SessionError {
    fn from(value: TransitionError) -> Self {
        SessionError::Internal(value.to_string())
    }
}

impl From<RequestError> for SessionError {
    fn from(value: RequestError) -> Self {
        SessionError::Network(value.to_string())
    }
}

impl From<SessionInitError> for SessionError {
    fn from(err: SessionInitError) -> Self {
        match err {
            SessionInitError::P2P(_, e) | SessionInitError::Relay(_, e) => e,
        }
    }
}

impl From<anyhow::Error> for RequestError {
    fn from(value: Error) -> Self {
        RequestError::Generic(value.to_string())
    }
}

pub trait ResultExt<T, E> {
    fn on_err<O: FnOnce(&E)>(self, op: O) -> Result<T, E>;
}

impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn on_err<O: FnOnce(&E)>(self, op: O) -> Result<T, E> {
        match self {
            Ok(t) => Ok(t),
            Err(e) => {
                op(&e);
                Err(e)
            }
        }
    }
}
