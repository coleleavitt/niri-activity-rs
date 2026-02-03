use thiserror::Error;

#[allow(dead_code)]
#[allow(clippy::enum_variant_names)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("niri ipc: {0}")]
    NiriIpc(#[from] std::io::Error),
    #[error("niri error: {0}")]
    NiriError(String),
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("config: {0}")]
    Config(#[from] toml::de::Error),
    #[error("unexpected response from niri")]
    UnexpectedResponse,
}
