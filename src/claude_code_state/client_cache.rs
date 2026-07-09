//! Per-account wreq client cache for the chat / count_tokens hot path.
//!
//! Before this cache, `ClaudeCodeState::from_credential` and
//! `acquire_account` built a fresh `wreq::Client` on every request AND
//! every retry attempt — a fresh connection pool each time, so every
//! upstream send paid a full TCP + TLS handshake. Caching per account
//! restores connection reuse while preserving the two invariants the
//! per-request build existed to protect:
//!
//! - **Cookie-jar isolation**: Cookie/OAuth clients run with
//!   `cookie_store(true)`; upstream `Set-Cookie` state (e.g. `__cf_bm`)
//!   is account-identity. The cache key includes `account_id` and the
//!   credential fingerprint, so a jar is never shared across accounts
//!   or across an admin credential rotation.
//! - **TLS fingerprint (JA4) emulation**: the shape decision
//!   (Cookie/OAuth → CLI emulation; ApiKey → plain, or emulation iff
//!   third-party mimicry) is part of the key, and every miss builds
//!   with exactly the same builder calls as before.
//!
//! Invalidation is structural: credential rotation or a proxy change
//! produces a different key, and the superseded client ages out via
//! `time_to_idle`. `time_to_live` bounds total jar lifetime. The admin
//! `/test` probe intentionally bypasses this cache
//! (`build_emulated_api_client`) so a test exercises a cold handshake.

use std::{sync::LazyLock, time::Duration};

use moka::sync::Cache;
use snafu::ResultExt;

use crate::{
    config::{AccountSlot, AuthMethod},
    error::{ClewdrError, WreqSnafu},
    services::account_pool::CredentialFingerprint,
};

use super::{fingerprint, proxy_from_url};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClientShape {
    /// `cookie_store(true)` + Claude Code CLI TLS emulation.
    CookieOrOauth,
    /// Plain client — direct API call, no emulation, no cookie jar.
    ApiKeyPlain,
    /// Third-party relay cloak: CLI TLS emulation, no cookie jar.
    ApiKeyEmulated,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    account_id: Option<i64>,
    shape: ClientShape,
    proxy_url: Option<String>,
    /// Same identity token the pool's release guard uses — rotating a
    /// credential flips the fingerprint, which retires the cached
    /// client (and its cookie jar) for that account.
    credential_fp: Option<CredentialFingerprint>,
}

static CLIENT_CACHE: LazyLock<Cache<ClientKey, wreq::Client>> = LazyLock::new(|| {
    Cache::builder()
        // Generously above any realistic account count; eviction is
        // driven by the idle/lifetime clocks, not capacity.
        .max_capacity(1024)
        .time_to_idle(Duration::from_secs(30 * 60))
        .time_to_live(Duration::from_secs(2 * 60 * 60))
        .build()
});

fn shape_for(slot: &AccountSlot) -> ClientShape {
    match slot.auth_method {
        AuthMethod::Cookie | AuthMethod::OAuth => ClientShape::CookieOrOauth,
        AuthMethod::ApiKey if slot.mimicry_mode.is_third_party() => ClientShape::ApiKeyEmulated,
        AuthMethod::ApiKey => ClientShape::ApiKeyPlain,
    }
}

/// Exactly the builder logic previously inlined in `from_credential` /
/// `acquire_account` — kept in one place so a cache miss can never
/// drift from the historical per-request construction. Fallible on
/// purpose: a build failure propagates rather than degrading to a
/// plain client, because sending a credentialed (Cookie/OAuth) request
/// through a client with no TLS emulation / no proxy would ship a
/// contradictory fingerprint — better to fail the request than to
/// silently break stealth.
fn build(shape: ClientShape, proxy_url: Option<&str>) -> Result<wreq::Client, ClewdrError> {
    let mut builder = wreq::Client::builder();
    match shape {
        ClientShape::CookieOrOauth => {
            builder = builder
                .cookie_store(true)
                .emulation(fingerprint::claude_code_emulation());
        }
        ClientShape::ApiKeyEmulated => {
            builder = builder.emulation(fingerprint::claude_code_emulation());
        }
        ClientShape::ApiKeyPlain => {}
    }
    if let Some(proxy) = proxy_from_url(proxy_url) {
        builder = builder.proxy(proxy);
    }
    builder.build().context(WreqSnafu {
        msg: "Failed to build client for credential",
    })
}

/// Fetch (or build and cache) the client for a dispatched slot.
/// `wreq::Client` is an `Arc` handle — cloning shares the underlying
/// connection pool, which is the whole point.
///
/// Build failures are NOT cached: on the (effectively unreachable)
/// error path the entry is left empty so the next request retries a
/// fresh build rather than being pinned to a broken client. Concurrent
/// cold-start misses on the same key may each build once — accepted, a
/// one-time startup cost — in exchange for never caching a failure.
pub(crate) fn get_or_build(slot: &AccountSlot) -> Result<wreq::Client, ClewdrError> {
    let shape = shape_for(slot);
    let key = ClientKey {
        account_id: slot.account_id,
        shape,
        proxy_url: slot.proxy_url.clone(),
        credential_fp: CredentialFingerprint::from_slot(slot),
    };
    if let Some(client) = CLIENT_CACHE.get(&key) {
        return Ok(client);
    }
    let client = build(shape, slot.proxy_url.as_deref())?;
    CLIENT_CACHE.insert(key, client.clone());
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountSlot;

    fn oauth_slot(account_id: i64, refresh_token: &str) -> AccountSlot {
        AccountSlot {
            account_id: Some(account_id),
            auth_method: AuthMethod::OAuth,
            token: Some(crate::config::TokenInfo::from_parts(
                "at".to_string(),
                refresh_token.to_string(),
                std::time::Duration::from_secs(3600),
                "org-uuid".to_string(),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn same_slot_hits_cache_and_rotation_retires_it() {
        // Unique account id so parallel tests can't collide on entries.
        let slot = oauth_slot(910_001, "rt-fingerprint-stable-000000");
        let before = {
            CLIENT_CACHE.run_pending_tasks();
            CLIENT_CACHE.entry_count()
        };
        let _a = get_or_build(&slot).unwrap();
        let _b = get_or_build(&slot).unwrap();
        CLIENT_CACHE.run_pending_tasks();
        // Two gets, one entry — the second was a hit.
        assert_eq!(CLIENT_CACHE.entry_count(), before + 1);

        // Credential rotation → different fingerprint → different key →
        // a fresh client (and cookie jar) for the rotated credential.
        let rotated = oauth_slot(910_001, "rt-fingerprint-ROTATED-11111");
        let _c = get_or_build(&rotated).unwrap();
        CLIENT_CACHE.run_pending_tasks();
        assert_eq!(CLIENT_CACHE.entry_count(), before + 2);
    }

    #[test]
    fn accounts_never_share_a_key_even_with_same_shape_and_proxy() {
        let a = oauth_slot(1, "rt-shared-prefix-differs-late-a");
        let b = oauth_slot(2, "rt-shared-prefix-differs-late-b");
        let key = |s: &AccountSlot| ClientKey {
            account_id: s.account_id,
            shape: shape_for(s),
            proxy_url: s.proxy_url.clone(),
            credential_fp: CredentialFingerprint::from_slot(s),
        };
        assert_ne!(key(&a), key(&b));
    }
}
