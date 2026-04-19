use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProxyRow {
    pub id: i64,
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>,
    pub last_test_success: Option<i64>,
    pub last_test_latency_ms: Option<i64>,
    pub last_test_message: Option<String>,
    pub last_test_ip_address: Option<String>,
    pub last_test_country: Option<String>,
    pub last_test_region: Option<String>,
    pub last_test_city: Option<String>,
    pub last_test_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProxyUpdate {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub username: Option<Option<String>>,
    pub password: Option<Option<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ProxyTestResultUpdate<'a> {
    pub success: Option<bool>,
    pub latency_ms: Option<i64>,
    pub message: Option<&'a str>,
    pub ip_address: Option<&'a str>,
    pub country: Option<&'a str>,
    pub region: Option<&'a str>,
    pub city: Option<&'a str>,
}

pub fn build_proxy_url(row: &ProxyRow) -> Result<String, url::ParseError> {
    build_proxy_url_from_parts(
        &row.protocol,
        &row.host,
        row.port,
        row.username.as_deref(),
        row.password.as_deref(),
    )
}

pub fn build_proxy_url_from_parts(
    protocol: &str,
    host: &str,
    port: i64,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(&format!("{protocol}://{host}:{port}"))?;
    match (
        username.map(str::trim).filter(|s| !s.is_empty()),
        password.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(user), Some(pass)) => {
            let _ = url.set_username(user);
            let _ = url.set_password(Some(pass));
        }
        (Some(user), None) => {
            let _ = url.set_username(user);
            let _ = url.set_password(None);
        }
        _ => {}
    }
    Ok(url.to_string())
}

pub async fn list_proxies(
    pool: &SqlitePool,
    offset: i64,
    limit: i64,
) -> Result<(Vec<ProxyRow>, i64), sqlx::Error> {
    let items = sqlx::query_as::<_, ProxyRow>(
        r#"SELECT
            id, name, protocol, host, port, username, password,
            last_test_success, last_test_latency_ms, last_test_message,
            last_test_ip_address, last_test_country, last_test_region, last_test_city,
            last_test_at, created_at, updated_at
        FROM proxies
        ORDER BY created_at DESC, id DESC
        LIMIT ?1 OFFSET ?2"#,
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM proxies")
        .fetch_one(pool)
        .await?;

    Ok((items, total))
}

pub async fn list_all_proxies(pool: &SqlitePool) -> Result<Vec<ProxyRow>, sqlx::Error> {
    sqlx::query_as::<_, ProxyRow>(
        r#"SELECT
            id, name, protocol, host, port, username, password,
            last_test_success, last_test_latency_ms, last_test_message,
            last_test_ip_address, last_test_country, last_test_region, last_test_city,
            last_test_at, created_at, updated_at
        FROM proxies
        ORDER BY created_at DESC, id DESC"#,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_proxy_by_id(pool: &SqlitePool, id: i64) -> Result<Option<ProxyRow>, sqlx::Error> {
    sqlx::query_as::<_, ProxyRow>(
        r#"SELECT
            id, name, protocol, host, port, username, password,
            last_test_success, last_test_latency_ms, last_test_message,
            last_test_ip_address, last_test_country, last_test_region, last_test_city,
            last_test_at, created_at, updated_at
        FROM proxies
        WHERE id = ?1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn create_proxy(
    pool: &SqlitePool,
    name: &str,
    protocol: &str,
    host: &str,
    port: i64,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<ProxyRow, sqlx::Error> {
    let id = sqlx::query(
        r#"INSERT INTO proxies (
            name, protocol, host, port, username, password
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
    )
    .bind(name)
    .bind(protocol)
    .bind(host)
    .bind(port)
    .bind(username)
    .bind(password)
    .execute(pool)
    .await?
    .last_insert_rowid();

    get_proxy_by_id(pool, id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub async fn update_proxy(
    pool: &SqlitePool,
    id: i64,
    update: ProxyUpdate,
) -> Result<Option<ProxyRow>, sqlx::Error> {
    sqlx::query(
        r#"UPDATE proxies
           SET name = COALESCE(?1, name),
               protocol = COALESCE(?2, protocol),
               host = COALESCE(?3, host),
               port = COALESCE(?4, port),
               username = CASE WHEN ?5 = 1 THEN ?6 ELSE username END,
               password = CASE WHEN ?7 = 1 THEN ?8 ELSE password END,
               updated_at = CURRENT_TIMESTAMP
         WHERE id = ?9"#,
    )
    .bind(update.name)
    .bind(update.protocol)
    .bind(update.host)
    .bind(update.port)
    .bind(update.username.is_some() as i64)
    .bind(update.username.flatten())
    .bind(update.password.is_some() as i64)
    .bind(update.password.flatten())
    .bind(id)
    .execute(pool)
    .await?;

    get_proxy_by_id(pool, id).await
}

pub async fn delete_proxy(pool: &SqlitePool, id: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM proxies WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected())
}

pub async fn update_proxy_test_result(
    pool: &SqlitePool,
    id: i64,
    update: ProxyTestResultUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE proxies
           SET last_test_success = ?1,
               last_test_latency_ms = ?2,
               last_test_message = ?3,
               last_test_ip_address = ?4,
               last_test_country = ?5,
               last_test_region = ?6,
               last_test_city = ?7,
               last_test_at = CURRENT_TIMESTAMP,
               updated_at = CURRENT_TIMESTAMP
         WHERE id = ?8"#,
    )
    .bind(update.success.map(|v| v as i64))
    .bind(update.latency_ms)
    .bind(update.message)
    .bind(update.ip_address)
    .bind(update.country)
    .bind(update.region)
    .bind(update.city)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
