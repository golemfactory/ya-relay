#[derive(thiserror::Error, Clone, Debug, PartialEq)]
pub enum SessionError {
    Internal(String),
    Protocol(String),
    BadResponse(String),
    Network(String),
    Timeout(String),
}
