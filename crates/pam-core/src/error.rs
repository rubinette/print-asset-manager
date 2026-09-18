use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("xml: {0}")]
    Xml(String),
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("empty mesh: {0}")]
    EmptyMesh(PathBuf),
    #[error("parse error in {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, Error>;
