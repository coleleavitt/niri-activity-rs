use std::path::PathBuf;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("no home directory")]
    NoHomeDir,
    #[error("history database not readable: {0}")]
    UnreadableHistory(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;
