use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::common::{Paginated, PaginationParams};
use crate::error::ClewdrError;

#[derive(Serialize, sqlx::FromRow)]
pub struct PolicyResponse {
    pub id: i64,
    pub name: String,
    pub max_concurrent: i64,
    pub rpm_limit: i64,
    pub weekly_budget_nanousd: i64,
    pub monthly_budget_nanousd: i64,
    pub assigned_user_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct CreatePolicyRequest {
    pub name: String,
    pub max_concurrent: i64,
    pub rpm_limit: i64,
    pub weekly_budget_nanousd: i64,
    pub monthly_budget_nanousd: i64,
}

#[derive(Deserialize)]
pub struct UpdatePolicyRequest {
    pub name: Option<String>,
    pub max_concurrent: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub weekly_budget_nanousd: Option<i64>,
    pub monthly_budget_nanousd: Option<i64>,
}

const POLICY_SELECT: &str = r#"
    SELECT p.id, p.name, p.max_concurrent, p.rpm_limit,
           p.weekly_budget_nanousd, p.monthly_budget_nanousd,
           (SELECT COUNT(*) FROM users WHERE policy_id = p.id) as assigned_user_count,
           p.created_at, p.updated_at
    FROM policies p
"#;

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Paginated<PolicyResponse>>, ClewdrError> {
    let (offset, limit) = params.resolve();

    let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM policies")
        .fetch_one(&db)
        .await?;

    let query = format!("{POLICY_SELECT} ORDER BY p.id LIMIT ?1 OFFSET ?2");
    let items: Vec<PolicyResponse> = sqlx::query_as(&query)
        .bind(limit)
        .bind(offset)
        .fetch_all(&db)
        .await?;

    Ok(Json(Paginated { items, total: total.0, offset, limit }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<PolicyResponse>), ClewdrError> {
    if req.max_concurrent <= 0 || req.rpm_limit <= 0 {
        return Err(ClewdrError::BadRequest { msg: "max_concurrent and rpm_limit must be positive" });
    }

    let id = sqlx::query(
        "INSERT INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&req.name)
    .bind(req.max_concurrent)
    .bind(req.rpm_limit)
    .bind(req.weekly_budget_nanousd)
    .bind(req.monthly_budget_nanousd)
    .execute(&db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e {
            if de.message().contains("UNIQUE") {
                return ClewdrError::Conflict { msg: "policy name already exists" };
            }
        }
        ClewdrError::from(e)
    })?
    .last_insert_rowid();

    let query = format!("{POLICY_SELECT} WHERE p.id = ?1");
    let row: PolicyResponse = sqlx::query_as(&query)
        .bind(id)
        .fetch_one(&db)
        .await?;

    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn update(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
    Json(req): Json<UpdatePolicyRequest>,
) -> Result<Json<PolicyResponse>, ClewdrError> {
    // Validate all fields first before any writes
    if let Some(v) = req.max_concurrent {
        if v <= 0 { return Err(ClewdrError::BadRequest { msg: "max_concurrent must be positive" }); }
    }
    if let Some(v) = req.rpm_limit {
        if v <= 0 { return Err(ClewdrError::BadRequest { msg: "rpm_limit must be positive" }); }
    }

    let mut tx = db.begin().await?;

    let existing: Option<(i64,)> = sqlx::query_as("SELECT id FROM policies WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if existing.is_none() {
        return Err(ClewdrError::NotFound { msg: "policy not found" });
    }

    if let Some(ref name) = req.name {
        sqlx::query("UPDATE policies SET name = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(name).bind(id).execute(&mut *tx).await
            .map_err(|e| {
                if let sqlx::Error::Database(ref de) = e {
                    if de.message().contains("UNIQUE") {
                        return ClewdrError::Conflict { msg: "policy name already exists" };
                    }
                }
                ClewdrError::from(e)
            })?;
    }
    if let Some(v) = req.max_concurrent {
        sqlx::query("UPDATE policies SET max_concurrent = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(v).bind(id).execute(&mut *tx).await?;
    }
    if let Some(v) = req.rpm_limit {
        sqlx::query("UPDATE policies SET rpm_limit = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(v).bind(id).execute(&mut *tx).await?;
    }
    if let Some(v) = req.weekly_budget_nanousd {
        sqlx::query("UPDATE policies SET weekly_budget_nanousd = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(v).bind(id).execute(&mut *tx).await?;
    }
    if let Some(v) = req.monthly_budget_nanousd {
        sqlx::query("UPDATE policies SET monthly_budget_nanousd = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2")
            .bind(v).bind(id).execute(&mut *tx).await?;
    }

    tx.commit().await?;

    let query = format!("{POLICY_SELECT} WHERE p.id = ?1");
    let row: PolicyResponse = sqlx::query_as(&query)
        .bind(id)
        .fetch_one(&db)
        .await?;

    Ok(Json(row))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    if id == 1 {
        return Err(ClewdrError::Conflict { msg: "cannot delete the default policy" });
    }

    let mut tx = db.begin().await?;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE policy_id = ?1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if count.0 > 0 {
        return Err(ClewdrError::Conflict { msg: "policy is still assigned to users" });
    }

    let result = sqlx::query("DELETE FROM policies WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ClewdrError::NotFound { msg: "policy not found" });
    }

    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}
