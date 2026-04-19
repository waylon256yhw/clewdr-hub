use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

use super::common::{Paginated, PaginationParams};
use crate::{
    db::proxies::{
        ProxyRow, ProxyTestResultUpdate, ProxyUpdate, build_proxy_url, create_proxy, delete_proxy,
        get_proxy_by_id, list_proxies, update_proxy, update_proxy_test_result,
    },
    error::ClewdrError,
    services::account_pool::AccountPoolHandle,
};

#[derive(Serialize)]
pub struct ProxyResponse {
    pub id: i64,
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>,
    pub last_test_success: Option<bool>,
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

#[derive(Serialize)]
pub struct ProxyTestResponse {
    pub success: bool,
    pub message: String,
    pub latency_ms: Option<i64>,
    pub ip_address: Option<String>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateProxyRequest {
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateProxyRequest {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn map_proxy(row: ProxyRow) -> ProxyResponse {
    ProxyResponse {
        id: row.id,
        name: row.name,
        protocol: row.protocol,
        host: row.host,
        port: row.port,
        username: row.username,
        password: row.password,
        last_test_success: row.last_test_success.map(|v| v != 0),
        last_test_latency_ms: row.last_test_latency_ms,
        last_test_message: row.last_test_message,
        last_test_ip_address: row.last_test_ip_address,
        last_test_country: row.last_test_country,
        last_test_region: row.last_test_region,
        last_test_city: row.last_test_city,
        last_test_at: row.last_test_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn validate_protocol(protocol: &str) -> Result<(), ClewdrError> {
    match protocol.trim() {
        "http" | "https" | "socks5" | "socks5h" => Ok(()),
        _ => Err(ClewdrError::BadRequest {
            msg: "invalid proxy protocol",
        }),
    }
}

fn validate_port(port: i64) -> Result<(), ClewdrError> {
    if (1..=65535).contains(&port) {
        Ok(())
    } else {
        Err(ClewdrError::BadRequest {
            msg: "invalid proxy port",
        })
    }
}

#[derive(Deserialize)]
struct IpWhoResponse {
    success: bool,
    ip: Option<String>,
    country: Option<String>,
    region: Option<String>,
    city: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct HttpBinResponse {
    origin: String,
}

#[derive(Deserialize)]
struct Ip2LocationResponse {
    country_name: Option<String>,
    region_name: Option<String>,
    city_name: Option<String>,
}

fn error_chain_contains(err: &(dyn std::error::Error + 'static), needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    let mut current = Some(err);
    while let Some(source) = current {
        if source.to_string().to_ascii_lowercase().contains(&needle) {
            return true;
        }
        current = source.source();
    }
    false
}

fn describe_probe_status(status: StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => "探测目标拒绝访问".to_string(),
        407 => "代理鉴权失败".to_string(),
        408 => "探测请求超时".to_string(),
        429 => "探测目标限流".to_string(),
        500..=599 => "探测目标暂时不可用".to_string(),
        code => format!("探测目标返回 HTTP {code}"),
    }
}

fn normalize_proxy_test_error(err: &wreq::Error) -> String {
    if err.is_timeout() {
        return "连接超时".to_string();
    }
    if let Some(status) = err.status() {
        return describe_probe_status(status);
    }
    if err.is_proxy_connect() {
        if error_chain_contains(err, "proxy authorization required")
            || error_chain_contains(err, "authentication required")
            || error_chain_contains(err, "authorization required")
        {
            return "代理鉴权失败".to_string();
        }
        if error_chain_contains(err, "connection refused")
            || error_chain_contains(err, "actively refused")
        {
            return "代理拒绝连接".to_string();
        }
        if error_chain_contains(err, "dns")
            || error_chain_contains(err, "resolve")
            || error_chain_contains(err, "lookup")
            || error_chain_contains(err, "name or service not known")
        {
            return "代理地址解析失败".to_string();
        }
        if error_chain_contains(err, "network is unreachable")
            || error_chain_contains(err, "no route to host")
        {
            return "代理网络不可达".to_string();
        }
        return "无法连接到代理".to_string();
    }
    if err.is_connect() {
        if error_chain_contains(err, "dns")
            || error_chain_contains(err, "resolve")
            || error_chain_contains(err, "lookup")
            || error_chain_contains(err, "name or service not known")
        {
            return "目标地址解析失败".to_string();
        }
        if error_chain_contains(err, "connection refused")
            || error_chain_contains(err, "actively refused")
        {
            return "目标地址拒绝连接".to_string();
        }
        if error_chain_contains(err, "network is unreachable")
            || error_chain_contains(err, "no route to host")
        {
            return "目标网络不可达".to_string();
        }
        return "网络连接失败".to_string();
    }
    if err.is_tls()
        || error_chain_contains(err, "tls")
        || error_chain_contains(err, "ssl")
        || error_chain_contains(err, "certificate")
    {
        return "TLS 握手失败".to_string();
    }
    if err.is_decode()
        || error_chain_contains(err, "json")
        || error_chain_contains(err, "decode")
        || error_chain_contains(err, "expected value")
    {
        return "探测响应解析失败".to_string();
    }
    if err.is_builder() {
        return "代理配置无效".to_string();
    }
    "代理测试失败".to_string()
}

fn primary_ip_address(ip_address: &str) -> Option<&str> {
    let first = ip_address.split(',').next()?.trim();
    (!first.is_empty()).then_some(first)
}

fn normalize_country_name(country_name: Option<String>) -> Option<String> {
    country_name
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn lookup_ip_location(ip_address: &str) -> (Option<String>, Option<String>, Option<String>) {
    let Some(primary_ip) = primary_ip_address(ip_address) else {
        return (None, None, None);
    };

    let encoded_ip: String = url::form_urlencoded::byte_serialize(primary_ip.as_bytes()).collect();
    let lookup_url = format!("https://api.ip2location.io/?ip={encoded_ip}&format=json");
    let client = match wreq::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "failed to build ip2location client");
            return (None, None, None);
        }
    };

    let response = match client
        .get(&lookup_url)
        .header("accept", "application/json")
        .header("user-agent", "clewdr-hub/proxy-test")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            warn!(ip = primary_ip, error = %err, "ip2location lookup failed");
            return (None, None, None);
        }
    };

    if !response.status().is_success() {
        warn!(
            ip = primary_ip,
            status = response.status().as_u16(),
            "ip2location lookup returned non-success status"
        );
        return (None, None, None);
    }

    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            warn!(ip = primary_ip, error = %err, "failed to read ip2location response");
            return (None, None, None);
        }
    };

    match serde_json::from_str::<Ip2LocationResponse>(&body) {
        Ok(parsed) => (
            normalize_country_name(parsed.country_name),
            parsed.region_name,
            parsed.city_name,
        ),
        Err(err) => {
            warn!(ip = primary_ip, error = %err, "failed to parse ip2location response");
            (None, None, None)
        }
    }
}

async fn probe_proxy(proxy_url: &str) -> ProxyTestResponse {
    let probes = [
        ("https://ipwho.is/", "ipwho"),
        ("https://httpbin.org/ip", "httpbin"),
    ];
    let mut client_builder = wreq::Client::builder().timeout(std::time::Duration::from_secs(10));
    if let Some(proxy) = crate::claude_code_state::proxy_from_url(Some(proxy_url)) {
        client_builder = client_builder.proxy(proxy);
    }
    let client = match client_builder.build() {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "failed to build proxy test client");
            return ProxyTestResponse {
                success: false,
                message: normalize_proxy_test_error(&err),
                latency_ms: None,
                ip_address: None,
                country: None,
                region: None,
                city: None,
            };
        }
    };

    let mut last_message = "proxy test failed".to_string();
    for (url, parser) in probes {
        let started = std::time::Instant::now();
        let response = client
            .get(url)
            .header("accept", "application/json")
            .header("user-agent", "clewdr-hub/proxy-test")
            .send()
            .await;
        let latency_ms = started.elapsed().as_millis() as i64;

        let resp = match response {
            Ok(resp) => resp,
            Err(err) => {
                warn!(probe_url = url, error = %err, "proxy test request failed");
                last_message = normalize_proxy_test_error(&err);
                continue;
            }
        };
        if !resp.status().is_success() {
            last_message = describe_probe_status(resp.status());
            continue;
        }

        let body = match resp.text().await {
            Ok(body) => body,
            Err(err) => {
                warn!(probe_url = url, error = %err, "failed to read proxy test response");
                last_message = normalize_proxy_test_error(&err);
                continue;
            }
        };

        match parser {
            "ipwho" => match serde_json::from_str::<IpWhoResponse>(&body) {
                Ok(parsed) if parsed.success => {
                    let ip_address = parsed.ip;
                    let (country, region, city) = match ip_address.as_deref() {
                        Some(ip) => lookup_ip_location(ip).await,
                        None => (None, None, None),
                    };
                    return ProxyTestResponse {
                        success: true,
                        message: "基础连通性正常".to_string(),
                        latency_ms: Some(latency_ms),
                        ip_address,
                        country: country.or_else(|| normalize_country_name(parsed.country)),
                        region: region.or(parsed.region),
                        city: city.or(parsed.city),
                    };
                }
                Ok(parsed) => {
                    warn!(
                        probe_url = url,
                        message = parsed.message.as_deref().unwrap_or("unknown"),
                        "probe target reported failure"
                    );
                    last_message = "探测目标返回异常".to_string();
                }
                Err(err) => {
                    warn!(probe_url = url, error = %err, "failed to parse ipwho response");
                    last_message = "探测响应解析失败".to_string();
                }
            },
            "httpbin" => match serde_json::from_str::<HttpBinResponse>(&body) {
                Ok(parsed) => {
                    let ip_address = parsed.origin;
                    let (country, region, city) = lookup_ip_location(&ip_address).await;
                    return ProxyTestResponse {
                        success: true,
                        message: "基础连通性正常".to_string(),
                        latency_ms: Some(latency_ms),
                        ip_address: Some(ip_address),
                        country,
                        region,
                        city,
                    };
                }
                Err(err) => {
                    warn!(probe_url = url, error = %err, "failed to parse httpbin response");
                    last_message = "探测响应解析失败".to_string();
                }
            },
            _ => {}
        }
    }

    ProxyTestResponse {
        success: false,
        message: last_message,
        latency_ms: None,
        ip_address: None,
        country: None,
        region: None,
        city: None,
    }
}

pub async fn list(
    State(db): State<SqlitePool>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Paginated<ProxyResponse>>, ClewdrError> {
    let (offset, limit) = params.resolve();
    let (items, total) = list_proxies(&db, offset, limit).await?;
    Ok(Json(Paginated {
        items: items.into_iter().map(map_proxy).collect(),
        total,
        offset,
        limit,
    }))
}

pub async fn create(
    State(db): State<SqlitePool>,
    Json(req): Json<CreateProxyRequest>,
) -> Result<(StatusCode, Json<ProxyResponse>), ClewdrError> {
    let name = req.name.trim();
    let protocol = req.protocol.trim();
    let host = req.host.trim();
    if name.is_empty() || host.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "proxy name and host are required",
        });
    }
    validate_protocol(protocol)?;
    validate_port(req.port)?;
    let row = create_proxy(
        &db,
        name,
        protocol,
        host,
        req.port,
        normalize_optional(req.username).as_deref(),
        normalize_optional(req.password).as_deref(),
    )
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e
            && de.message().contains("UNIQUE")
        {
            return ClewdrError::Conflict {
                msg: "proxy name already exists",
            };
        }
        ClewdrError::from(e)
    })?;
    Ok((StatusCode::CREATED, Json(map_proxy(row))))
}

pub async fn update(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateProxyRequest>,
) -> Result<Json<ProxyResponse>, ClewdrError> {
    if let Some(ref protocol) = req.protocol {
        validate_protocol(protocol.trim())?;
    }
    if let Some(port) = req.port {
        validate_port(port)?;
    }
    let row = update_proxy(
        &db,
        id,
        ProxyUpdate {
            name: req
                .name
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            protocol: req
                .protocol
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            host: req
                .host
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            port: req.port,
            username: req.username.map(|v| normalize_optional(Some(v))),
            password: req.password.map(|v| normalize_optional(Some(v))),
        },
    )
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e
            && de.message().contains("UNIQUE")
        {
            return ClewdrError::Conflict {
                msg: "proxy name already exists",
            };
        }
        ClewdrError::from(e)
    })?
    .ok_or(ClewdrError::NotFound {
        msg: "proxy not found",
    })?;
    let _ = actor.reload_from_db().await;
    Ok(Json(map_proxy(row)))
}

pub async fn remove(
    State(db): State<SqlitePool>,
    State(actor): State<AccountPoolHandle>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ClewdrError> {
    if delete_proxy(&db, id).await? == 0 {
        return Err(ClewdrError::NotFound {
            msg: "proxy not found",
        });
    }
    let _ = actor.reload_from_db().await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn test(
    State(db): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<Json<ProxyTestResponse>, ClewdrError> {
    let proxy = get_proxy_by_id(&db, id)
        .await?
        .ok_or(ClewdrError::NotFound {
            msg: "proxy not found",
        })?;
    let proxy_url = build_proxy_url(&proxy).map_err(|_| ClewdrError::BadRequest {
        msg: "Invalid proxy configuration",
    })?;
    let result = probe_proxy(&proxy_url).await;

    update_proxy_test_result(
        &db,
        id,
        ProxyTestResultUpdate {
            success: Some(result.success),
            latency_ms: result.latency_ms,
            message: Some(result.message.as_str()),
            ip_address: result.ip_address.as_deref(),
            country: result.country.as_deref(),
            region: result.region.as_deref(),
            city: result.city.as_deref(),
        },
    )
    .await?;

    Ok(Json(result))
}
