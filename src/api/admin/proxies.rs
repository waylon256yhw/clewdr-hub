use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use tracing::warn;

use super::common::{Paginated, PaginationParams};
use crate::{
    billing::{BillingContext, RequestType, persist_probe_log},
    db::proxies::{
        ProxyRow, ProxyTestResultUpdate, ProxyUpdate, build_proxy_url, create_proxy, delete_proxy,
        get_proxy_by_id, list_proxies, update_proxy, update_proxy_test_result,
    },
    error::ClewdrError,
    services::account_pool::AccountPoolHandle,
    state::AppState,
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

struct IpLocationLookupResult {
    country: Option<String>,
    region: Option<String>,
    city: Option<String>,
    http_status: Option<u16>,
    raw_body: Option<Value>,
    error: Option<String>,
}

struct ProxyProbeOutcome {
    response: ProxyTestResponse,
    http_status: Option<u16>,
    log_status: &'static str,
    bundle: Value,
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

fn parse_json_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn proxy_log_status(message: &str, http_status: Option<u16>) -> &'static str {
    if matches!(http_status, Some(401 | 403 | 407)) || message == "代理鉴权失败" {
        "auth_rejected"
    } else {
        "upstream_error"
    }
}

async fn lookup_ip_location(ip_address: &str) -> IpLocationLookupResult {
    let Some(primary_ip) = primary_ip_address(ip_address) else {
        return IpLocationLookupResult {
            country: None,
            region: None,
            city: None,
            http_status: None,
            raw_body: None,
            error: Some("missing primary ip".to_string()),
        };
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
            return IpLocationLookupResult {
                country: None,
                region: None,
                city: None,
                http_status: None,
                raw_body: None,
                error: Some(err.to_string()),
            };
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
            return IpLocationLookupResult {
                country: None,
                region: None,
                city: None,
                http_status: None,
                raw_body: None,
                error: Some(err.to_string()),
            };
        }
    };
    let status = response.status().as_u16();

    if !response.status().is_success() {
        warn!(
            ip = primary_ip,
            status, "ip2location lookup returned non-success status"
        );
        let body = response.text().await.unwrap_or_default();
        return IpLocationLookupResult {
            country: None,
            region: None,
            city: None,
            http_status: Some(status),
            raw_body: (!body.is_empty()).then(|| parse_json_or_string(&body)),
            error: Some(format!("HTTP {status}")),
        };
    }

    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            warn!(ip = primary_ip, error = %err, "failed to read ip2location response");
            return IpLocationLookupResult {
                country: None,
                region: None,
                city: None,
                http_status: Some(status),
                raw_body: None,
                error: Some(err.to_string()),
            };
        }
    };
    let raw_body = parse_json_or_string(&body);

    match serde_json::from_str::<Ip2LocationResponse>(&body) {
        Ok(parsed) => IpLocationLookupResult {
            country: normalize_country_name(parsed.country_name),
            region: parsed.region_name,
            city: parsed.city_name,
            http_status: Some(status),
            raw_body: Some(raw_body),
            error: None,
        },
        Err(err) => {
            warn!(ip = primary_ip, error = %err, "failed to parse ip2location response");
            IpLocationLookupResult {
                country: None,
                region: None,
                city: None,
                http_status: Some(status),
                raw_body: Some(raw_body),
                error: Some(err.to_string()),
            }
        }
    }
}

async fn probe_proxy(proxy: &ProxyRow, proxy_url: &str) -> ProxyProbeOutcome {
    let probes = [
        ("https://ipwho.is/", "ipwho"),
        ("https://httpbin.org/ip", "httpbin"),
    ];
    let mut bundle = Map::new();
    let mut attempts = Vec::new();
    bundle.insert(
        "proxy".to_string(),
        json!({
            "id": proxy.id,
            "name": proxy.name,
            "protocol": proxy.protocol,
            "host": proxy.host,
            "port": proxy.port,
        }),
    );
    let mut client_builder = wreq::Client::builder().timeout(std::time::Duration::from_secs(10));
    if let Some(proxy) = crate::claude_code_state::proxy_from_url(Some(proxy_url)) {
        client_builder = client_builder.proxy(proxy);
    }
    let client = match client_builder.build() {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "failed to build proxy test client");
            let message = normalize_proxy_test_error(&err);
            bundle.insert("attempts".to_string(), Value::Array(attempts));
            bundle.insert(
                "result".to_string(),
                json!({
                    "success": false,
                    "message": message,
                }),
            );
            return ProxyProbeOutcome {
                response: ProxyTestResponse {
                    success: false,
                    message: message.clone(),
                    latency_ms: None,
                    ip_address: None,
                    country: None,
                    region: None,
                    city: None,
                },
                http_status: None,
                log_status: proxy_log_status(&message, None),
                bundle: Value::Object(bundle),
            };
        }
    };

    let mut last_message = "proxy test failed".to_string();
    let mut last_http_status = None;
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
                last_http_status = err.status().map(|status| status.as_u16());
                attempts.push(json!({
                    "url": url,
                    "parser": parser,
                    "latency_ms": latency_ms,
                    "success": false,
                    "error": err.to_string(),
                    "normalized_error": last_message,
                }));
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            last_message = describe_probe_status(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            );
            last_http_status = Some(status);
            attempts.push(json!({
                "url": url,
                "parser": parser,
                "latency_ms": latency_ms,
                "success": false,
                "http_status": status,
                "body": (!body.is_empty()).then(|| parse_json_or_string(&body)),
            }));
            continue;
        }

        let body = match resp.text().await {
            Ok(body) => body,
            Err(err) => {
                warn!(probe_url = url, error = %err, "failed to read proxy test response");
                last_message = normalize_proxy_test_error(&err);
                last_http_status = err.status().map(|status| status.as_u16());
                attempts.push(json!({
                    "url": url,
                    "parser": parser,
                    "latency_ms": latency_ms,
                    "success": false,
                    "error": err.to_string(),
                    "normalized_error": last_message,
                }));
                continue;
            }
        };
        let raw_body = parse_json_or_string(&body);

        match parser {
            "ipwho" => match serde_json::from_str::<IpWhoResponse>(&body) {
                Ok(parsed) if parsed.success => {
                    let ip_address = parsed.ip;
                    let geo_lookup = match ip_address.as_deref() {
                        Some(ip) => Some(lookup_ip_location(ip).await),
                        None => None,
                    };
                    let country = geo_lookup
                        .as_ref()
                        .and_then(|lookup| lookup.country.clone())
                        .or_else(|| normalize_country_name(parsed.country));
                    let region = geo_lookup
                        .as_ref()
                        .and_then(|lookup| lookup.region.clone())
                        .or(parsed.region);
                    let city = geo_lookup
                        .as_ref()
                        .and_then(|lookup| lookup.city.clone())
                        .or(parsed.city);
                    attempts.push(json!({
                        "url": url,
                        "parser": parser,
                        "latency_ms": latency_ms,
                        "success": true,
                        "http_status": 200,
                        "body": raw_body,
                    }));
                    if let Some(lookup) = geo_lookup {
                        bundle.insert(
                            "location_lookup".to_string(),
                            json!({
                                "provider": "ip2location",
                                "http_status": lookup.http_status,
                                "error": lookup.error,
                                "body": lookup.raw_body,
                            }),
                        );
                    }
                    let response = ProxyTestResponse {
                        success: true,
                        message: "基础连通性正常".to_string(),
                        latency_ms: Some(latency_ms),
                        ip_address,
                        country,
                        region,
                        city,
                    };
                    bundle.insert("attempts".to_string(), Value::Array(attempts));
                    bundle.insert(
                        "result".to_string(),
                        serde_json::to_value(&response).unwrap_or(Value::Null),
                    );
                    return ProxyProbeOutcome {
                        response,
                        http_status: Some(200),
                        log_status: "ok",
                        bundle: Value::Object(bundle),
                    };
                }
                Ok(parsed) => {
                    warn!(
                        probe_url = url,
                        message = parsed.message.as_deref().unwrap_or("unknown"),
                        "probe target reported failure"
                    );
                    last_message = "探测目标返回异常".to_string();
                    last_http_status = Some(200);
                    attempts.push(json!({
                        "url": url,
                        "parser": parser,
                        "latency_ms": latency_ms,
                        "success": false,
                        "http_status": 200,
                        "body": raw_body,
                        "error": parsed.message,
                    }));
                }
                Err(err) => {
                    warn!(probe_url = url, error = %err, "failed to parse ipwho response");
                    last_message = "探测响应解析失败".to_string();
                    last_http_status = Some(200);
                    attempts.push(json!({
                        "url": url,
                        "parser": parser,
                        "latency_ms": latency_ms,
                        "success": false,
                        "http_status": 200,
                        "body": raw_body,
                        "error": err.to_string(),
                    }));
                }
            },
            "httpbin" => match serde_json::from_str::<HttpBinResponse>(&body) {
                Ok(parsed) => {
                    let ip_address = parsed.origin;
                    let geo_lookup = lookup_ip_location(&ip_address).await;
                    attempts.push(json!({
                        "url": url,
                        "parser": parser,
                        "latency_ms": latency_ms,
                        "success": true,
                        "http_status": 200,
                        "body": raw_body,
                    }));
                    bundle.insert(
                        "location_lookup".to_string(),
                        json!({
                            "provider": "ip2location",
                            "http_status": geo_lookup.http_status,
                            "error": geo_lookup.error,
                            "body": geo_lookup.raw_body,
                        }),
                    );
                    let response = ProxyTestResponse {
                        success: true,
                        message: "基础连通性正常".to_string(),
                        latency_ms: Some(latency_ms),
                        ip_address: Some(ip_address),
                        country: geo_lookup.country,
                        region: geo_lookup.region,
                        city: geo_lookup.city,
                    };
                    bundle.insert("attempts".to_string(), Value::Array(attempts));
                    bundle.insert(
                        "result".to_string(),
                        serde_json::to_value(&response).unwrap_or(Value::Null),
                    );
                    return ProxyProbeOutcome {
                        response,
                        http_status: Some(200),
                        log_status: "ok",
                        bundle: Value::Object(bundle),
                    };
                }
                Err(err) => {
                    warn!(probe_url = url, error = %err, "failed to parse httpbin response");
                    last_message = "探测响应解析失败".to_string();
                    last_http_status = Some(200);
                    attempts.push(json!({
                        "url": url,
                        "parser": parser,
                        "latency_ms": latency_ms,
                        "success": false,
                        "http_status": 200,
                        "body": raw_body,
                        "error": err.to_string(),
                    }));
                }
            },
            _ => {}
        }
    }

    let response = ProxyTestResponse {
        success: false,
        message: last_message.clone(),
        latency_ms: None,
        ip_address: None,
        country: None,
        region: None,
        city: None,
    };
    bundle.insert("attempts".to_string(), Value::Array(attempts));
    bundle.insert(
        "result".to_string(),
        serde_json::to_value(&response).unwrap_or(Value::Null),
    );
    ProxyProbeOutcome {
        response,
        http_status: last_http_status,
        log_status: proxy_log_status(&last_message, last_http_status),
        bundle: Value::Object(bundle),
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
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ProxyTestResponse>, ClewdrError> {
    let proxy = get_proxy_by_id(&state.db, id)
        .await?
        .ok_or(ClewdrError::NotFound {
            msg: "proxy not found",
        })?;
    let proxy_url = build_proxy_url(&proxy).map_err(|_| ClewdrError::BadRequest {
        msg: "Invalid proxy configuration",
    })?;
    let started_at = chrono::Utc::now();
    let outcome = probe_proxy(&proxy, &proxy_url).await;

    update_proxy_test_result(
        &state.db,
        id,
        ProxyTestResultUpdate {
            success: Some(outcome.response.success),
            latency_ms: outcome.response.latency_ms,
            message: Some(outcome.response.message.as_str()),
            ip_address: outcome.response.ip_address.as_deref(),
            country: outcome.response.country.as_deref(),
            region: outcome.response.region.as_deref(),
            city: outcome.response.city.as_deref(),
        },
    )
    .await?;

    let response_body = serde_json::to_string(&outcome.bundle).unwrap_or_else(|_| "{}".to_string());
    let ctx = BillingContext {
        db: state.db.clone(),
        user_id: None,
        api_key_id: None,
        account_id: None,
        model_raw: String::new(),
        request_id: format!("probe-proxy-{}-{}", id, uuid::Uuid::new_v4()),
        started_at,
        event_tx: state.event_tx.clone(),
    };
    persist_probe_log(
        &ctx,
        RequestType::ProbeProxy,
        outcome.log_status,
        outcome.http_status,
        &response_body,
        (!outcome.response.success).then_some(outcome.response.message.as_str()),
    )
    .await;

    Ok(Json(outcome.response))
}
