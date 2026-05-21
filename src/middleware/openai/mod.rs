use axum::response::IntoResponse;

mod request;

pub use request::{OpenAIChatPreprocess, OpenAIRequestError, translate_request};

use axum::Json;
use http::StatusCode;
use serde_json::json;

use crate::types::openai::OpenAIErrorBody;

impl IntoResponse for OpenAIRequestError {
    fn into_response(self) -> axum::response::Response {
        let OpenAIRequestError { status, body } = self;
        (status, Json(json!({ "error": body }))).into_response()
    }
}

impl OpenAIRequestError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: OpenAIErrorBody::invalid_request(message),
        }
    }
}
