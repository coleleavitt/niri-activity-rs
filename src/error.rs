use thiserror::Error;

#[allow(dead_code)]
#[allow(clippy::enum_variant_names)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("niri ipc: {0}")]
    NiriIpc(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("niri error: {0}")]
    NiriError(String),
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("config: {0}")]
    Config(#[from] toml::de::Error),
    #[error("logind: {0}")]
    Logind(String),
    #[error("unexpected response from niri")]
    UnexpectedResponse,
    #[error("failed to connect to niri IPC socket")]
    NiriConnectionFailed,
    #[error("niri event stream closed unexpectedly")]
    NiriEventStreamClosed,
    #[error("failed to connect to logind D-Bus")]
    LogindConnectionFailed,
    #[error("logind session not found for current user")]
    LogindSessionNotFound,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}
