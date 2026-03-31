use wreq::StatusCode;

use crate::VERSION_INFO;

pub async fn api_version() -> String {
    VERSION_INFO.to_string()
}

pub async fn api_auth() -> StatusCode {
    // Auth is already validated by RequireAdminAuth middleware
    StatusCode::OK
}
