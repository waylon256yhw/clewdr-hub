use axum::Json;
use serde_json::json;

use super::error::ApiError;
use crate::config::{CLEWDR_CONFIG, ClewdrConfig};

pub async fn api_get_config() -> Result<Json<serde_json::Value>, ApiError> {
    let mut config_json = json!(CLEWDR_CONFIG.load().as_ref());
    if let Some(obj) = config_json.as_object_mut() {
        obj.remove("cookie_array");
        obj.remove("wasted_cookie");
    }

    Ok(Json(config_json))
}

pub async fn api_post_config(
    Json(c): Json<ClewdrConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let c = c.validate();
    CLEWDR_CONFIG.rcu(|old_c| {
        let mut new_c = ClewdrConfig::clone(&c);
        new_c.cookie_array = old_c.cookie_array.to_owned();
        new_c.wasted_cookie = old_c.wasted_cookie.to_owned();
        new_c
    });
    if let Err(e) = CLEWDR_CONFIG.load().save().await {
        return Err(ApiError::internal(format!("Failed to save config: {}", e)));
    }

    Ok(Json(serde_json::json!({
        "message": "Config updated successfully",
        "config": c
    })))
}
