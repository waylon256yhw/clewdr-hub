use axum::{Json, extract::{Path, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::{Paginated, PaginationParams};
use crate::error::ClewdrError;

#[derive(Serialize, sqlx::FromRow)]
pub struct ModelRow {
    pub model_id: String,
    pub display_name: String,
    pub enabled: i32,
    pub source: String,
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreateModelRequest {
    pub model_id: String,
    pub display_name: String,
    pub sort_order: Option<i32>,
}

#[derive(Deserialize)]
pub struct UpdateModelRequest {
    pub display_name: Option<String>,
    pub enabled: Option<bool>,
    pub sort_order: Option<i32>,
}

const MODEL_SELECT: &str = "SELECT model_id, display_name, enabled, source, sort_order, created_at, updated_at FROM models";

pub async fn list(
    State(db): State<SqlitePool>,
    axum::extract::Query(params): axum::extract::Query<PaginationParams>,
) -> Result<Json<Paginated<ModelRow>>, ClewdrError> {
    let (offset, limit) = params.resolve();
    let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM models")
        .fetch_one(&db)
        .await?;
    let items: Vec<ModelRow> = sqlx::query_as(&format!(
        "{MODEL_SELECT} ORDER BY sort_order, model_id LIMIT ?1 OFFSET ?2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(&db)
    .await?;
    Ok(Json(Paginated { items, total: total.0, offset, limit }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreateModelRequest>,
) -> Result<(StatusCode, Json<ModelRow>), ClewdrError> {
    let model_id = req.model_id.trim().to_string();
    if model_id.is_empty() {
        return Err(ClewdrError::BadRequest { msg: "model_id is required" });
    }
    let display_name = req.display_name.trim().to_string();
    if display_name.is_empty() {
        return Err(ClewdrError::BadRequest { msg: "display_name is required" });
    }

    let sort_order = req.sort_order.unwrap_or(0);
    let result = sqlx::query(
        "INSERT INTO models (model_id, display_name, source, sort_order) VALUES (?1, ?2, 'admin', ?3)"
    )
    .bind(&model_id)
    .bind(&display_name)
    .bind(sort_order)
    .execute(&db)
    .await;

    match result {
        Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
            return Err(ClewdrError::Conflict { msg: "model_id already exists" });
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }

    let row: ModelRow = sqlx::query_as(&format!("{MODEL_SELECT} WHERE model_id = ?1"))
        .bind(&model_id)
        .fetch_one(&db)
        .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn update(
    State(db): State<SqlitePool>,
    Path(model_id): Path<String>,
    Json(req): Json<UpdateModelRequest>,
) -> Result<Json<ModelRow>, ClewdrError> {
    let mut tx = db.begin().await?;

    let existing: Option<(String,)> = sqlx::query_as("SELECT model_id FROM models WHERE model_id = ?1")
        .bind(&model_id)
        .fetch_optional(&mut *tx)
        .await?;
    if existing.is_none() {
        return Err(ClewdrError::NotFound { msg: "model not found" });
    }

    if let Some(ref name) = req.display_name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(ClewdrError::BadRequest { msg: "display_name cannot be empty" });
        }
        sqlx::query("UPDATE models SET display_name = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE model_id = ?2")
            .bind(trimmed)
            .bind(&model_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(enabled) = req.enabled {
        sqlx::query("UPDATE models SET enabled = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE model_id = ?2")
            .bind(i32::from(enabled))
            .bind(&model_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(order) = req.sort_order {
        sqlx::query("UPDATE models SET sort_order = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE model_id = ?2")
            .bind(order)
            .bind(&model_id)
            .execute(&mut *tx)
            .await?;
    }

    let row: ModelRow = sqlx::query_as(&format!("{MODEL_SELECT} WHERE model_id = ?1"))
        .bind(&model_id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Json(row))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    Path(model_id): Path<String>,
) -> Result<StatusCode, ClewdrError> {
    let result = sqlx::query("DELETE FROM models WHERE model_id = ?1")
        .bind(&model_id)
        .execute(&db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound { msg: "model not found" });
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reset_defaults(
    State(db): State<SqlitePool>,
) -> Result<Json<Paginated<ModelRow>>, ClewdrError> {
    crate::db::reset_default_models(&db).await?;
    let items: Vec<ModelRow> = sqlx::query_as(&format!("{MODEL_SELECT} ORDER BY sort_order, model_id"))
        .fetch_all(&db)
        .await?;
    let total = items.len() as i64;
    Ok(Json(Paginated { items, total, offset: 0, limit: 100 }))
}
