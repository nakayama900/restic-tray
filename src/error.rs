use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("secrets error: {0}")]
    Secrets(#[from] crate::secrets::SecretsError),

    #[error("restic execution error: {0}")]
    Restic(String),

    #[error("restic repository lock timeout (exit code 11): {0}")]
    ResticLockTimeout(String),

    #[error("Cloudflare R2 API error: {0}")]
    R2(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

impl AppError {
    pub fn is_lock_timeout(&self) -> bool {
        matches!(self, Self::ResticLockTimeout(_))
    }
}
