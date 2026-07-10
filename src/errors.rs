use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// Errors surfaced to clients before the WebSocket upgrade completes.
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("missing or invalid token")]
    Unauthorized,
    #[error("session queue is full")]
    QueueFull,
    #[error("server is shutting down")]
    ShuttingDown,
    #[error("timed out waiting for a session slot")]
    QueueTimeout,
    #[error("timed out waiting for Chromium to start")]
    ChromiumStartupTimeout,
    #[error("failed to start Chromium: {0}")]
    ChromiumUnavailable(String),
}

impl GatewayError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            GatewayError::Unauthorized => StatusCode::UNAUTHORIZED,
            GatewayError::QueueFull => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::ShuttingDown | GatewayError::ChromiumUnavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            GatewayError::QueueTimeout | GatewayError::ChromiumStartupTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = serde_json::json!({ "error": self.to_string() });
        (status, axum::Json(body)).into_response()
    }
}
