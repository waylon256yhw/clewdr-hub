use axum::Json;
use serde_json::json;

use super::error::ApiError;
use crate::config::{CLEWDR_CONFIG, ClewdrConfig};

pub async fn api_get_config() -> Result<Json<serde_json::Value>, ApiError> {
    let config_json = json!(CLEWDR_CONFIG.load().as_ref());
    Ok(Json(config_json))
}

pub async fn api_post_config(
    Json(c): Json<ClewdrConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let c = c.validate();
    CLEWDR_CONFIG.rcu(|_old_c| ClewdrConfig::clone(&c));
    if let Err(e) = CLEWDR_CONFIG.load().save().await {
        return Err(ApiError::internal(format!("Failed to save config: {}", e)));
    }

    Ok(Json(serde_json::json!({
        "message": "Config updated successfully",
        "config": c
    })))
}
