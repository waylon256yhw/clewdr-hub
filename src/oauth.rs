use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

use http::header::USER_AGENT;
use oauth2::{
    AuthUrl, Client, ClientId, CsrfToken, EndpointNotSet, EndpointSet, PkceCodeChallenge,
    RedirectUrl, Scope, StandardRevocableToken, TokenUrl,
    basic::{
        BasicErrorResponse, BasicRevocationErrorResponse, BasicTokenIntrospectionResponse,
        BasicTokenResponse,
    },
};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt};
use tokio::sync::Mutex;
use tracing::error;
use url::{Url, form_urlencoded};

use crate::{
    config::{CC_REDIRECT_URI, CC_TOKEN_URL, CLEWDR_CONFIG, RuntimeStateParams, TokenInfo},
    error::{CheckClaudeErr, ClewdrError, UnexpectedNoneSnafu, WreqSnafu},
    stealth,
};

const CLAUDE_AUTH_URL: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_API_VERSION: &str = "2023-06-01";
const CLAUDE_BETA_OAUTH: &str = "oauth-2025-04-20";
const ADMIN_OAUTH_TTL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_SCOPE: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
];

type ClaudeOauthClient = Client<
    BasicErrorResponse,
    BasicTokenResponse,
    BasicTokenIntrospectionResponse,
    StandardRevocableToken,
    BasicRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
>;

#[derive(Clone)]
struct AdminOAuthState {
    verifier: String,
    redirect_uri: String,
    created_at: Instant,
}

static ADMIN_OAUTH_STATES: LazyLock<Mutex<HashMap<String, AdminOAuthState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(serde::Serialize)]
pub struct AdminOAuthStartResponse {
    pub auth_url: String,
    pub state: String,
    pub redirect_uri: String,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthProfile {
    #[serde(default)]
    account: OAuthProfileAccount,
    #[serde(default)]
    organization: OAuthProfileOrg,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthProfileAccount {
    #[serde(alias = "email_address")]
    email: Option<String>,
    #[serde(default)]
    has_claude_max: bool,
    #[serde(default)]
    has_claude_pro: bool,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthProfileOrg {
    uuid: Option<String>,
    organization_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    five_hour: Option<UsageBucketPayload>,
    #[serde(default)]
    seven_day: Option<UsageBucketPayload>,
    #[serde(default)]
    seven_day_sonnet: Option<UsageBucketPayload>,
    #[serde(default)]
    seven_day_opus: Option<UsageBucketPayload>,
}

#[derive(Debug, Default, Deserialize)]
struct UsageBucketPayload {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthTokenOrganization {
    uuid: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthTokenErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    organization: OAuthTokenOrganization,
    #[serde(default, alias = "organizationUuid")]
    organization_uuid: Option<String>,
    #[serde(flatten)]
    error: OAuthTokenErrorBody,
}

#[derive(Debug, Clone)]
pub struct OAuthAccountSnapshot {
    pub email: Option<String>,
    pub account_type: Option<String>,
    pub organization_uuid: String,
    pub runtime: RuntimeStateParams,
}

#[derive(Debug, Clone)]
pub struct OAuthExchangeResult {
    pub token: TokenInfo,
    pub snapshot: OAuthAccountSnapshot,
}

fn setup_client(redirect_uri: &str) -> Result<ClaudeOauthClient, ClewdrError> {
    Ok(
        oauth2::basic::BasicClient::new(ClientId::new(CLEWDR_CONFIG.load().cc_client_id()))
            .set_auth_type(oauth2::AuthType::RequestBody)
            .set_auth_uri(AuthUrl::new(CLAUDE_AUTH_URL.to_string()).map_err(|_| {
                ClewdrError::UnexpectedNone {
                    msg: "Invalid Claude auth URL",
                }
            })?)
            .set_redirect_uri(RedirectUrl::new(redirect_uri.to_string()).map_err(|_| {
                ClewdrError::UnexpectedNone {
                    msg: "Invalid redirect URI",
                }
            })?)
            .set_token_uri(TokenUrl::new(CC_TOKEN_URL.into()).map_err(|_| {
                ClewdrError::UnexpectedNone {
                    msg: "Invalid token URI",
                }
            })?),
    )
}

fn cleanup_expired_states(states: &mut HashMap<String, AdminOAuthState>) {
    states.retain(|_, item| item.created_at.elapsed() < ADMIN_OAUTH_TTL);
}

fn oauth_client() -> wreq::Client {
    let profile = stealth::global_profile().load();
    let mut builder = wreq::Client::builder();
    if let Some(proxy) = profile.proxy.as_deref().filter(|s| !s.trim().is_empty()) {
        match wreq::Proxy::all(proxy) {
            Ok(proxy) => {
                builder = builder.proxy(proxy);
            }
            Err(err) => {
                error!("Failed to parse proxy from settings for OAuth client: {err}");
            }
        }
    }
    builder.build().unwrap_or_else(|err| {
        error!("Failed to build OAuth client: {err}");
        wreq::Client::new()
    })
}

fn oauth_form_body(params: &[(&str, &str)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

async fn send_oauth_token_request(body: String) -> Result<OAuthTokenResponse, ClewdrError> {
    let response = oauth_client()
        .post(CC_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json, text/plain, */*")
        .header("origin", "https://claude.ai")
        .header("referer", "https://claude.ai/")
        .header("anthropic-version", CLAUDE_API_VERSION)
        .header("anthropic-beta", CLAUDE_BETA_OAUTH)
        .header(USER_AGENT, stealth::global_profile().load().user_agent())
        .body(body)
        .send()
        .await
        .context(WreqSnafu {
            msg: "Failed to request OAuth token",
        })?;

    let status = response.status();
    let bytes = response.bytes().await.context(WreqSnafu {
        msg: "Failed to read OAuth token response",
    })?;
    let body_text = String::from_utf8_lossy(&bytes).to_string();

    if !status.is_success() {
        let parsed = serde_json::from_slice::<OAuthTokenErrorBody>(&bytes).ok();
        let detail = parsed
            .and_then(|value| {
                value
                    .error_description
                    .or(value.error)
                    .map(|msg| msg.trim().to_string())
            })
            .filter(|msg| !msg.is_empty())
            .unwrap_or(body_text);
        return Err(ClewdrError::Whatever {
            message: format!("OAuth token request failed with status {status}: {detail}"),
            source: None,
        });
    }

    serde_json::from_slice::<OAuthTokenResponse>(&bytes).map_err(|source| ClewdrError::Whatever {
        message: format!("OAuth token response was not valid JSON: {body_text}"),
        source: Some(Box::new(source)),
    })
}

async fn exchange_oauth_code(
    code: &str,
    state: Option<&str>,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthTokenResponse, ClewdrError> {
    let client_id = CLEWDR_CONFIG.load().cc_client_id();
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if let Some(state) = state.filter(|s| !s.trim().is_empty()) {
        params.push(("state", state));
    }
    send_oauth_token_request(oauth_form_body(&params)).await
}

async fn refresh_oauth_access_token(
    refresh_token: &str,
) -> Result<OAuthTokenResponse, ClewdrError> {
    let client_id = CLEWDR_CONFIG.load().cc_client_id();
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("refresh_token", refresh_token),
    ];
    send_oauth_token_request(oauth_form_body(&params)).await
}

fn token_response_access_token(
    token: &OAuthTokenResponse,
    fallback_msg: &'static str,
) -> Result<String, ClewdrError> {
    if let Some(access_token) = token.access_token.clone() {
        return Ok(access_token);
    }

    if let Some(detail) = token
        .error
        .error_description
        .clone()
        .or(token.error.error.clone())
        .filter(|msg| !msg.trim().is_empty())
    {
        return Err(ClewdrError::Whatever {
            message: format!("OAuth token response missing access_token: {detail}"),
            source: None,
        });
    }

    Err(ClewdrError::UnexpectedNone { msg: fallback_msg })
}

pub async fn start_admin_oauth_flow(
    redirect_uri: Option<String>,
) -> Result<AdminOAuthStartResponse, ClewdrError> {
    let redirect_uri = redirect_uri
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| CC_REDIRECT_URI.to_string());
    let client = setup_client(&redirect_uri)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, state) = DEFAULT_SCOPE
        .iter()
        .fold(
            client
                .authorize_url(|| CsrfToken::new_random_len(32))
                .add_extra_param("code", "true")
                .set_pkce_challenge(challenge),
            |req, scope| req.add_scope(Scope::new((*scope).to_string())),
        )
        .url();

    let state_id = state.secret().to_string();
    let verifier_secret = verifier.secret().to_string();
    let mut states = ADMIN_OAUTH_STATES.lock().await;
    cleanup_expired_states(&mut states);
    // Admin UI only needs the latest pending OAuth flow. Replacing older
    // pending states avoids ambiguous code-only submissions after repeated
    // "generate auth URL" clicks.
    states.clear();
    states.insert(
        state_id.clone(),
        AdminOAuthState {
            verifier: verifier_secret,
            redirect_uri: redirect_uri.clone(),
            created_at: Instant::now(),
        },
    );

    Ok(AdminOAuthStartResponse {
        auth_url: auth_url.to_string(),
        state: state_id,
        redirect_uri,
    })
}

fn parse_callback_input(
    input: &str,
    fallback_state: Option<&str>,
) -> Result<(String, Option<String>), ClewdrError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "oauth callback input is required",
        });
    }

    if let Ok(url) = Url::parse(trimmed) {
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        let code = pairs.get("code").context(UnexpectedNoneSnafu {
            msg: "No code found in callback URL",
        })?;
        let state = pairs
            .get("state")
            .map(|s| s.to_string())
            .or_else(|| fallback_state.map(|s| s.to_string()));
        return Ok((code.to_string(), state));
    }

    if let Some((code, state)) = trimmed.split_once('#') {
        let code = code.trim();
        let state = state.trim();
        if !code.is_empty() {
            return Ok((
                code.to_string(),
                (!state.is_empty())
                    .then(|| state.to_string())
                    .or_else(|| fallback_state.map(|s| s.to_string())),
            ));
        }
    }

    Ok((trimmed.to_string(), fallback_state.map(|s| s.to_string())))
}

async fn take_admin_oauth_state(state: Option<&str>) -> Result<AdminOAuthState, ClewdrError> {
    let mut states = ADMIN_OAUTH_STATES.lock().await;
    cleanup_expired_states(&mut states);

    if let Some(state_id) = state {
        return states.remove(state_id).ok_or(ClewdrError::BadRequest {
            msg: "OAuth state not found or expired",
        });
    }

    if states.len() == 1 {
        let key = states.keys().next().cloned().context(UnexpectedNoneSnafu {
            msg: "OAuth state missing",
        })?;
        return states.remove(&key).ok_or(ClewdrError::BadRequest {
            msg: "OAuth state not found or expired",
        });
    }

    if states.is_empty() {
        return Err(ClewdrError::BadRequest {
            msg: "OAuth state not found or expired; generate a new authorization URL",
        });
    }

    Err(ClewdrError::BadRequest {
        msg: "OAuth state is required when multiple pending authorizations exist",
    })
}

fn parse_account_type(profile: &OAuthProfile) -> Option<String> {
    if let Some(kind) = profile.organization.organization_type.as_deref() {
        return Some(kind.to_string());
    }
    if profile.account.has_claude_max {
        return Some("max".to_string());
    }
    if profile.account.has_claude_pro {
        return Some("pro".to_string());
    }
    None
}

pub async fn fetch_oauth_snapshot(access_token: &str) -> Result<OAuthAccountSnapshot, ClewdrError> {
    let (snapshot, _, _) = fetch_oauth_snapshot_raw(access_token).await?;
    Ok(snapshot)
}

/// Like [`fetch_oauth_snapshot`] but also returns the raw profile and usage JSON bodies,
/// so manually-triggered probes can persist them for debugging.
pub async fn fetch_oauth_snapshot_raw(
    access_token: &str,
) -> Result<(OAuthAccountSnapshot, serde_json::Value, serde_json::Value), ClewdrError> {
    let client = oauth_client();
    let (profile, profile_raw) = fetch_oauth_profile(&client, access_token).await?;
    let (usage, usage_raw) = fetch_oauth_usage(&client, access_token).await?;

    let parse_window = |bucket: Option<UsageBucketPayload>| -> (Option<i64>, Option<f64>) {
        let Some(bucket) = bucket else {
            return (None, None);
        };
        let resets_at = bucket
            .resets_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp());
        (resets_at, bucket.utilization)
    };

    let (session_resets_at, session_utilization) = parse_window(usage.five_hour);
    let (weekly_resets_at, weekly_utilization) = parse_window(usage.seven_day);
    let (weekly_sonnet_resets_at, weekly_sonnet_utilization) = parse_window(usage.seven_day_sonnet);
    let (weekly_opus_resets_at, weekly_opus_utilization) = parse_window(usage.seven_day_opus);

    let snapshot = OAuthAccountSnapshot {
        email: profile.account.email.clone(),
        account_type: parse_account_type(&profile),
        organization_uuid: profile
            .organization
            .uuid
            .clone()
            .context(UnexpectedNoneSnafu {
                msg: "OAuth profile missing organization UUID",
            })?,
        runtime: RuntimeStateParams {
            reset_time: [
                (session_utilization, session_resets_at),
                (weekly_utilization, weekly_resets_at),
            ]
            .into_iter()
            .filter(|(util, ts)| *util == Some(100.0) && ts.is_some())
            .map(|(_, ts)| ts.unwrap())
            .max(),
            supports_claude_1m_sonnet: Some(true),
            supports_claude_1m_opus: Some(true),
            count_tokens_allowed: None,
            session_resets_at,
            weekly_resets_at,
            weekly_sonnet_resets_at,
            weekly_opus_resets_at,
            resets_last_checked_at: Some(chrono::Utc::now().timestamp()),
            session_has_reset: Some(session_resets_at.is_some()),
            weekly_has_reset: Some(weekly_resets_at.is_some()),
            weekly_sonnet_has_reset: Some(weekly_sonnet_resets_at.is_some()),
            weekly_opus_has_reset: Some(weekly_opus_resets_at.is_some()),
            session_utilization,
            weekly_utilization,
            weekly_sonnet_utilization,
            weekly_opus_utilization,
            buckets: Default::default(),
        },
    };
    Ok((snapshot, profile_raw, usage_raw))
}

pub async fn exchange_admin_oauth_callback(
    input: &str,
    fallback_state: Option<&str>,
) -> Result<OAuthExchangeResult, ClewdrError> {
    let (code, state) = parse_callback_input(input, fallback_state)?;
    let stored = take_admin_oauth_state(state.as_deref()).await?;
    let token = exchange_oauth_code(
        &code,
        state.as_deref(),
        &stored.redirect_uri,
        &stored.verifier,
    )
    .await?;
    let access_token =
        token_response_access_token(&token, "OAuth token response missing access_token")?;
    let snapshot = fetch_oauth_snapshot(&access_token).await?;
    let organization_uuid = token
        .organization
        .uuid
        .clone()
        .or(token.organization_uuid.clone())
        .unwrap_or_else(|| snapshot.organization_uuid.clone());
    Ok(OAuthExchangeResult {
        token: TokenInfo::from_parts(
            access_token,
            token.refresh_token.unwrap_or_default(),
            Duration::from_secs(token.expires_in.unwrap_or_default()),
            organization_uuid,
        ),
        snapshot,
    })
}

pub async fn refresh_oauth_token(token: &TokenInfo) -> Result<OAuthExchangeResult, ClewdrError> {
    let (result, _, _) = refresh_oauth_token_with_raw(token).await?;
    Ok(result)
}

/// Like [`refresh_oauth_token`] but also returns the raw profile and usage JSON bodies,
/// so manually-triggered probes can persist them for debugging.
pub async fn refresh_oauth_token_with_raw(
    token: &TokenInfo,
) -> Result<(OAuthExchangeResult, serde_json::Value, serde_json::Value), ClewdrError> {
    let raw = refresh_oauth_access_token(&token.refresh_token).await?;
    let access_token =
        token_response_access_token(&raw, "OAuth refresh response missing access_token")?;
    let (snapshot, profile_raw, usage_raw) = fetch_oauth_snapshot_raw(&access_token).await?;
    let organization_uuid = raw
        .organization
        .uuid
        .clone()
        .or(raw.organization_uuid.clone())
        .unwrap_or_else(|| token.organization.uuid.clone());
    Ok((
        OAuthExchangeResult {
            token: TokenInfo::from_parts(
                access_token,
                raw.refresh_token
                    .unwrap_or_else(|| token.refresh_token.clone()),
                Duration::from_secs(raw.expires_in.unwrap_or_default()),
                organization_uuid,
            ),
            snapshot,
        },
        profile_raw,
        usage_raw,
    ))
}

async fn fetch_oauth_profile(
    client: &wreq::Client,
    access_token: &str,
) -> Result<(OAuthProfile, serde_json::Value), ClewdrError> {
    let raw: serde_json::Value = client
        .request(
            wreq::Method::GET,
            "https://api.anthropic.com/api/oauth/profile",
        )
        .bearer_auth(access_token)
        .header("accept", "application/json")
        .header("anthropic-beta", CLAUDE_BETA_OAUTH)
        .header("anthropic-version", CLAUDE_API_VERSION)
        .header(USER_AGENT, stealth::global_profile().load().user_agent())
        .send()
        .await
        .context(WreqSnafu {
            msg: "Failed to fetch OAuth profile",
        })?
        .check_claude()
        .await?
        .json::<serde_json::Value>()
        .await
        .context(WreqSnafu {
            msg: "Failed to parse OAuth profile",
        })?;
    let parsed: OAuthProfile =
        serde_json::from_value(raw.clone()).map_err(|source| ClewdrError::Whatever {
            message: format!("OAuth profile payload was not valid: {source}"),
            source: Some(Box::new(source)),
        })?;
    Ok((parsed, raw))
}

async fn fetch_oauth_usage(
    client: &wreq::Client,
    access_token: &str,
) -> Result<(UsagePayload, serde_json::Value), ClewdrError> {
    let raw: serde_json::Value = client
        .request(
            wreq::Method::GET,
            "https://api.anthropic.com/api/oauth/usage",
        )
        .bearer_auth(access_token)
        .header("accept", "application/json")
        .header("anthropic-beta", CLAUDE_BETA_OAUTH)
        .header("anthropic-version", CLAUDE_API_VERSION)
        .header(USER_AGENT, stealth::global_profile().load().user_agent())
        .send()
        .await
        .context(WreqSnafu {
            msg: "Failed to fetch OAuth usage",
        })?
        .check_claude()
        .await?
        .json::<serde_json::Value>()
        .await
        .context(WreqSnafu {
            msg: "Failed to parse OAuth usage",
        })?;
    let parsed: UsagePayload =
        serde_json::from_value(raw.clone()).map_err(|source| ClewdrError::Whatever {
            message: format!("OAuth usage payload was not valid: {source}"),
            source: Some(Box::new(source)),
        })?;
    Ok((parsed, raw))
}
