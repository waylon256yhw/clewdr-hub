use crate::VERSION_INFO;

pub async fn api_version() -> String {
    VERSION_INFO.to_string()
}
