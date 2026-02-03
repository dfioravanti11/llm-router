//! Errors the proxy path can return to a client.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("request body exceeds the {limit} byte limit")]
    BodyTooLarge { limit: usize },

    #[error("could not read request body: {0}")]
    RequestBody(String),

    #[error("could not reach worker `{worker}`: {source}")]
    Upstream {
        worker: String,
        #[source]
        source: reqwest::Error,
    },
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            ProxyError::BodyTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            ProxyError::RequestBody(_) => StatusCode::BAD_REQUEST,
            ProxyError::Upstream { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            ProxyError::BodyTooLarge { .. } | ProxyError::RequestBody(_) => "invalid_request_error",
            ProxyError::Upstream { .. } => "upstream_error",
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = self.status();
        let kind = self.kind();
        let message = self.to_string();

        if status.is_server_error() {
            tracing::error!(error = %message, "request failed");
        } else {
            tracing::debug!(error = %message, "request rejected");
        }

        let body = json!({
            "error": {
                "message": message,
                "type": kind,
            }
        });
        (status, Json(body)).into_response()
    }
}
