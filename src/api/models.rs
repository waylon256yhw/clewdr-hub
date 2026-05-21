use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::error::ClewdrError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelFormat {
    /// Compatibility superset (default): both Anthropic and OpenAI fields.
    Compat,
    /// Current Anthropic-style response.
    Anthropic,
    /// Strict OpenAI shape.
    OpenAI,
}

fn parse_model_format(query: &HashMap<String, String>) -> Result<ModelFormat, ClewdrError> {
    match query.get("format").map(String::as_str) {
        None | Some("") => Ok(ModelFormat::Compat),
        Some("openai") => Ok(ModelFormat::OpenAI),
        Some("anthropic") => Ok(ModelFormat::Anthropic),
        Some(_) => Err(ClewdrError::BadRequest {
            msg: "format must be one of: openai, anthropic (empty = compat superset)",
        }),
    }
}

#[derive(Serialize, sqlx::FromRow)]
struct ModelEntry {
    #[sqlx(rename = "model_id")]
    id: String,
    display_name: String,
    created_at: String,
}

impl ModelEntry {
    fn created_unix(&self) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .map(|dt| dt.timestamp())
            .unwrap_or(0)
    }
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

#[derive(Serialize)]
struct OpenAIModelEntry {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

impl From<ModelEntry> for OpenAIModelEntry {
    fn from(e: ModelEntry) -> Self {
        let created = e.created_unix();
        Self {
            id: e.id,
            object: "model",
            created,
            owned_by: "anthropic",
        }
    }
}

#[derive(Serialize)]
struct OpenAIModelList {
    object: &'static str,
    data: Vec<OpenAIModelEntry>,
}

#[derive(Serialize)]
struct CompatModelEntry {
    id: String,
    display_name: String,
    created_at: String,
    #[serde(rename = "type")]
    kind: &'static str,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

impl From<ModelEntry> for CompatModelEntry {
    fn from(e: ModelEntry) -> Self {
        let created = e.created_unix();
        Self {
            id: e.id,
            display_name: e.display_name,
            created_at: e.created_at,
            kind: "model",
            object: "model",
            created,
            owned_by: "anthropic",
        }
    }
}

#[derive(Serialize)]
struct CompatModelList {
    object: &'static str,
    data: Vec<CompatModelEntry>,
    has_more: bool,
    first_id: Option<String>,
    last_id: Option<String>,
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ClewdrError> {
    let format = parse_model_format(&query)?;
    let rows: Vec<ModelEntry> = sqlx::query_as(
        "SELECT model_id, display_name, created_at FROM models WHERE enabled = 1 ORDER BY sort_order, model_id"
    )
    .fetch_all(&db)
    .await?;

    Ok(match format {
        ModelFormat::Compat => {
            let entries: Vec<CompatModelEntry> = rows.into_iter().map(Into::into).collect();
            let first_id = entries.first().map(|e| e.id.clone());
            let last_id = entries.last().map(|e| e.id.clone());
            Json(CompatModelList {
                object: "list",
                data: entries,
                has_more: false,
                first_id,
                last_id,
            })
            .into_response()
        }
        ModelFormat::Anthropic => {
            let entries: Vec<ModelResponse> = rows.into_iter().map(Into::into).collect();
            let first_id = entries.first().map(|e| e.id.clone());
            let last_id = entries.last().map(|e| e.id.clone());
            Json(ModelsListResponse {
                data: entries,
                has_more: false,
                first_id,
                last_id,
            })
            .into_response()
        }
        ModelFormat::OpenAI => {
            let entries: Vec<OpenAIModelEntry> = rows.into_iter().map(Into::into).collect();
            Json(OpenAIModelList {
                object: "list",
                data: entries,
            })
            .into_response()
        }
    })
}

pub async fn get(
    State(db): State<SqlitePool>,
    Path(model_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ClewdrError> {
    let format = parse_model_format(&query)?;
    let entry: Option<ModelEntry> = sqlx::query_as(
        "SELECT model_id, display_name, created_at FROM models WHERE model_id = ?1 AND enabled = 1",
    )
    .bind(&model_id)
    .fetch_optional(&db)
    .await?;

    let entry = entry.ok_or(ClewdrError::NotFound {
        msg: "model not found",
    })?;

    Ok(match format {
        ModelFormat::Compat => Json(CompatModelEntry::from(entry)).into_response(),
        ModelFormat::Anthropic => Json(ModelResponse::from(entry)).into_response(),
        ModelFormat::OpenAI => Json(OpenAIModelEntry::from(entry)).into_response(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, created_at: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            display_name: id.to_string(),
            created_at: created_at.to_string(),
        }
    }

    fn q(format: Option<&str>) -> HashMap<String, String> {
        let mut q = HashMap::new();
        if let Some(f) = format {
            q.insert("format".to_string(), f.to_string());
        }
        q
    }

    #[test]
    fn parse_format_defaults_to_compat() {
        assert_eq!(parse_model_format(&q(None)).unwrap(), ModelFormat::Compat);
        assert_eq!(
            parse_model_format(&q(Some(""))).unwrap(),
            ModelFormat::Compat
        );
    }

    #[test]
    fn parse_format_openai_and_anthropic() {
        assert_eq!(
            parse_model_format(&q(Some("openai"))).unwrap(),
            ModelFormat::OpenAI
        );
        assert_eq!(
            parse_model_format(&q(Some("anthropic"))).unwrap(),
            ModelFormat::Anthropic
        );
    }

    #[test]
    fn parse_format_unknown_returns_bad_request() {
        let err = parse_model_format(&q(Some("gemini"))).unwrap_err();
        assert!(matches!(err, ClewdrError::BadRequest { .. }));
    }

    #[test]
    fn compat_entry_serializes_with_both_field_sets() {
        let e = entry("claude-sonnet-4-6", "2026-01-15T10:30:00Z");
        let json = serde_json::to_value(CompatModelEntry::from(e)).unwrap();
        assert_eq!(json["id"], "claude-sonnet-4-6");
        assert_eq!(json["type"], "model");
        assert_eq!(json["object"], "model");
        assert_eq!(json["owned_by"], "anthropic");
        assert!(json["created"].as_i64().unwrap() > 0);
        assert!(json.get("display_name").is_some());
    }

    #[test]
    fn openai_entry_has_strict_shape_only() {
        let e = entry("claude-sonnet-4-6", "2026-01-15T10:30:00Z");
        let json = serde_json::to_value(OpenAIModelEntry::from(e)).unwrap();
        assert_eq!(json["object"], "model");
        assert!(json.get("display_name").is_none());
        assert!(json.get("type").is_none());
    }

    #[test]
    fn anthropic_entry_has_no_object_or_owned_by() {
        let e = entry("claude-sonnet-4-6", "2026-01-15T10:30:00Z");
        let json = serde_json::to_value(ModelResponse::from(e)).unwrap();
        assert_eq!(json["type"], "model");
        assert!(json.get("object").is_none());
        assert!(json.get("owned_by").is_none());
    }

    #[test]
    fn created_at_unparseable_falls_back_to_zero() {
        let e = entry("x", "not-a-date");
        assert_eq!(e.created_unix(), 0);
    }
}
