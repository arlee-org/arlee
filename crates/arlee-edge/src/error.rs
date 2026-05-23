use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Self::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(serde_json::json!({"detail": msg}))).into_response()
    }
}

pub fn map_runner_err(e: anyhow::Error) -> AppError {
    let s = e.to_string();
    if s.starts_with("sandbox not found") || s.starts_with("file not found") {
        AppError::NotFound(s)
    } else {
        AppError::Other(e)
    }
}
