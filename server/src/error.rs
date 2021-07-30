use crate::session::SessionId;

#[derive(thiserror::Error, Clone, Debug)]
pub enum ServerError {
    #[error("Session [{0}] not found")]
    SessionNotFound(SessionId),
    #[error("Internal Server error: {0}")]
    Internal(String),
}
