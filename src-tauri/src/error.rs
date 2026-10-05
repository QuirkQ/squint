use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("GitHub request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error::Message(message.into())
    }
}

// Commands reject with this, so the UI receives a plain message string.
impl Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
