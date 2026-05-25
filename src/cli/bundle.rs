//! Shared bundle types for `export-config` / `import-config`.
//!
//! A bundle is the on-disk form of a clewdr config snapshot — TOML +
//! selected DB tables — produced by `clewdr export-config` and consumed by
//! `clewdr import-config`. The on-wire format is JSON (commit #6 and #7);
//! AES-GCM encryption wraps that JSON in commit #8.
//!
//! Schema is captured *dynamically* via `PRAGMA table_info(...)` rather
//! than hand-coded, so a migration that adds a new column doesn't silently
//! drop data on round-trip. BLOB cells are encoded as `{"type":"blob",
//! "base64":"..."}` to disambiguate from TEXT.

use std::collections::BTreeMap;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::{Column, Row as SqlxRow, SqliteConnection, TypeInfo, ValueRef};

use crate::error::ClewdrError;

/// Bundle format version. Bump on incompatible schema changes.
pub const BUNDLE_VERSION: u32 = 1;

/// Tables we always export by default — the "config-like" set the operator
/// would expect to round-trip across machines or restore from a backup.
pub const DEFAULT_TABLES: &[&str] = &[
    "policies",
    "users",
    "api_keys",
    "api_key_account_bindings",
    "accounts",
    "proxies",
    "settings",
    "models",
    "model_pricing",
];

/// Tables only included with `--include-runtime`. They round-trip cleanly
/// but they're large and tied to the source machine's state, so the
/// default backup excludes them.
pub const RUNTIME_TABLES: &[&str] = &[
    "account_runtime_state",
    "usage_rollups",
    "usage_lifetime_totals",
];

/// Tables we never export, regardless of flags:
/// - `request_logs` is append-only log data, large and not portable
/// - `_sqlx_migrations` is rebuilt on import via the migration step
pub const NEVER_EXPORTED: &[&str] = &["request_logs", "_sqlx_migrations"];

/// `settings` rows whose `key` matches this list are never exported, even
/// without `--no-secrets`. `session_secret` is the HMAC signing key for
/// admin cookies; carrying it across machines would invalidate sessions
/// on one host and silently bind sessions to a foreign secret on the
/// other. Each install must mint its own.
pub const SETTINGS_KEYS_NEVER_EXPORTED: &[&str] = &["session_secret"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub version: u32,
    /// RFC3339 UTC timestamp of when the bundle was produced.
    pub produced_at: String,
    /// `CARGO_PKG_VERSION` of the binary that produced the bundle.
    pub clewdr_version: String,
    /// Per-table schema captured at export time, keyed by table name.
    pub schema: BTreeMap<String, TableSchema>,
    /// Verbatim contents of `clewdr.toml` (empty string if absent or in no_fs mode).
    pub config_toml: String,
    /// Per-table rows, keyed by table name. Each row is a column→value map.
    pub tables: BTreeMap<String, Vec<Row>>,
    /// Tables we deliberately skipped, for transparency in the bundle.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub columns: Vec<ColumnInfo>,
    /// Column names that participate in the PRIMARY KEY, in PK order.
    pub pk: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Declared SQLite type (`INTEGER` / `TEXT` / `BLOB` / `REAL` / etc.).
    pub kind: String,
    pub notnull: bool,
    pub default_value: Option<String>,
}

pub type Row = BTreeMap<String, CellValue>;

/// SQLite cell value. Custom Serialize / Deserialize so:
/// - NULL  → JSON null
/// - INT   → JSON integer
/// - REAL  → JSON float
/// - TEXT  → JSON string
/// - BLOB  → JSON object `{"type":"blob","base64":"..."}` (so it can't be
///   confused with an arbitrary text cell that happens to look base64-y)
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Serialize for CellValue {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            CellValue::Null => ser.serialize_unit(),
            CellValue::Integer(i) => ser.serialize_i64(*i),
            CellValue::Real(f) => ser.serialize_f64(*f),
            CellValue::Text(t) => ser.serialize_str(t),
            CellValue::Blob(b) => {
                let mut m = ser.serialize_map(Some(2))?;
                m.serialize_entry("type", "blob")?;
                m.serialize_entry(
                    "base64",
                    &base64::engine::general_purpose::STANDARD.encode(b),
                )?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for CellValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Null => Ok(CellValue::Null),
            serde_json::Value::Bool(b) => Ok(CellValue::Integer(if b { 1 } else { 0 })),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(CellValue::Integer(i))
                } else if let Some(f) = n.as_f64() {
                    Ok(CellValue::Real(f))
                } else {
                    Err(D::Error::custom("number out of representable range"))
                }
            }
            serde_json::Value::String(s) => Ok(CellValue::Text(s)),
            serde_json::Value::Object(o) => {
                let ty = o.get("type").and_then(serde_json::Value::as_str);
                let b64 = o.get("base64").and_then(serde_json::Value::as_str);
                match (ty, b64) {
                    (Some("blob"), Some(b64)) => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(D::Error::custom)?;
                        Ok(CellValue::Blob(bytes))
                    }
                    _ => Err(D::Error::custom(
                        "object cell must have type=\"blob\" and base64 fields",
                    )),
                }
            }
            serde_json::Value::Array(_) => Err(D::Error::custom("arrays are not valid cells")),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Schema + row readers (PRAGMA table_info + SELECT *)
// ──────────────────────────────────────────────────────────────────────────

pub async fn read_table_schema(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<TableSchema, ClewdrError> {
    // PRAGMA table_info returns: cid, name, type, notnull, dflt_value, pk.
    // Composite PKs use 1-indexed positions on the pk column; we capture
    // them in PK order.
    let rows: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA table_info({table})"))
            .fetch_all(&mut *conn)
            .await?;
    if rows.is_empty() {
        return Err(ClewdrError::NotFound {
            msg: "table not found in schema",
        });
    }
    let mut columns = Vec::with_capacity(rows.len());
    let mut pk_pairs: Vec<(i64, String)> = Vec::new();
    for (_cid, name, kind, notnull, default_value, pk_pos) in rows {
        if pk_pos > 0 {
            pk_pairs.push((pk_pos, name.clone()));
        }
        columns.push(ColumnInfo {
            name,
            kind,
            notnull: notnull != 0,
            default_value,
        });
    }
    pk_pairs.sort_by_key(|(pos, _)| *pos);
    let pk = pk_pairs.into_iter().map(|(_, n)| n).collect();
    Ok(TableSchema { columns, pk })
}

pub async fn read_table_rows(
    conn: &mut SqliteConnection,
    table: &str,
    schema: &TableSchema,
) -> Result<Vec<Row>, ClewdrError> {
    let sql = format!("SELECT * FROM {table}");
    let rows = sqlx::query(&sql).fetch_all(&mut *conn).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells: Row = BTreeMap::new();
        for col in row.columns() {
            let name = col.name();
            let cell = read_cell(&row, name)?;
            cells.insert(name.to_string(), cell);
        }
        // Make sure every schema-declared column appears, even if SELECT *
        // happened to return them in a different order on a future SQLite.
        for declared in &schema.columns {
            cells
                .entry(declared.name.clone())
                .or_insert(CellValue::Null);
        }
        out.push(cells);
    }
    Ok(out)
}

fn read_cell(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<CellValue, ClewdrError> {
    let raw = row.try_get_raw(name)?;
    if raw.is_null() {
        return Ok(CellValue::Null);
    }
    let ty = raw.type_info().name().to_ascii_uppercase();
    let cell = match ty.as_str() {
        "INTEGER" | "INT" | "BIGINT" | "BOOLEAN" => {
            let v: i64 = row.try_get(name)?;
            CellValue::Integer(v)
        }
        "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" => {
            // NUMERIC affinity sometimes lands as INTEGER at row level —
            // sqlx will downcast cleanly. If integer fits, prefer Integer.
            if let Ok(i) = row.try_get::<i64, _>(name) {
                CellValue::Integer(i)
            } else {
                let f: f64 = row.try_get(name)?;
                CellValue::Real(f)
            }
        }
        "TEXT" | "VARCHAR" | "CHAR" | "CLOB" | "DATETIME" | "DATE" => {
            let v: String = row.try_get(name)?;
            CellValue::Text(v)
        }
        "BLOB" => {
            let v: Vec<u8> = row.try_get(name)?;
            CellValue::Blob(v)
        }
        // Unknown affinity: try string, fall back to blob.
        _ => match row.try_get::<String, _>(name) {
            Ok(s) => CellValue::Text(s),
            Err(_) => match row.try_get::<Vec<u8>, _>(name) {
                Ok(b) => CellValue::Blob(b),
                Err(e) => return Err(e.into()),
            },
        },
    };
    Ok(cell)
}

// ──────────────────────────────────────────────────────────────────────────
// Secret redaction (consumed by export-config when --no-secrets is set)
// ──────────────────────────────────────────────────────────────────────────

/// Columns whose values are session-bearing secrets. When `--no-secrets` is
/// set, these are nulled before serialization. Centralised here so import
/// can later reason about "this row was redacted on export" without
/// re-deriving the list.
///
/// Note on `api_keys`:
/// - `plaintext_key` is the literal `sk-clewdr-…` string the user copies
///   into a client. Bundling it with `--no-secrets` would defeat the flag.
/// - `key_hash` is the blake3 verification hash; with the lookup_key
///   prefix it's enough to brute-force narrow keyspaces, so we strip it
///   too. Net effect: a `--no-secrets` bundle keeps api_keys metadata
///   (label, user_id, expiry, lookup_key prefix) but cannot be used to
///   authenticate — the operator must rotate keys after restore.
pub fn secret_columns_for(table: &str) -> &'static [&'static str] {
    match table {
        "accounts" => &[
            "cookie_blob",
            "oauth_access_token",
            "oauth_refresh_token",
            // Step 5 / C11: ApiKey credentials are secrets too.
            // `api_key_secret` is the bearer-equivalent; the
            // `api_key_extra_headers` JSON column carries per-account
            // header values (e.g. `anthropic-workspace-id`) that PRD
            // §Security classifies as secret. `api_key_base_url` is
            // admin-supplied metadata and stays in the bundle so the
            // operator can rebuild the account after rotating keys.
            "api_key_secret",
            "api_key_extra_headers",
        ],
        "api_keys" => &["plaintext_key", "key_hash"],
        "proxies" => &["password"],
        "users" => &["password_hash"],
        _ => &[],
    }
}

pub fn redact_secrets_in_place(table: &str, rows: &mut [Row]) {
    let cols = secret_columns_for(table);
    if cols.is_empty() {
        return;
    }
    for row in rows {
        for col in cols {
            if let Some(v) = row.get_mut(*col)
                && !matches!(v, CellValue::Null)
            {
                *v = CellValue::Null;
            }
        }
    }
}

/// Strip rows whose `key` column matches a never-exported settings key.
pub fn drop_blocked_settings_rows(rows: &mut Vec<Row>) {
    rows.retain(|row| {
        let Some(CellValue::Text(key)) = row.get("key") else {
            return true;
        };
        !SETTINGS_KEYS_NEVER_EXPORTED.contains(&key.as_str())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(v: CellValue) -> CellValue {
        let json = serde_json::to_string(&v).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn cell_value_roundtrips_null() {
        assert_eq!(round_trip(CellValue::Null), CellValue::Null);
    }

    #[test]
    fn cell_value_roundtrips_integer() {
        assert_eq!(round_trip(CellValue::Integer(42)), CellValue::Integer(42));
        assert_eq!(round_trip(CellValue::Integer(-1)), CellValue::Integer(-1));
        assert_eq!(
            round_trip(CellValue::Integer(i64::MAX)),
            CellValue::Integer(i64::MAX)
        );
    }

    #[test]
    fn cell_value_roundtrips_real() {
        match round_trip(CellValue::Real(3.14159)) {
            CellValue::Real(v) => assert!((v - 3.14159).abs() < 1e-9),
            other => panic!("expected Real, got {other:?}"),
        }
    }

    #[test]
    fn cell_value_roundtrips_text() {
        assert_eq!(
            round_trip(CellValue::Text("hello".to_string())),
            CellValue::Text("hello".to_string())
        );
        // Unicode + control chars
        let s = "α\nβ\tγ".to_string();
        assert_eq!(round_trip(CellValue::Text(s.clone())), CellValue::Text(s));
    }

    #[test]
    fn cell_value_roundtrips_blob_bytewise() {
        // Cookie-shaped bytes: sub-blobs that include null bytes, high
        // bytes, and ASCII alike. Must round-trip *byte-identically*.
        let bytes: Vec<u8> = (0..=255u8).collect();
        let restored = round_trip(CellValue::Blob(bytes.clone()));
        match restored {
            CellValue::Blob(b) => assert_eq!(b, bytes),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn blob_serializes_as_tagged_object() {
        // Ensures TEXT cells that look base64-ish never get mistaken for
        // BLOBs: blobs are objects, text is bare strings.
        let v = CellValue::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let val: serde_json::Value = serde_json::to_value(&v).unwrap();
        assert_eq!(val["type"], "blob");
        assert_eq!(val["base64"], "3q2+7w=="); // base64(0xDE 0xAD 0xBE 0xEF)
    }

    #[test]
    fn text_does_not_get_decoded_as_blob() {
        // A text cell containing a string that *looks* like our blob
        // marker must not be reinterpreted on deserialize. Raw strings
        // always stay as Text; only object-shaped cells become Blob.
        let v = CellValue::Text("dGVzdA==".to_string()); // base64("test")
        assert_eq!(round_trip(v.clone()), v);
    }

    #[test]
    fn deserialize_rejects_object_without_blob_tag() {
        let raw = json!({"type": "image", "base64": "AAAA"});
        let res: Result<CellValue, _> = serde_json::from_value(raw);
        assert!(res.is_err());
    }

    #[test]
    fn deserialize_bool_coerces_to_integer() {
        // SQLite has no boolean type; we accept JSON bool for forward
        // compat with non-rust consumers and store it as 0/1.
        let res: CellValue = serde_json::from_str("true").unwrap();
        assert_eq!(res, CellValue::Integer(1));
        let res: CellValue = serde_json::from_str("false").unwrap();
        assert_eq!(res, CellValue::Integer(0));
    }

    fn make_rows(secrets: Option<Vec<u8>>) -> Vec<Row> {
        let mut row: Row = BTreeMap::new();
        row.insert("id".to_string(), CellValue::Integer(1));
        row.insert("name".to_string(), CellValue::Text("acct".to_string()));
        if let Some(b) = secrets {
            row.insert("cookie_blob".to_string(), CellValue::Blob(b));
            row.insert(
                "oauth_refresh_token".to_string(),
                CellValue::Blob(vec![0xAA]),
            );
            // Step 5 / C11: ApiKey-shaped secrets also fall under
            // accounts redaction.
            row.insert(
                "api_key_secret".to_string(),
                CellValue::Text("sk-ant-test".to_string()),
            );
            row.insert(
                "api_key_extra_headers".to_string(),
                CellValue::Text(r#"{"anthropic-workspace-id":"ws-secret"}"#.to_string()),
            );
        }
        vec![row]
    }

    #[test]
    fn redact_secrets_nulls_account_secret_columns() {
        let mut rows = make_rows(Some(vec![0xDE, 0xAD]));
        redact_secrets_in_place("accounts", &mut rows);
        let row = &rows[0];
        // Non-secret columns untouched
        assert_eq!(row["id"], CellValue::Integer(1));
        assert_eq!(row["name"], CellValue::Text("acct".to_string()));
        // Secret columns nulled (cookie + oauth + api_key shapes)
        assert_eq!(row["cookie_blob"], CellValue::Null);
        assert_eq!(row["oauth_refresh_token"], CellValue::Null);
        assert_eq!(row["api_key_secret"], CellValue::Null);
        assert_eq!(row["api_key_extra_headers"], CellValue::Null);
    }

    #[test]
    fn redact_secrets_is_table_aware() {
        // For tables without a secret column list, redaction is a no-op
        // and must not silently drop any data.
        let mut rows = make_rows(Some(vec![0xDE]));
        redact_secrets_in_place("policies", &mut rows);
        assert_eq!(rows[0]["cookie_blob"], CellValue::Blob(vec![0xDE]));
    }

    #[test]
    fn drop_blocked_settings_rows_filters_session_secret() {
        let mut rows = vec![
            BTreeMap::from([
                ("key".to_string(), CellValue::Text("session_secret".into())),
                ("value".to_string(), CellValue::Text("x".into())),
            ]),
            BTreeMap::from([
                ("key".to_string(), CellValue::Text("ip_pool".into())),
                ("value".to_string(), CellValue::Text("y".into())),
            ]),
        ];
        drop_blocked_settings_rows(&mut rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["key"],
            CellValue::Text("ip_pool".to_string()),
            "only the non-blocked row should survive"
        );
    }

    #[tokio::test]
    async fn read_table_schema_returns_users_columns() {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let schema = read_table_schema(&mut conn, "users").await.unwrap();
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        // We don't pin the exact column set (migrations evolve it), but
        // these load-bearing ones must always be present.
        for required in ["id", "username", "password_hash", "role", "session_version"] {
            assert!(
                names.contains(&required),
                "users schema missing {required}: {names:?}"
            );
        }
        assert_eq!(schema.pk, vec!["id".to_string()]);
    }

    #[tokio::test]
    async fn read_table_rows_roundtrips_seeded_models() {
        let pool = crate::db::init_pool(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        crate::db::seed_admin(&pool).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let schema = read_table_schema(&mut conn, "models").await.unwrap();
        let rows = read_table_rows(&mut conn, "models", &schema).await.unwrap();
        assert!(!rows.is_empty(), "expected seeded model rows");
        let first = &rows[0];
        assert!(matches!(first.get("model_id"), Some(CellValue::Text(_))));
    }
}
