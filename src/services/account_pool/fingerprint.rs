use tracing::warn;

use crate::config::{AccountSlot, AuthMethod, InvalidAccountSlot, Reason, TokenInfo};

use super::state::{AccountPoolActor, AccountPoolState, RuntimeMergeMode, RuntimeUpdate};

/// Length of the credential prefix used for fingerprinting in C5's
/// release_runtime guard. 20 bytes is enough to distinguish admin
/// replacements (cookie blobs and refresh tokens are both 80+ chars with
/// high-entropy first bytes) without bloating the message payload that
/// flows through every chat / probe completion.
const CREDENTIAL_FINGERPRINT_LEN: usize = 20;

/// Stable identity for a credential at request-acquire time. Captured by
/// every caller of `release_runtime` so `collect_by_id` can detect that
/// the pool's slot has been credential-rotated (admin replacement) since
/// the request started, and discard the stale runtime / Reason instead of
/// applying it to a slot that no longer represents the same logical
/// credential.
///
/// OAuth uses the **refresh_token** prefix, not access_token: a normal
/// OAuth refresh rotates `access_token` but keeps `refresh_token`, so the
/// fingerprint must survive `refresh_token`-stable rotations or every
/// request that overlaps a refresh would falsely trip the guard. Admin
/// reconnect rotates both, so the fingerprint correctly flips.
///
/// ApiKey uses the api_key secret prefix. Admin rotation of the key
/// itself is the only event that should invalidate runtime — there is
/// no transparent rotation in the api-key model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CredentialFingerprint {
    Cookie(String),
    OAuth(String),
    ApiKey(String),
}

impl CredentialFingerprint {
    fn cookie_prefix(s: &str) -> Self {
        let cap = CREDENTIAL_FINGERPRINT_LEN.min(s.len());
        CredentialFingerprint::Cookie(s[..cap].to_string())
    }

    fn oauth_prefix(s: &str) -> Self {
        let cap = CREDENTIAL_FINGERPRINT_LEN.min(s.len());
        CredentialFingerprint::OAuth(s[..cap].to_string())
    }

    fn api_key_prefix(s: &str) -> Self {
        let cap = CREDENTIAL_FINGERPRINT_LEN.min(s.len());
        CredentialFingerprint::ApiKey(s[..cap].to_string())
    }

    pub fn from_oauth_refresh_token(refresh_token: &str) -> Self {
        Self::oauth_prefix(refresh_token)
    }

    /// Build a fingerprint from a request-time `AccountSlot`. Returns None
    /// when the slot has no usable credential identifier (an OAuth slot
    /// with `token = None`, or an ApiKey slot with `api_key_secret = None`
    /// — should not happen in practice, but treated as "no fingerprint"
    /// so the caller's guard becomes a pass-through rather than a false
    /// rejection).
    pub fn from_slot(slot: &AccountSlot) -> Option<Self> {
        match slot.auth_method {
            // Use the inner cookie blob (`Deref<Target = str>`), not
            // `to_string()` — the latter prepends `sessionKey=` which is
            // identical across every cookie account and would collapse
            // every fingerprint into the same 20-byte prefix. Cookie kind
            // invariant: post-C8 a Cookie slot has `cookie = Some(_)`;
            // None here is a corrupted slot, treat as "no fingerprint".
            AuthMethod::Cookie => slot
                .cookie
                .as_ref()
                .map(|c| Self::cookie_prefix(c.as_ref())),
            AuthMethod::OAuth => slot
                .token
                .as_ref()
                .map(|t| Self::oauth_prefix(&t.refresh_token)),
            AuthMethod::ApiKey => slot
                .api_key_secret
                .as_ref()
                .map(|s| Self::api_key_prefix(s.as_str())),
        }
    }
}

impl AccountPoolActor {
    /// Update the cached OAuth token for an account in both `valid` and
    /// `exhausted`. The authoritative DB write is expected to have already
    /// happened on the caller's side — this only keeps the in-memory slot in
    /// sync so subsequent dispatches don't hand out a stale credential.
    ///
    /// Does not mark the account dirty: the runtime flush must never write
    /// credential columns, per `docs/account-normalization-2026-04-21.md`
    /// ("凭证类字段以 DB 为准").
    pub(super) fn update_slot_credential(
        state: &mut AccountPoolState,
        account_id: i64,
        token: Option<TokenInfo>,
    ) {
        for slot in state.valid.iter_mut() {
            if slot.account_id == Some(account_id) {
                slot.token = token.clone();
            }
        }

        if let Some(slot) = state.exhausted.get_mut(&account_id) {
            slot.token = token;
        }
    }

    pub(super) fn pool_oauth_refresh_token_matches(
        state: &AccountPoolState,
        account_id: i64,
        expected_refresh_token: &str,
    ) -> bool {
        state
            .valid
            .iter()
            .find(|slot| slot.account_id == Some(account_id))
            .or_else(|| state.exhausted.get(&account_id))
            .and_then(|slot| slot.token.as_ref())
            .is_some_and(|token| token.refresh_token == expected_refresh_token)
    }

    /// In-memory convergence for an account whose authoritative status was
    /// just written to DB by an explicit path (`set_account_auth_error`,
    /// `set_account_disabled`, or similar). Removes the account from dispatch
    /// surfaces, wipes affinity entries pointing at it, and records it in
    /// `state.invalid` so pool-view summaries reflect DB reality.
    ///
    /// Deliberately does **not** call `mark_dirty`: the status was already
    /// persisted by the caller, and letting `do_flush` also touch status would
    /// let the pool's Reason race with the DB's (`auth_error` vs `disabled`).
    /// See `docs/account-normalization-2026-04-21.md` §"容易漏掉 #5" for the
    /// broader principle.
    pub(super) fn converge_invalidate(
        state: &mut AccountPoolState,
        account_id: i64,
        reason: Reason,
    ) {
        // Pull the account out of valid / exhausted, capturing its auth_method
        // along the way so the InvalidAccountSlot record can preserve the
        // kind for admin overview's invalid-grouping display. If the
        // account isn't in either bucket (already invalid, or never loaded)
        // we leave `state.invalid` untouched — over-writing it would erase
        // the existing reason without us being sure of the kind.
        let mut removed_kind: Option<AuthMethod> = None;
        state.valid.retain(|c| {
            if c.account_id == Some(account_id) {
                if removed_kind.is_none() {
                    removed_kind = Some(c.auth_method);
                }
                false
            } else {
                true
            }
        });

        if let Some(slot) = state.exhausted.remove(&account_id) {
            removed_kind.get_or_insert(slot.auth_method);
        }

        // Record in invalid so pool-view summaries and collect's sticky-reason
        // guard see the authoritative reason. Existing entry (if any) is
        // replaced so the reason reflects the latest cause.
        if let Some(auth_method) = removed_kind {
            state.invalid.insert(
                account_id,
                InvalidAccountSlot::new(account_id, auth_method, reason),
            );
        }

        // Stop advertising the account for preferred-drain dispatch.
        state.drain_first_ids.remove(&account_id);

        // Detach the account from every flush-driven DB status write so the
        // authoritative status just written by the caller cannot be raced:
        //   - `reactivated` would cause `set_accounts_active` to flip it back
        //     to "active".
        //   - `dirty` combined with an entry in `state.invalid` would cause
        //     `set_account_disabled(id, reason.to_db_string())` to overwrite
        //     an `auth_error` row with `disabled`. Runtime-state flushing
        //     only scans `valid` + `exhausted` (neither contains this account
        //     anymore), so dropping the account from `dirty` loses nothing
        //     meaningful.
        state.reactivated.remove(&account_id);
        state.dirty.remove(&account_id);

        // Wipe affinity entries pointing at this account_id so coding sessions
        // rebind on the next request.
        state
            .moka
            .invalidate_entries_if(move |_, v| *v == account_id)
            .ok();

        // Inflight is intentionally left alone: in-flight Return / ReleaseSlot
        // messages still arrive for this account and must decrement the
        // counter. The collect sticky-reason guard prevents those Returns from
        // flipping the account back into `valid`.

        Self::emit_accounts_refresh(state);
    }

    /// Account-id-keyed collect. Finds the pool's own slot for this
    /// `account_id`, merges `update` onto it, then moves it between
    /// `valid` / `exhausted` / `invalid` according to `reason`. Credential
    /// bytes on the pool's slot are never touched — only the runtime
    /// fields in `update`. See
    /// `docs/account-normalization-2026-04-21.md` §Step 3 Goal 1.
    /// Compute the credential fingerprint of the pool's *current* slot for
    /// `account_id`, by peeking each bucket without consuming. C5's guard
    /// compares this against the caller's request-time fingerprint.
    ///
    /// Lookup order: `valid` → `exhausted`. The `invalid` bucket no
    /// longer carries credential bytes (Step 4 / C6 retired
    /// `InvalidAccountSlot.cookie`) — we return None for invalid-only
    /// accounts. That's correct because every reason that can land an
    /// account in `invalid` (Free / Disabled / Banned / Null) is already
    /// caught by the sticky-reason guard above before this fingerprint
    /// check runs, so a None return here cannot mask a stale-write race.
    pub(super) fn pool_credential_fingerprint(
        state: &AccountPoolState,
        account_id: i64,
    ) -> Option<CredentialFingerprint> {
        if let Some(slot) = state
            .valid
            .iter()
            .find(|c| c.account_id == Some(account_id))
        {
            return CredentialFingerprint::from_slot(slot);
        }
        if let Some(slot) = state.exhausted.get(&account_id) {
            return CredentialFingerprint::from_slot(slot);
        }
        None
    }

    pub(super) fn collect_by_id(
        state: &mut AccountPoolState,
        account_id: i64,
        update: RuntimeUpdate,
        reason: Option<Reason>,
        expected_fingerprint: Option<CredentialFingerprint>,
        merge_mode: RuntimeMergeMode,
    ) -> bool {
        // Step 3.5 C4b note: `collect_by_id` only carries `Option<Reason>`
        // — it has no access to the original `ClewdrError` /
        // `AccountFailureContext` that produced this transition. The
        // structured `accounts.last_failure_json` column is therefore
        // NEVER written here. Real-context writes happen at the
        // direct DB writers in `chat.rs` (mark_oauth_account_*,
        // cookie InvalidCookie path) and `probe.rs`
        // (run_cookie_probe error arms, probe_oauth_upstream_failure)
        // BEFORE they call `release_runtime` / `release_account` to
        // queue the bucket move. By the time the work reaches
        // `collect_by_id`, the persistence write is already done.
        //
        // Routing the full context through `release_runtime` /
        // `AccountPoolMessage::Return` would require widening every
        // pool message — explicitly out of scope for v1.2.0.
        let removed_probe = state.probing.remove(&account_id);

        // Sticky-reason guard: must peek `invalid` BEFORE we remove, so a
        // Return from an in-flight request whose account was explicitly
        // invalidated (auth_error / disabled / banned / free / null)
        // doesn't auto-reactivate. TMR / Restricted stay transient — they
        // intentionally flow through the cooldown reactivation path below.
        if let Some(existing) = state.invalid.get(&account_id)
            && matches!(
                existing.reason,
                Reason::Free | Reason::Disabled | Reason::Banned | Reason::Null
            )
        {
            return removed_probe;
        }

        // Fingerprint guard (Step 4 / C5): the caller captured the
        // credential identity at request-acquire time. If the pool's
        // current credential differs (admin reconnect or kind flip while
        // the request was in flight), the runtime + Reason in this update
        // belong to a credential that no longer represents this account
        // — applying either would either erase the new credential's
        // usage state or push a stale auth_error onto a healthy slot.
        //
        // OAuth refresh is *not* a mismatch: the fingerprint is the
        // refresh_token prefix, which survives a normal refresh.
        if let Some(expected) = expected_fingerprint.as_ref() {
            let actual = Self::pool_credential_fingerprint(state, account_id);
            if actual.as_ref() != Some(expected) {
                warn!(
                    "[release_runtime] credential fingerprint mismatch for account {account_id} \
                     (expected {:?}, actual {:?}); dropping stale runtime + reason",
                    expected, actual
                );
                return removed_probe;
            }
        }

        let had_valid = state
            .valid
            .iter()
            .position(|c| c.account_id == Some(account_id))
            .and_then(|i| state.valid.remove(i));
        let had_exhausted = state.exhausted.remove(&account_id);
        // Don't pop the invalid entry yet — if the account is *only* in
        // invalid we can't rebucket it (post-C6 invalid no longer carries
        // credential bytes for slot rebuild), so we'd otherwise leak it
        // out of every bucket. Only consume the entry if we actually have
        // a slot to migrate.
        let had_invalid_flag = state.invalid.contains_key(&account_id);

        let had_valid_flag = had_valid.is_some();
        let had_exhausted_flag = had_exhausted.is_some();

        // Prefer a full slot from valid / exhausted because it carries the
        // live credential. After Step 4 / C6 the `invalid` bucket no longer
        // stores credential bytes, so an account that's *only* in invalid
        // cannot be re-bucketed here — it stays put until `do_reload`
        // rebuilds it from DB. In practice every reason that lands an
        // account in `invalid` (Free / Disabled / Banned / Null) is
        // sticky and would have been caught by the sticky-reason guard
        // above, so this `_` arm is unreachable on the hot path; we
        // return defensively rather than panicking.
        let mut slot = match (had_valid, had_exhausted) {
            (Some(s), _) => s,
            (None, Some(s)) => s,
            _ => return removed_probe,
        };

        // We have a slot to rebucket — pop the invalid entry now (if any)
        // so the rebucket below is the sole writer for this account_id.
        let _ = state.invalid.remove(&account_id);

        match merge_mode {
            RuntimeMergeMode::Full => slot.apply_runtime_state(&update),
            RuntimeMergeMode::OAuthSnapshot => slot.apply_oauth_snapshot_runtime(&update),
        }

        let changed_set = match &reason {
            None => {
                if slot.reset_time.is_some() {
                    state.exhausted.insert(account_id, slot);
                    !had_exhausted_flag
                } else {
                    state.valid.push_back(slot);
                    !had_valid_flag
                }
            }
            Some(Reason::TooManyRequest(i) | Reason::Restricted(i)) => {
                slot.reset_time = Some(*i);
                slot.reset_window_usage();
                state.exhausted.insert(account_id, slot);
                !had_exhausted_flag
            }
            Some(reason) => {
                slot.reset_window_usage();
                state.invalid.insert(
                    account_id,
                    InvalidAccountSlot::new(account_id, slot.auth_method, reason.clone()),
                );
                !had_invalid_flag
            }
        };

        let moved_out_of_invalid = had_invalid_flag
            && matches!(
                &reason,
                None | Some(Reason::TooManyRequest(_) | Reason::Restricted(_))
            );
        if moved_out_of_invalid {
            state.reactivated.insert(account_id);
        }

        Self::mark_dirty(state, Some(account_id));
        if changed_set {
            Self::log(state);
        }
        removed_probe
    }

    pub(super) fn apply_in_memory_runtime(
        dst: &mut AccountSlot,
        mem: AccountSlot,
        preserve_token: bool,
    ) {
        if preserve_token {
            dst.token = mem.token;
        }
        dst.reset_time = mem.reset_time;
        dst.session_usage = mem.session_usage;
        dst.weekly_usage = mem.weekly_usage;
        dst.weekly_sonnet_usage = mem.weekly_sonnet_usage;
        dst.weekly_opus_usage = mem.weekly_opus_usage;
        dst.lifetime_usage = mem.lifetime_usage;
        dst.session_resets_at = mem.session_resets_at;
        dst.weekly_resets_at = mem.weekly_resets_at;
        dst.weekly_sonnet_resets_at = mem.weekly_sonnet_resets_at;
        dst.weekly_opus_resets_at = mem.weekly_opus_resets_at;
        dst.resets_last_checked_at = mem.resets_last_checked_at;
        dst.session_has_reset = mem.session_has_reset;
        dst.weekly_has_reset = mem.weekly_has_reset;
        dst.weekly_sonnet_has_reset = mem.weekly_sonnet_has_reset;
        dst.weekly_opus_has_reset = mem.weekly_opus_has_reset;
        dst.supports_claude_1m_sonnet = mem.supports_claude_1m_sonnet;
        dst.supports_claude_1m_opus = mem.supports_claude_1m_opus;
        dst.count_tokens_allowed = mem.count_tokens_allowed;
        dst.session_utilization = mem.session_utilization;
        dst.weekly_utilization = mem.weekly_utilization;
        dst.weekly_sonnet_utilization = mem.weekly_sonnet_utilization;
        dst.weekly_opus_utilization = mem.weekly_opus_utilization;
        dst.weekly_scoped_limits = mem.weekly_scoped_limits;
        // Prefer memory email/account_type if DB is null but memory has it.
        if dst.email.is_none() {
            dst.email = mem.email;
        }
        if dst.account_type.is_none() {
            dst.account_type = mem.account_type;
        }
    }
}
