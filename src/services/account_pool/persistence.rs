use std::collections::{HashMap, HashSet};

use chrono::Utc;
use tracing::{error, warn};

use crate::{
    config::{AccountSlot, AuthMethod, InvalidAccountSlot, Reason},
    db::accounts::{
        active_reset_time, batch_upsert_runtime_states, load_all_accounts, set_account_disabled,
        set_accounts_active,
    },
    services::account_health::compose_health_snapshot,
};

use super::state::{AccountPoolActor, AccountPoolState};

impl AccountPoolActor {
    pub(super) async fn do_flush(state: &mut AccountPoolState) {
        if state.dirty.is_empty() {
            return;
        }
        let dirty_ids: HashSet<i64> = std::mem::take(&mut state.dirty);

        // Build runtime-state updates only. Credential fields
        // (`oauth_access_token / oauth_refresh_token / oauth_expires_at`) must
        // not be written from the in-memory slot — doing so can overwrite a
        // freshly-rotated refresh token with the stale copy that the pool has
        // not seen yet. Credentials follow the rule "DB is authoritative" per
        // `docs/account-normalization-2026-04-21.md`; the dedicated refresh
        // paths (probe, chat's `persist_oauth_refresh`, admin test) write DB
        // themselves and call `update_credential` to sync the in-memory slot.
        let mut params = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.values()) {
            if let Some(id) = cs.account_id
                && dirty_ids.contains(&id)
            {
                params.push((id, cs.to_runtime_params()));
            }
        }

        let mut disabled = Vec::new();
        for uc in state.invalid.values() {
            if dirty_ids.contains(&uc.account_id) {
                disabled.push((uc.account_id, uc.reason.to_db_string()));
            }
        }

        // Await directly — 1-3 accounts, <1ms. On failure, re-insert dirty IDs.
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            warn!("Failed to flush runtime states: {e}");
            for (id, _) in &params {
                state.dirty.insert(*id);
            }
        }

        if !state.reactivated.is_empty() {
            let ids: Vec<i64> = state.reactivated.drain().collect();
            if let Err(e) = set_accounts_active(&state.db, &ids).await {
                warn!("Failed to reactivate accounts: {e}");
            }
        }
        for (id, reason) in &disabled {
            if let Err(e) = set_account_disabled(&state.db, *id, reason).await {
                warn!("Failed to set account {id} disabled: {e}");
                state.dirty.insert(*id);
            }
        }
    }

    pub(super) async fn do_reload(state: &mut AccountPoolState) {
        // Flush pending dirty state before reload to avoid losing in-memory changes
        Self::do_flush(state).await;

        let accounts = match load_all_accounts(&state.db).await {
            Ok(a) => a,
            Err(e) => {
                error!("Failed to load accounts from DB: {e}");
                return;
            }
        };

        // Index current in-memory state by account_id
        let mut mem_cookies: HashMap<i64, AccountSlot> = HashMap::new();
        for cs in state.valid.drain(..) {
            if let Some(id) = cs.account_id {
                mem_cookies.insert(id, cs);
            }
        }
        for (id, cs) in state.exhausted.drain() {
            mem_cookies.insert(id, cs);
        }
        // Drain invalid set — will be rebuilt from DB
        state.invalid.clear();

        let mut replaced_ids = Vec::new();

        // Rebuild from DB
        for row in &accounts {
            if matches!(row.status.as_str(), "disabled" | "auth_error") {
                // Post Step 4 / C6 the invalid bucket only stores
                // (account_id, auth_method, reason) — no credential bytes.
                // Skip rows with no credential at all so we don't surface
                // a phantom invalid entry for a half-deleted account.
                if row.cookie_blob.is_none()
                    && row.oauth_token.is_none()
                    && row.api_key_secret.is_none()
                {
                    continue;
                }
                let reason = row
                    .invalid_reason
                    .as_deref()
                    .map(Reason::from_db_string)
                    .unwrap_or(Reason::Null);
                let auth_method = AuthMethod::from_auth_source(&row.auth_source);
                state
                    .invalid
                    .insert(row.id, InvalidAccountSlot::new(row.id, auth_method, reason));
                continue;
            }

            // Build the slot from whichever credential the row carries.
            // Step 4 / C8 onward: OAuth-only rows go through
            // `AccountSlot::oauth(...)` directly (no placeholder-cookie
            // synthesis); cookie rows continue through `AccountSlot::new`
            // which parses the blob. Step 5: ApiKey rows go through
            // `AccountSlot::api_key(...)` reading the api_key_* columns.
            // The common tail below stamps the remaining row metadata
            // onto either kind.
            let row_kind = AuthMethod::from_auth_source(&row.auth_source);
            let mut cs = match row_kind {
                AuthMethod::ApiKey => {
                    let (Some(base_url), Some(secret)) =
                        (row.api_key_base_url.clone(), row.api_key_secret.clone())
                    else {
                        warn!(
                            "ApiKey row '{}' missing base_url or secret; skipping",
                            row.name
                        );
                        continue;
                    };
                    let extra_headers = match row.api_key_extra_headers.as_deref() {
                        None | Some("") => None,
                        Some(raw) => match serde_json::from_str::<
                            std::collections::BTreeMap<String, String>,
                        >(raw)
                        {
                            Ok(map) if !map.is_empty() => {
                                Some(crate::config::ApiKeyExtraHeaders::new(map))
                            }
                            Ok(_) => None,
                            Err(e) => {
                                warn!(
                                    "ApiKey row '{}' has unparseable extra_headers JSON; \
                                     dropping extras: {e}",
                                    row.name
                                );
                                None
                            }
                        },
                    };
                    let extra_body = match row.api_key_extra_body.as_deref() {
                        None | Some("") => None,
                        Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
                            Ok(v) if v.as_object().is_some_and(|o| !o.is_empty()) => Some(v),
                            Ok(_) => None,
                            Err(e) => {
                                warn!(
                                    "ApiKey row '{}' has unparseable extra_body JSON; \
                                     dropping extra body: {e}",
                                    row.name
                                );
                                None
                            }
                        },
                    };
                    let mimicry_mode = crate::config::MimicryMode::from_db(&row.mimicry_mode);
                    let mimicry_config = match (mimicry_mode, row.mimicry_config.as_deref()) {
                        (crate::config::MimicryMode::ThirdParty, Some(raw)) if !raw.is_empty() => {
                            match serde_json::from_str::<crate::config::ThirdPartyMimicryConfig>(
                                raw,
                            ) {
                                Ok(cfg) => Some(cfg),
                                Err(e) => {
                                    warn!(
                                        "ApiKey row '{}' has unparseable mimicry_config JSON; \
                                         falling back to defaults: {e}",
                                        row.name
                                    );
                                    Some(crate::config::ThirdPartyMimicryConfig::default())
                                }
                            }
                        }
                        _ => None,
                    };
                    AccountSlot::api_key(
                        row.id,
                        base_url,
                        crate::config::ApiKeySecret::new(secret),
                        extra_headers,
                        extra_body,
                        mimicry_mode,
                        mimicry_config,
                    )
                }
                AuthMethod::Cookie | AuthMethod::OAuth => {
                    match (row.cookie_blob.as_deref(), row.oauth_token.as_ref()) {
                        (Some(cookie_str), _) => match AccountSlot::new(cookie_str, None) {
                            Ok(cs) => cs,
                            Err(e) => {
                                warn!("Invalid cookie for account '{}': {e}", row.name);
                                continue;
                            }
                        },
                        (None, Some(token)) => AccountSlot::oauth(row.id, token.clone()),
                        (None, None) => continue,
                    }
                }
            };
            cs.account_id = Some(row.id);
            cs.auth_method = row_kind;
            cs.proxy_url = row.proxy_url.clone();
            cs.email = row.email.clone();
            cs.account_type = row.account_type.clone();
            if let Some(token) = row.oauth_token.clone() {
                cs.token = Some(token);
            }

            // Merge by credential-kind tuple, not cookie byte equality. Kind
            // flip (cookie↔oauth) = real credential replacement → fresh
            // defaults + probing cleanup. Same kind preserves runtime; DB
            // credential is authoritative and was already applied above when
            // `row.oauth_token` was attached to `cs`.
            //
            // `mem_kind` and `row_kind` both come from explicit AuthMethod
            // (Step 4 PR #6 / C3): mem reads its own field (loader stamps
            // it from row.auth_source on load); row reads `auth_source`
            // directly. This replaces the pre-C3 placeholder-cookie marker
            // and `row.oauth_token.is_some()` proxies — cookie accounts
            // hold a bearer token in `slot.token` after `exchange_token`,
            // so token presence is not a reliable kind discriminator.
            //
            // Within the cookie kind, a byte-level `cookie_blob` change is
            // treated as admin-initiated replacement (DB never changes
            // cookie bytes implicitly). OAuth access_token rotation from a
            // normal refresh is preserved — runtime/probing must survive.
            if let Some(mem) = mem_cookies.remove(&row.id) {
                let mem_kind = mem.auth_method;
                let same_kind = mem_kind == row_kind;
                let cookie_content_swap =
                    same_kind && row_kind == AuthMethod::Cookie && mem.cookie != cs.cookie;
                if same_kind && !cookie_content_swap {
                    Self::apply_in_memory_runtime(&mut cs, mem, row_kind == AuthMethod::Cookie);
                    cs.proxy_url = row.proxy_url.clone();
                } else {
                    replaced_ids.push(row.id);
                }
            } else if row_kind != AuthMethod::ApiKey
                && let Some(ref runtime) = row.runtime
            {
                // Cold-restart (or bundle-import) path: mem is empty
                // and we'd otherwise apply the stale row.runtime
                // verbatim. For ApiKey accounts that runtime is
                // structurally meaningless — pay-as-you-go has no
                // quota window / cooldown semantics (PRD Decision 2)
                // — and is actively harmful when the row was
                // previously cookie/oauth and got switched to
                // api_key: a stale `reset_time` would park the slot
                // in `exhausted`, and a stale `count_tokens_allowed
                // = false` would route count_tokens to the local
                // estimator instead of the upstream count_tokens
                // endpoint. Admin update DELETEs the runtime row on
                // switch-in (admin/accounts.rs) as the primary
                // cleanup; this guard is the loader-side defense for
                // bundle-import / manual-DB-edit paths that bypass
                // that cleanup.
                let params = runtime.to_params();
                cs.apply_runtime_state(&params);
                let normalized_reset = active_reset_time(row);
                if cs.reset_time != normalized_reset {
                    cs.reset_time = normalized_reset;
                    Self::mark_dirty(state, cs.account_id);
                }
            }

            if cs.reset_time.is_some() {
                state.exhausted.insert(row.id, cs);
            } else {
                state.valid.push_back(cs);
            }
        }

        // Accounts not in DB anymore → already removed by drain + not re-inserted
        // (mem_cookies remaining entries are deleted accounts)

        // Clear moka cache since cookie set changed
        state.moka.invalidate_all();

        // Rebuild inflight map: preserve current counts, update max_slots from DB
        let mut new_inflight = HashMap::new();
        for row in &accounts {
            if row.cookie_blob.is_none()
                && row.oauth_token.is_none()
                && row.api_key_secret.is_none()
            {
                continue;
            }
            let current = state.inflight.get(&row.id).map_or(0, |(cur, _)| *cur);
            new_inflight.insert(row.id, (current, row.max_slots as u32));
        }
        state.inflight = new_inflight;

        // Rebuild the drain_first index from DB.
        state.drain_first_ids = accounts
            .iter()
            .filter(|r| r.drain_first)
            .map(|r| r.id)
            .collect();

        // Clean stale probing IDs (deleted accounts + cookie-replaced accounts)
        let current_ids: HashSet<i64> = accounts.iter().map(|r| r.id).collect();
        state.probing.retain(|id| current_ids.contains(id));
        for id in &replaced_ids {
            state.probing.remove(id);
        }

        let view = Self::snapshot_view(state);
        let snapshot = compose_health_snapshot(&view, &accounts, Utc::now().timestamp());
        Self::log_account_summary(&snapshot.summary);

        // Spawn probes for unprobed cookies
        Self::spawn_probes_for_unprobed(state);
        Self::emit_accounts_refresh(state);
    }
}
