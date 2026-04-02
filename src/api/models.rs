use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::error::ClewdrError;

#[derive(Serialize, sqlx::FromRow)]
struct ModelEntry {
    #[sqlx(rename = "model_id")]
    id: String,
    display_name: String,
    created_at: String,
}

#[derive(Serialize)]
pub struct ModelResponse {
    id: String,
    display_name: String,
    created_at: String,
    #[serde(rename = "type")]
    kind: &'static str,
}

impl From<ModelEntry> for ModelResponse {
    fn from(e: ModelEntry) -> Self {
        Self {
            id: e.id,
            display_name: e.display_name,
            created_at: e.created_at,
            kind: "model",
        }
    }
}

#[derive(Serialize)]
pub struct ModelsListResponse {
    data: Vec<ModelResponse>,
    has_more: bool,
    first_id: Option<String>,
    last_id: Option<String>,
}

pub async fn list(State(db): State<SqlitePool>) -> Result<Json<ModelsListResponse>, ClewdrError> {
    let rows: Vec<ModelEntry> = sqlx::query_as(
        "SELECT model_id, display_name, created_at FROM models WHERE enabled = 1 ORDER BY sort_order, model_id"
    )
    .fetch_all(&db)
    .await?;

    let models: Vec<ModelResponse> = rows.into_iter().map(Into::into).collect();
    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());

    Ok(Json(ModelsListResponse {
        data: models,
        has_more: false,
        first_id,
        last_id,
    }))
}

pub async fn get(
    State(db): State<SqlitePool>,
    Path(model_id): Path<String>,
) -> Result<Json<ModelResponse>, ClewdrError> {
    let entry: Option<ModelEntry> = sqlx::query_as(
        "SELECT model_id, display_name, created_at FROM models WHERE model_id = ?1 AND enabled = 1",
    )
    .bind(&model_id)
    .fetch_optional(&db)
    .await?;

    match entry {
        Some(e) => Ok(Json(e.into())),
        None => Err(ClewdrError::NotFound {
            msg: "model not found",
        }),
    }
}
