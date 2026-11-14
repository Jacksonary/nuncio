//! Error helpers.
//!
//! The crate uses [`nuncio_core::AppError`] everywhere; these thin constructors
//! keep provider code readable.

pub use nuncio_core::{AppError, Result};

pub fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_input(message)
}

pub fn protocol(message: impl Into<String>) -> AppError {
    AppError::protocol(message)
}

pub fn network(message: impl Into<String>) -> AppError {
    AppError::network(message)
}

pub fn internal(message: impl Into<String>) -> AppError {
    AppError::internal(message)
}

pub fn unsupported(message: impl Into<String>) -> AppError {
    AppError::unsupported(message)
}
