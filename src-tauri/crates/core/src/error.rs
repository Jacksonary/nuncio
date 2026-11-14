//! Application error type shared by every Nuncio crate.
//!
//! `AppError` implements `Serialize` so it can cross the Tauri IPC boundary as
//! `{ "code": "...", "message": "..." }` (see `IpcError` in `src/ipc/api.ts`).

use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;

use crate::Result;

/// Every fallible operation in Nuncio returns this error.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Db(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Internal(String),
}

impl AppError {
    /// Stable machine-readable code exposed to the frontend (`IpcError.code`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "notFound",
            Self::Auth(_) => "auth",
            Self::Network(_) => "network",
            Self::Protocol(_) => "protocol",
            Self::InvalidInput(_) => "invalidInput",
            Self::Conflict(_) => "conflict",
            Self::Io(_) => "io",
            Self::Db(_) => "db",
            Self::Cancelled => "cancelled",
            Self::Unsupported(_) => "unsupported",
            Self::Internal(_) => "internal",
        }
    }

    /// Human readable message (the part after the code).
    pub fn message(&self) -> String {
        match self {
            Self::Cancelled => "operation cancelled".to_string(),
            other => other.to_string(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }

    pub fn db(message: impl Into<String>) -> Self {
        Self::Db(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Map an optional row lookup into a `NotFound` error.
    pub fn require<T>(value: Option<T>, what: &str) -> Result<T> {
        value.ok_or_else(|| Self::NotFound(format!("{what} not found")))
    }
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("IpcError", 2)?;
        state.serialize_field("code", self.code())?;
        state.serialize_field("message", &self.message())?;
        state.end()
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error.to_string())
    }
}

impl From<r2d2::Error> for AppError {
    fn from(error: r2d2::Error) -> Self {
        Self::Db(format!("database pool error: {error}"))
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(format!("JSON error: {error}"))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Internal(format!("background task failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_code_and_message() {
        let error = AppError::Auth("please sign in again".into());
        let value = serde_json::to_value(&error).expect("serialize");
        assert_eq!(value["code"], "auth");
        assert_eq!(value["message"], "please sign in again");
    }

    #[test]
    fn every_code_is_stable() {
        let cases = [
            (AppError::NotFound("x".into()), "notFound"),
            (AppError::Auth("x".into()), "auth"),
            (AppError::Network("x".into()), "network"),
            (AppError::Protocol("x".into()), "protocol"),
            (AppError::InvalidInput("x".into()), "invalidInput"),
            (AppError::Conflict("x".into()), "conflict"),
            (AppError::Io("x".into()), "io"),
            (AppError::Db("x".into()), "db"),
            (AppError::Cancelled, "cancelled"),
            (AppError::Unsupported("x".into()), "unsupported"),
            (AppError::Internal("x".into()), "internal"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code(), expected);
            let value = serde_json::to_value(&error).expect("serialize");
            assert_eq!(value["code"], expected);
        }
    }

    #[test]
    fn conversions_preserve_kind() {
        let io = AppError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert_eq!(io.code(), "io");
        let json = AppError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        assert_eq!(json.code(), "internal");
        let anyhow = AppError::from(anyhow::anyhow!("boom"));
        assert_eq!(anyhow.code(), "internal");
    }
}
