use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Error::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Error::Sqlx(sqlx::Error::RowNotFound) => {
                (StatusCode::NOT_FOUND, "not found".to_string())
            }
            _ => {
                tracing::error!(error = ?self, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, msg).into_response()
    }
}
