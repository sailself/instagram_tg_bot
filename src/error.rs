//! Startup / configuration error type. Per-extraction errors live in
//! [`crate::extract::ExtractError`].

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("missing required env var: {0}")]
    MissingEnv(&'static str),

    #[error("invalid value for {var}: {msg}")]
    InvalidEnv { var: &'static str, msg: String },
}
