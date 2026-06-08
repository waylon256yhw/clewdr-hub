use std::net::{IpAddr, SocketAddr};

use axum::extract::ConnectInfo;
use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use ipnet::IpNet;
use tracing::warn;

use crate::config::CLEWDR_CONFIG;
use crate::db::api_key::parse_api_key;
use crate::db::queries::authenticate_api_key;
use crate::error::ClewdrError;
use crate::middleware::openai::OpenAIRequestError;
use crate::session;
use crate::state::AuthState;
use crate::types::openai::OpenAIErrorBody;

// Caps applied at parse time so an audited / curious client cannot bloat
// rows in api_keys.last_used_ip or future request_log_audits.
const MAX_XFF_RAW_BYTES: usize = 1024;
const MAX_XFF_HOPS: usize = 16;
const MAX_IP_TOKEN_BYTES: usize = 64;

/// Source of the resolved client IP. Lets administrators tell at a glance
/// whether the value came from the TCP peer (most trustworthy) or from a
/// forwarded header (only trustworthy when the peer itself is trusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIpSource {
    /// Peer was not in `trusted_proxies` — header values were ignored and
    /// the TCP peer address was used as-is.
    Peer,
    /// Peer was trusted; the leftmost non-trusted hop in `X-Forwarded-For`
    /// (scanned right-to-left) was used.
    Xff,
    /// Peer was trusted but XFF was absent/all-trusted; `X-Real-IP` was
    /// taken as the client IP.
    Xri,
}

impl ClientIpSource {
    // Consumed by the audit pipeline (Step B) and by frontend rendering
    // to badge the source of the IP. Marked as allowed-dead here because
    // Step A lands ahead of the consumer; once Step B ships this can be
    // removed.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Xff => "xff",
            Self::Xri => "xri",
        }
    }
}

/// Result of resolving the real client IP from a request.
///
/// Independent of audit configuration: every authenticated request goes
/// through this so `last_used_ip` is always trustworthy. The audit pipeline
/// (Step B) reads the same struct and persists `peer_ip` / `source` /
/// `forwarded_chain` alongside `client_ip`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `source`, `peer_ip`, `forwarded_chain` are consumed in Step B (audit pipeline).
pub struct ResolvedClientIp {
    pub client_ip: String,
    pub source: ClientIpSource,
    pub peer_ip: String,
    /// Truncated raw `X-Forwarded-For` header value (if present), capped at
    /// `MAX_XFF_RAW_BYTES`. Stored for audit so reviewers can see the
    /// proxy chain claim even after we picked one element from it.
    pub forwarded_chain: Option<String>,
}

fn extract_key_from_headers(parts: &Parts) -> Option<String> {
    if let Some(key) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.to_string());
    }
    if let Some(auth) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        && let Some(token) = auth.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    None
}

fn ip_is_trusted(ip: &IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|net| net.contains(ip))
}

fn truncate_token(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= MAX_IP_TOKEN_BYTES {
        trimmed.to_string()
    } else {
        trimmed[..MAX_IP_TOKEN_BYTES].to_string()
    }
}

fn truncate_chain(s: &str) -> String {
    if s.len() <= MAX_XFF_RAW_BYTES {
        s.to_string()
    } else {
        s[..MAX_XFF_RAW_BYTES].to_string()
    }
}

/// Resolve the real client IP, applying the trust policy in `trusted`.
///
/// Algorithm:
/// 1. Pull TCP peer IP from `ConnectInfo<SocketAddr>` (injected by
///    `into_make_service_with_connect_info` in `main.rs`).
/// 2. If peer is **not** trusted → return peer IP, ignore forwarded headers.
///    This is what stops a direct caller from spoofing their IP.
/// 3. If peer is trusted → scan `X-Forwarded-For` right-to-left, skip
///    trusted hops, return first untrusted hop as `Xff`.
/// 4. If XFF is empty or fully trusted → fall back to `X-Real-IP` as `Xri`.
/// 5. Otherwise return peer (the last trusted hop is the client too).
pub fn resolve_client_ip(parts: &Parts, trusted: &[IpNet]) -> ResolvedClientIp {
    let peer_ip = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "0.0.0.0".to_string());

    let forwarded_chain_raw = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(truncate_chain);

    let peer_parsed: Option<IpAddr> = peer_ip.parse().ok();
    let peer_trusted = peer_parsed
        .as_ref()
        .map(|ip| ip_is_trusted(ip, trusted))
        .unwrap_or(false);

    if !peer_trusted {
        return ResolvedClientIp {
            client_ip: peer_ip.clone(),
            source: ClientIpSource::Peer,
            peer_ip,
            forwarded_chain: forwarded_chain_raw,
        };
    }

    // Peer is trusted: walk XFF right-to-left and return first non-trusted
    // hop. The cap is applied **from the right** so attacker-controlled
    // prefix hops can't push real trusted-proxy hops out of the scan
    // window. Concretely: if a client sends
    //     X-Forwarded-For: spoof1, spoof2, ..., spoof_2000
    // and our infra appends `real_proxy` on the right, taking `MAX` from
    // the LEFT would chop `real_proxy` off and let us return whatever
    // attacker put at position `MAX-1`. Taking from the right preserves
    // the chain segment we actually appended.
    if let Some(ref raw) = forwarded_chain_raw {
        let all_hops: Vec<&str> = raw.split(',').collect();
        let scan_start = all_hops.len().saturating_sub(MAX_XFF_HOPS);
        // Walk rightmost-first; bail out at scan_start (left of which is
        // attacker noise we deliberately do not trust).
        for hop in all_hops[scan_start..].iter().rev() {
            let token = truncate_token(hop);
            if token.is_empty() {
                continue;
            }
            match token.parse::<IpAddr>() {
                Ok(parsed) => {
                    if !ip_is_trusted(&parsed, trusted) {
                        return ResolvedClientIp {
                            client_ip: token,
                            source: ClientIpSource::Xff,
                            peer_ip,
                            forwarded_chain: forwarded_chain_raw,
                        };
                    }
                }
                Err(_) => {
                    // Malformed token — keep scanning rather than trusting it.
                    continue;
                }
            }
        }
    }

    // No untrusted hop in XFF — try X-Real-IP next.
    if let Some(xri) = parts
        .headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(truncate_token)
        .filter(|s| !s.is_empty())
    {
        return ResolvedClientIp {
            client_ip: xri,
            source: ClientIpSource::Xri,
            peer_ip,
            forwarded_chain: forwarded_chain_raw,
        };
    }

    // Fully degenerate (trusted peer, no headers) — treat peer as client.
    ResolvedClientIp {
        client_ip: peer_ip.clone(),
        source: ClientIpSource::Peer,
        peer_ip,
        forwarded_chain: forwarded_chain_raw,
    }
}

pub struct RequireFlexibleAuth;

impl<S> FromRequestParts<S> for RequireFlexibleAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        let key = extract_key_from_headers(parts).ok_or(ClewdrError::InvalidAuth)?;

        if let Some((lookup, hash)) = parse_api_key(&key) {
            match authenticate_api_key(&auth_state.db, &lookup, &hash).await {
                Ok(Some(authed_user)) => {
                    let cfg = CLEWDR_CONFIG.load();
                    let resolved = resolve_client_ip(parts, &cfg.trusted_proxies);
                    let db = auth_state.db.clone();
                    let ak_id = authed_user.api_key_id;
                    let uid = authed_user.user_id;
                    let client_ip = resolved.client_ip.clone();
                    tokio::spawn(async move {
                        if let Some(ak_id) = ak_id {
                            let _ = crate::db::queries::touch_api_key(
                                &db,
                                ak_id,
                                Some(client_ip.as_str()),
                            )
                            .await;
                        }
                        let _ = crate::db::queries::touch_user(&db, uid).await;
                    });
                    // Stash the resolved IP info for downstream consumers
                    // (audit pipeline in Step B reads this). Always inserted,
                    // not conditional on audit being enabled — keeps the
                    // hot path branch-free and lets debugging tools peek.
                    parts.extensions.insert(resolved);
                    parts.extensions.insert(authed_user);
                    return Ok(Self);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("DB error during API key auth: {e}");
                }
            }
        }

        Err(ClewdrError::InvalidAuth)
    }
}

/// `RequireFlexibleAuth` variant whose rejection serializes in the OpenAI
/// error envelope. Used by the `/v1/chat/completions` route so missing or
/// invalid API keys do not leak the Anthropic-shape error body that
/// [`ClewdrError`]'s default `IntoResponse` would emit.
pub struct RequireFlexibleAuthOpenAI;

impl<S> FromRequestParts<S> for RequireFlexibleAuthOpenAI
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = OpenAIRequestError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match RequireFlexibleAuth::from_request_parts(parts, state).await {
            Ok(_) => Ok(Self),
            Err(err) => Err(OpenAIRequestError {
                status: StatusCode::UNAUTHORIZED,
                body: OpenAIErrorBody {
                    message: err.to_string(),
                    // OpenAI historically emits invalid_request_error for both
                    // missing and incorrect API keys at the auth boundary;
                    // client SDKs key off this value.
                    kind: "invalid_request_error".to_string(),
                    code: None,
                    param: None,
                },
            }),
        }
    }
}

pub struct RequireAdminAuth;

impl<S> FromRequestParts<S> for RequireAdminAuth
where
    AuthState: FromRef<S>,
    S: Sync + Send,
{
    type Rejection = ClewdrError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);

        let cookie_header = parts
            .headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let cookie_value =
            session::extract_session_cookie(cookie_header).ok_or(ClewdrError::InvalidAuth)?;

        let claims = session::validate_session_cookie(&auth_state.session_secret, cookie_value)
            .ok_or(ClewdrError::InvalidAuth)?;

        let row: Option<(i64, String, String, i32, Option<String>, i64)> = sqlx::query_as(
            "SELECT u.id, u.username, u.role, u.session_version, u.disabled_at, u.policy_id
             FROM users u WHERE u.id = ?1",
        )
        .bind(claims.user_id)
        .fetch_optional(&auth_state.db)
        .await
        .map_err(|e| {
            warn!("DB error during cookie auth: {e}");
            ClewdrError::InvalidAuth
        })?;

        let Some((user_id, username, role, session_version, disabled_at, policy_id)) = row else {
            return Err(ClewdrError::InvalidAuth);
        };

        if disabled_at.is_some() || role != "admin" || session_version != claims.session_version {
            return Err(ClewdrError::InvalidAuth);
        }

        let Some((max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)) = sqlx::query_as::<_, (i32, i32, i64, i64)>(
            "SELECT max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd FROM policies WHERE id = ?1",
        )
        .bind(policy_id)
        .fetch_optional(&auth_state.db)
        .await
        .map_err(|e| {
            warn!("DB error loading policy: {e}");
            ClewdrError::InvalidAuth
        })? else {
            return Err(ClewdrError::InvalidAuth);
        };

        parts
            .extensions
            .insert(crate::db::models::AuthenticatedUser {
                user_id,
                username,
                role,
                api_key_id: None,
                policy_id,
                max_concurrent,
                rpm_limit,
                weekly_budget_nanousd,
                monthly_budget_nanousd,
                bound_account_ids: Vec::new(),
                auto_cache_enabled: false,
            });

        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn trusted() -> Vec<IpNet> {
        ["127.0.0.0/8", "::1/128", "172.16.0.0/12"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect()
    }

    /// Build a request Parts with ConnectInfo set and the given headers.
    fn make_parts(peer: &str, xff: Option<&str>, xri: Option<&str>) -> Parts {
        let mut req = Request::builder().uri("/").body(()).unwrap();
        let peer_ip: IpAddr = peer.parse().unwrap();
        let sock = SocketAddr::new(peer_ip, 12345);
        req.extensions_mut().insert(ConnectInfo(sock));
        if let Some(v) = xff {
            req.headers_mut()
                .insert("x-forwarded-for", v.parse().unwrap());
        }
        if let Some(v) = xri {
            req.headers_mut().insert("x-real-ip", v.parse().unwrap());
        }
        let (parts, _) = req.into_parts();
        parts
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        // Public-IP caller cannot spoof their address via headers.
        let parts = make_parts("8.8.8.8", Some("1.2.3.4"), Some("5.6.7.8"));
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "8.8.8.8");
        assert_eq!(r.source, ClientIpSource::Peer);
        assert_eq!(r.peer_ip, "8.8.8.8");
        // We still record the chain claim for audit visibility, but did not act on it.
        assert_eq!(r.forwarded_chain.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn trusted_peer_takes_xff_single_hop() {
        let parts = make_parts("127.0.0.1", Some("1.2.3.4"), None);
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "1.2.3.4");
        assert_eq!(r.source, ClientIpSource::Xff);
        assert_eq!(r.peer_ip, "127.0.0.1");
    }

    #[test]
    fn trusted_peer_walks_xff_right_to_left_skipping_trusted() {
        // Chain: real_client, edge_proxy(public), internal_lb(172.16), app_proxy(127)
        // Scanning right-to-left, skip 127.0.0.1 then 172.16.0.5, return 9.9.9.9.
        let parts = make_parts("127.0.0.1", Some("1.2.3.4, 9.9.9.9, 172.16.0.5"), None);
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "9.9.9.9");
        assert_eq!(r.source, ClientIpSource::Xff);
    }

    #[test]
    fn trusted_peer_falls_back_to_xri_when_xff_empty() {
        let parts = make_parts("127.0.0.1", None, Some("4.3.2.1"));
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "4.3.2.1");
        assert_eq!(r.source, ClientIpSource::Xri);
    }

    #[test]
    fn trusted_peer_no_headers_returns_peer() {
        let parts = make_parts("127.0.0.1", None, None);
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "127.0.0.1");
        assert_eq!(r.source, ClientIpSource::Peer);
        assert!(r.forwarded_chain.is_none());
    }

    #[test]
    fn trusted_peer_xff_all_trusted_falls_through_to_xri() {
        // All XFF hops are trusted (private + loopback), should fall back to XRI.
        let parts = make_parts("127.0.0.1", Some("172.16.0.1, 127.0.0.1"), Some("7.7.7.7"));
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "7.7.7.7");
        assert_eq!(r.source, ClientIpSource::Xri);
    }

    #[test]
    fn malformed_xff_token_is_skipped_not_trusted() {
        // Garbage in middle hop must not crash and must not be used as client.
        let parts = make_parts("127.0.0.1", Some("1.1.1.1, garbage, 127.0.0.1"), None);
        let r = resolve_client_ip(&parts, &trusted());
        // Right-to-left: 127.0.0.1 trusted (skip), garbage (skip), 1.1.1.1 returned.
        assert_eq!(r.client_ip, "1.1.1.1");
        assert_eq!(r.source, ClientIpSource::Xff);
    }

    #[test]
    fn empty_trusted_list_means_always_use_peer() {
        // Lockdown mode: don't trust any header chain.
        let empty: Vec<IpNet> = Vec::new();
        let parts = make_parts("127.0.0.1", Some("1.2.3.4"), Some("5.6.7.8"));
        let r = resolve_client_ip(&parts, &empty);
        assert_eq!(r.client_ip, "127.0.0.1");
        assert_eq!(r.source, ClientIpSource::Peer);
    }

    #[test]
    fn xff_hop_cap_keeps_rightmost_hops_blocks_prefix_spoof() {
        // Attacker prepends a huge number of spoofed hops, then real
        // proxy appends its own IP on the right. The hop cap must
        // preserve the RIGHTMOST window so the attacker's claims at
        // positions 0..many don't survive into the scan.
        let mut chain = String::new();
        for _ in 0..(MAX_XFF_HOPS * 4) {
            chain.push_str("9.9.9.9,");
        }
        // Real trusted-proxy tail: 172.16.0.5 (trusted) appended last.
        chain.push_str("172.16.0.5");

        let parts = make_parts("127.0.0.1", Some(&chain), None);
        let r = resolve_client_ip(&parts, &trusted());

        // Within the rightmost MAX_XFF_HOPS, all entries are either
        // trusted (172.16.0.5) or 9.9.9.9 spoof — but the rightmost
        // untrusted hop inside the scan window is still 9.9.9.9 from
        // the attacker's tail. That's expected: we cannot magically
        // discover where the attacker's prefix ends. The point of the
        // cap is *not* to defeat XFF spoofing (only `trusted_proxies`
        // does that — see `untrusted_peer_ignores_forwarded_headers`),
        // but to ensure the scan window is anchored on the right so
        // legitimate trusted-tail chains never get chopped.
        //
        // The real win is the regression in
        // `legitimate_long_chain_keeps_trusted_tail_visible` below.
        assert_eq!(r.source, ClientIpSource::Xff);
        assert_eq!(r.client_ip, "9.9.9.9");
    }

    #[test]
    fn legitimate_long_chain_keeps_trusted_tail_visible() {
        // Real client, then MAX_XFF_HOPS trusted proxies appended.
        // With LEFT-anchored truncation, the trusted tail would be
        // dropped and the leftmost trusted hop would be returned as
        // client. With RIGHT-anchored truncation we instead exhaust
        // the scan window seeing only trusted hops and correctly fall
        // through to XRI / peer fallback.
        let mut chain = String::from("8.8.8.8"); // real client at position 0
        for _ in 0..MAX_XFF_HOPS {
            chain.push_str(",127.0.0.1");
        }
        // Total = MAX_XFF_HOPS + 1, scan window covers only the trusted tail.

        let parts = make_parts("127.0.0.1", Some(&chain), Some("4.3.2.1"));
        let r = resolve_client_ip(&parts, &trusted());
        // Scan window contains only 127.0.0.1 hops (trusted) → fall
        // through to X-Real-IP. Crucially we do NOT return 8.8.8.8,
        // because we cannot verify whether positions before the window
        // are legitimate or attacker-injected.
        assert_eq!(r.client_ip, "4.3.2.1");
        assert_eq!(r.source, ClientIpSource::Xri);
    }

    #[test]
    fn xff_chain_is_truncated_to_max_bytes() {
        // Build an XFF longer than MAX_XFF_RAW_BYTES (1024).
        let huge = (0..200)
            .map(|i| format!("10.0.0.{},", i % 256))
            .collect::<String>();
        let parts = make_parts("127.0.0.1", Some(&huge), None);
        let r = resolve_client_ip(&parts, &trusted());
        assert!(r.forwarded_chain.as_ref().unwrap().len() <= MAX_XFF_RAW_BYTES);
    }

    #[test]
    fn ipv6_loopback_peer_is_trusted() {
        let parts = make_parts("::1", Some("2.2.2.2"), None);
        let r = resolve_client_ip(&parts, &trusted());
        assert_eq!(r.client_ip, "2.2.2.2");
        assert_eq!(r.source, ClientIpSource::Xff);
    }

    #[test]
    fn ip_token_too_long_is_truncated() {
        let mut long_token = "9".repeat(MAX_IP_TOKEN_BYTES * 2);
        long_token.push_str(".1.1.1");
        let parts = make_parts("127.0.0.1", Some(&long_token), None);
        // Truncated token is no longer a valid IP, so it gets skipped — not used.
        let r = resolve_client_ip(&parts, &trusted());
        // No valid XFF hop, no XRI → falls through to peer.
        assert_eq!(r.client_ip, "127.0.0.1");
        assert_eq!(r.source, ClientIpSource::Peer);
    }

    // Smoke check that the SocketAddrV4 type compiles in test scope.
    #[allow(dead_code)]
    fn _unused_compile_check() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0)
    }
}
