use sqlx::SqlitePool;

use super::api_key::generate_api_key;
use super::models::AuthenticatedUser;

/// Row shape returned by `authenticate_api_key`'s `sqlx::query_as`:
/// (ak.id, u.id, u.username, u.role, ak.key_hash, u.policy_id,
///  p.max_concurrent, p.rpm_limit, p.weekly_budget_nanousd, p.monthly_budget_nanousd).
type ApiKeyAuthRow = (i64, i64, String, String, Vec<u8>, i64, i32, i32, i64, i64);

/// Look up an API key by its lookup_key prefix, then verify the full blake3 hash.
pub async fn authenticate_api_key(
    pool: &SqlitePool,
    lookup_key: &str,
    full_key_hash: &[u8; 32],
) -> Result<Option<AuthenticatedUser>, sqlx::Error> {
    let row: Option<ApiKeyAuthRow> = sqlx::query_as(
        r#"
        SELECT ak.id, u.id, u.username, u.role, ak.key_hash, u.policy_id,
               p.max_concurrent, p.rpm_limit,
               p.weekly_budget_nanousd, p.monthly_budget_nanousd
        FROM api_keys ak
        JOIN users u ON ak.user_id = u.id
        JOIN policies p ON u.policy_id = p.id
        WHERE ak.lookup_key = ?1
          AND ak.disabled_at IS NULL
          AND (ak.expires_at IS NULL OR ak.expires_at > CURRENT_TIMESTAMP)
          AND u.disabled_at IS NULL
        "#,
    )
    .bind(lookup_key)
    .fetch_optional(pool)
    .await?;

    let Some((
        ak_id,
        user_id,
        username,
        role,
        stored_hash,
        policy_id,
        max_concurrent,
        rpm_limit,
        weekly_budget_nanousd,
        monthly_budget_nanousd,
    )) = row
    else {
        return Ok(None);
    };

    let stored: [u8; 32] = match stored_hash.try_into() {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };

    if stored != *full_key_hash {
        return Ok(None);
    }

    Ok(Some(AuthenticatedUser {
        user_id,
        username,
        role,
        api_key_id: Some(ak_id),
        policy_id,
        max_concurrent,
        rpm_limit,
        weekly_budget_nanousd,
        monthly_budget_nanousd,
        bound_account_ids: {
            let rows: Vec<(i64,)> = sqlx::query_as(
                "SELECT account_id FROM api_key_account_bindings WHERE api_key_id = ?1",
            )
            .bind(ak_id)
            .fetch_all(pool)
            .await?;
            rows.into_iter().map(|(id,)| id).collect()
        },
    }))
}

/// Update last_used_at and last_used_ip for an API key (fire-and-forget).
pub async fn touch_api_key(
    pool: &SqlitePool,
    api_key_id: i64,
    ip: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE api_keys SET last_used_at = CURRENT_TIMESTAMP, last_used_ip = ?1 WHERE id = ?2",
    )
    .bind(ip)
    .bind(api_key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update last_seen_at for a user (fire-and-forget).
pub async fn touch_user(pool: &SqlitePool, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_seen_at = CURRENT_TIMESTAMP WHERE id = ?1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Create a new API key for a user. Returns the full plaintext key (shown only once).
pub async fn create_api_key(
    pool: &SqlitePool,
    user_id: i64,
    label: Option<&str>,
) -> Result<String, sqlx::Error> {
    loop {
        let (plaintext, lookup_key, key_hash) = generate_api_key();
        let result = sqlx::query(
            "INSERT INTO api_keys (user_id, label, lookup_key, key_hash, plaintext_key) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(user_id)
        .bind(label)
        .bind(&lookup_key)
        .bind(key_hash.as_slice())
        .bind(&plaintext)
        .execute(pool)
        .await;

        match result {
            Ok(_) => return Ok(plaintext),
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                // lookup_key collision, retry with new random key
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
