use axum::extract::rejection::QueryRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use geors_index::EngineError;
use serde_json::json;
use tracing::error;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        Self::bad_request(r.body_text())
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::BadRequest(_) | EngineError::CountryNotLoaded { .. } => {
                Self::bad_request(e.to_string())
            }
            EngineError::Index(inner) => {
                error!(error = %inner, "index error");
                Self::internal(format!("internal error: {inner}"))
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
