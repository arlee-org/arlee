use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("no healthy edges available")]
    NoEdges,

    #[error("no edge has capacity for the requested memory_min_mb={0}")]
    NoCapacity(u32),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("upstream edge failed: {0}")]
    BadGateway(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Self::NoEdges => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Self::NoCapacity(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::BadGateway(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            Self::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(serde_json::json!({"detail": msg}))).into_response()
    }
}
