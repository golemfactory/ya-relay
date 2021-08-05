use crate::session::SessionId;

use ya_client_model::NodeId;

pub type ServerResult<T> = Result<T, Error>;

#[derive(thiserror::Error, Clone, Debug)]
pub enum Error {
    #[error("Undefined error: {0}")]
    Undefined(#[from] Undefined),
    #[error("BadRequest: {0}")]
    BadRequest(#[from] BadRequest),
    #[error("Unauthorized access: {0}")]
    Unauthorized(#[from] Unauthorized),
    #[error("NotFound: {0}")]
    NotFound(#[from] NotFound),
    #[error("Timeout: {0}")]
    Timeout(#[from] Timeout),
    #[error("Conflict: {0}")]
    Conflict(#[from] Conflict),
    #[error("PayloadTooLarge: {0}")]
    PayloadTooLarge(#[from] PayloadTooLarge),
    #[error("TooManyRequests: {0}")]
    TooManyRequests(#[from] TooManyRequests),
    #[error("Internal Server error: {0}")]
    Internal(#[from] InternalError),
    #[error("GatewayTimeout: {0}")]
    GatewayTimeout(#[from] GatewayTimeout),
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum Undefined {}

#[derive(thiserror::Error, Clone, Debug)]
pub enum BadRequest {
    #[error("Session [{0}] not found.")]
    SessionNotFound(SessionId),
    #[error("No SessionId.")]
    NoSessionId,
    #[error("Invalid NodeId.")]
    InvalidNodeId,
    #[error("Invalid packet type for session [{0}]. Expected: ")]
    InvalidPacket(SessionId, String),
    #[error("Failed to decode packet.")]
    DecodingFailed,
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum Unauthorized {
    #[error("Session [{0}] not found.")]
    SessionNotFound(SessionId),
    #[error("Invalid session id: {0:x?}.")]
    InvalidSessionId(Vec<u8>),
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum NotFound {
    #[error("Node [{0}] not registered.")]
    Node(NodeId),
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum Timeout {
    #[error("Waiting for ping response timed out.")]
    Ping,
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum Conflict {}

#[derive(thiserror::Error, Clone, Debug)]
pub enum PayloadTooLarge {}

#[derive(thiserror::Error, Clone, Debug)]
pub enum TooManyRequests {}

#[derive(thiserror::Error, Clone, Debug)]
pub enum InternalError {
    #[error("Failed to send response.")]
    Send,
    #[error("Failed to receive response.")]
    Receiving,
    #[error("Failed to encode packet.")]
    Encoding,
    #[error("Failed to decode packet.")]
    Decoding,
    #[error("Binding socket failed. {0}")]
    BindingSocket(String),
    #[error("Node info for session [{0}] not found.")]
    GettingSessionInfo(SessionId),
}

#[derive(thiserror::Error, Clone, Debug)]
pub enum GatewayTimeout {}
