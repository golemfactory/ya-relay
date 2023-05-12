#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum SessionError {
    #[error("Internal Error: {0}")]
    Internal(String),
    #[error("Protocol Error: {0}")]
    Protocol(String),
    #[error("Bad Requestor Error: {0}")]
    BadResponse(String),
    #[error("Network Error: {0}")]
    Network(String),
    #[error("Timeout Error: {0}")]
    Timeout(String),
}
