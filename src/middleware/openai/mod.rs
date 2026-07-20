use axum::response::IntoResponse;

mod request;
mod response;
mod stream;

pub use request::{OpenAIChatPreprocess, OpenAIRequestError, translate_request};
pub use response::{
    anthropic_type_to_oai, to_openai_non_stream, to_openai_non_stream_keepalive,
    translate_upstream_error_body,
};
pub use stream::to_openai_stream;

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
