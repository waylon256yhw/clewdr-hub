use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const COOKIE_NAME: &str = "clewdr_session";
const DEFAULT_TTL: u64 = 86400; // 24h

pub struct SessionClaims {
    pub user_id: i64,
    pub session_version: i32,
}

pub fn create_session_cookie(
    secret: &[u8; 32],
    user_id: i64,
    session_version: i32,
    ttl_secs: Option<u64>,
) -> String {
    let ttl = ttl_secs.unwrap_or(DEFAULT_TTL);
    let expires = now_unix() + ttl;
    let payload = format!("{user_id}.{session_version}.{expires}");
    let sig = sign(secret, &payload);
    format!("{payload}.{sig}")
}

pub fn validate_session_cookie(secret: &[u8; 32], value: &str) -> Option<SessionClaims> {
    let parts: Vec<&str> = value.rsplitn(2, '.').collect();
    if parts.len() != 2 {
        return None;
    }
    let (sig_hex, payload) = (parts[0], parts[1]);

    let expected = sign(secret, payload);
    if !constant_time_eq(sig_hex.as_bytes(), expected.as_bytes()) {
        return None;
    }

    let fields: Vec<&str> = payload.split('.').collect();
    if fields.len() != 3 {
        return None;
    }
    let user_id: i64 = fields[0].parse().ok()?;
    let session_version: i32 = fields[1].parse().ok()?;
    let expires: u64 = fields[2].parse().ok()?;

    if now_unix() > expires {
        return None;
    }

    Some(SessionClaims {
        user_id,
        session_version,
    })
}

pub fn set_cookie_header(cookie_value: &str, max_age: u64) -> String {
    format!(
        "{COOKIE_NAME}={cookie_value}; HttpOnly; SameSite=Lax; Secure; Path=/; Max-Age={max_age}"
    )
}

pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Secure; Path=/; Max-Age=0")
}

pub fn extract_session_cookie(cookie_header: &str) -> Option<&str> {
    for part in cookie_header.split("; ") {
        if let Some(val) = part.strip_prefix("clewdr_session=") {
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn sign(secret: &[u8; 32], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
