use blake3::Hash;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] turso::Error),

    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: Hash, actual: Hash },

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("UTF-8 error in stored key: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
