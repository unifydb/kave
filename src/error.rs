#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    E(String),

    #[error("io error: {0}")]
    IO(#[from] std::io::Error),

    #[error("parseint error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("uuid error: {0}")]
    UuidError(#[from] uuid::Error),

    #[error("bincode error: {0}")]
    BincodeError(#[from] bincode::Error),
}
impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error::E(s.to_string())
    }
}
impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::E(s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
