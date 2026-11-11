//! What a request is refused or failed with: a status and a message the
//! client shows as it is.

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
    /// For an internal error, what went wrong, which the caller is not
    /// shown: logged, and kept for whoever must act on it (a landing that is
    /// stuck says it to the operator).
    pub cause: Option<String>,
}

pub type ApiResult<T> = Result<T, HttpError>;

pub fn fail<T>(status: u16, message: impl Into<String>) -> ApiResult<T> {
    Err(HttpError::new(status, message))
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            cause: None,
        }
    }

    /// What went wrong, in full: the cause of an internal error, or the
    /// message.
    pub fn cause(&self) -> &str {
        self.cause.as_deref().unwrap_or(&self.message)
    }

    /// Anything not meant for the caller: logged in full, answered as 500.
    pub fn internal(error: impl std::fmt::Display) -> Self {
        eprintln!("newgit serve: internal error: {error}");
        Self {
            cause: Some(error.to_string()),
            ..Self::new(500, "internal error")
        }
    }
}

impl From<rusqlite::Error> for HttpError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(format!("sqlite: {error}"))
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(format!("{error:#}"))
    }
}

impl From<newgit_core::error::NewgitError> for HttpError {
    fn from(error: newgit_core::error::NewgitError) -> Self {
        Self::internal(error)
    }
}

impl From<std::io::Error> for HttpError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(format!("io: {error}"))
    }
}
