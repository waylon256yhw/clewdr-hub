use std::collections::{HashMap, HashSet, VecDeque};

use chrono::Utc;
use colored::Colorize;
use moka::sync::Cache;
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use serde::Serialize;
use snafu::{GenerateImplicitData, Location};
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::{
    claude_code_state::probe::probe_cookie,
    config::{ClewdrCookie, CookieStatus, Reason, UsageBreakdown, UselessCookie},
    db::accounts::{
        batch_upsert_runtime_states, load_all_accounts, set_account_disabled, set_accounts_active,
    },
    error::ClewdrError,
    stealth,
};

const INTERVAL: u64 = 300;
const FLUSH_INTERVAL: u64 = 15;
const SESSION_WINDOW_SECS: i64 = 5 * 60 * 60; // 5h
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60; // 7d

#[derive(Debug, Serialize, Clone)]
pub struct CookieStatusInfo {
    pub valid: Vec<CookieStatus>,
    pub exhausted: Vec<CookieStatus>,
    pub invalid: Vec<UselessCookie>,
}

#[derive(Debug)]
enum CookieActorMessage {
    Return(CookieStatus, Option<Reason>),
    Submit(CookieStatus),
    CheckReset,
    Request(
        Option<u64>,
        Vec<i64>,
        RpcReplyPort<Result<CookieStatus, ClewdrError>>,
    ),
    GetStatus(RpcReplyPort<CookieStatusInfo>),
    Delete(CookieStatus, RpcReplyPort<Result<(), ClewdrError>>),
    Update1mSupport(CookieStatus, RpcReplyPort<Result<(), ClewdrError>>),
    ReloadFromDb,
    ProbeAll(RpcReplyPort<Vec<i64>>),
    FlushDirty,
    SetHandle(CookieActorHandle),
    ReleaseSlot(i64),
    GetProbingIds(RpcReplyPort<Vec<i64>>),
    ClearProbing(i64),
    SetProbeError(i64, String),
    ClearProbeError(i64),
    GetProbeErrors(RpcReplyPort<HashMap<i64, String>>),
}

#[derive(Debug)]
struct CookieActorState {
    valid: VecDeque<CookieStatus>,
    exhausted: HashSet<CookieStatus>,
    invalid: HashSet<UselessCookie>,
    moka: Cache<u64, CookieStatus>,
    db: SqlitePool,
    dirty: HashSet<i64>,
    handle: Option<CookieActorHandle>,
    /// Per-account inflight tracking: account_id → (current_inflight, max_slots)
    inflight: HashMap<i64, (u32, u32)>,
    probing: HashSet<i64>,
    reactivated: HashSet<i64>,
    /// Last probe error per account (transient errors only, cleared on success)
    probe_errors: HashMap<i64, String>,
}

struct CookieActor;

impl CookieActor {
    fn mark_dirty(state: &mut CookieActorState, account_id: Option<i64>) {
        if let Some(id) = account_id {
            state.dirty.insert(id);
        }
    }

    fn mark_all_dirty(state: &mut CookieActorState) {
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                state.dirty.insert(id);
            }
        }
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                state.dirty.insert(id);
            }
        }
    }

    fn log(state: &CookieActorState) {
        info!(
            "Valid: {}, Exhausted: {}, Invalid: {}",
            state.valid.len().to_string().green(),
            state.exhausted.len().to_string().yellow(),
            state.invalid.len().to_string().red(),
        );
    }

    fn reset(state: &mut CookieActorState) {
        let mut reset_cookies = Vec::new();
        state.exhausted.retain(|cookie| {
            let reset_cookie = cookie.clone().reset();
            if reset_cookie.reset_time.is_none() {
                reset_cookies.push(reset_cookie);
                false
            } else {
                true
            }
        });
        if reset_cookies.is_empty() {
            return;
        }
        for c in reset_cookies {
            Self::mark_dirty(state, c.account_id);
            state.valid.push_back(c);
        }
        Self::log(state);
    }

    fn refresh_usage_windows(state: &mut CookieActorState) -> bool {
        fn reset_if_due(
            has_reset: Option<bool>,
            resets_at: &mut Option<i64>,
            usage: &mut UsageBreakdown,
            utilization: &mut Option<f64>,
            window_secs: i64,
            now: i64,
        ) -> bool {
            if has_reset == Some(true) && resets_at.map(|ts| now >= ts).unwrap_or(false) {
                *usage = UsageBreakdown::default();
                *utilization = Some(0.0);
                *resets_at = Some(now + window_secs);
                return true;
            }
            false
        }

        let now = Utc::now().timestamp();
        let mut changed = false;

        let apply_resets = |cookie: &mut CookieStatus| {
            let mut cookie_changed = reset_if_due(
                cookie.session_has_reset,
                &mut cookie.session_resets_at,
                &mut cookie.session_usage,
                &mut cookie.session_utilization,
                SESSION_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_has_reset,
                &mut cookie.weekly_resets_at,
                &mut cookie.weekly_usage,
                &mut cookie.weekly_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_sonnet_has_reset,
                &mut cookie.weekly_sonnet_resets_at,
                &mut cookie.weekly_sonnet_usage,
                &mut cookie.weekly_sonnet_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_opus_has_reset,
                &mut cookie.weekly_opus_resets_at,
                &mut cookie.weekly_opus_usage,
                &mut cookie.weekly_opus_utilization,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed
        };

        let mut dirty_from_valid = Vec::new();
        for cookie in state.valid.iter_mut() {
            if apply_resets(cookie) {
                changed = true;
                if let Some(id) = cookie.account_id {
                    dirty_from_valid.push(id);
                }
            }
        }
        for id in dirty_from_valid {
            state.dirty.insert(id);
        }

        if !state.exhausted.is_empty() {
            let mut dirty_from_exhausted = Vec::new();
            let mut new_exhausted = HashSet::with_capacity(state.exhausted.len());
            for mut cookie in state.exhausted.drain() {
                if apply_resets(&mut cookie) {
                    changed = true;
                    if let Some(id) = cookie.account_id {
                        dirty_from_exhausted.push(id);
                    }
                }
                new_exhausted.insert(cookie);
            }
            state.exhausted = new_exhausted;
            for id in dirty_from_exhausted {
                state.dirty.insert(id);
            }
        }

        changed
    }

    fn dispatch(
        &self,
        state: &mut CookieActorState,
        hash: Option<u64>,
        bound: &[i64],
    ) -> Result<CookieStatus, ClewdrError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        Self::reset(state);

        let cache_key = hash.map(|h| {
            if bound.is_empty() {
                h
            } else {
                let mut hasher = DefaultHasher::new();
                h.hash(&mut hasher);
                bound.hash(&mut hasher);
                hasher.finish()
            }
        });

        let is_allowed = |c: &CookieStatus, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            let bound_ok = bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id));
            let slot_ok = c.account_id.map_or(true, |id| {
                inflight.get(&id).map_or(true, |(cur, max)| cur < max)
            });
            bound_ok && slot_ok
        };

        if let Some(key) = cache_key
            && let Some(cached) = state.moka.get(&key)
            && let Some(cookie) = state
                .valid
                .iter()
                .find(|c| *c == &cached && is_allowed(c, &state.inflight))
        {
            let cookie = cookie.clone();
            if let Some(aid) = cookie.account_id {
                if let Some((cur, _)) = state.inflight.get_mut(&aid) {
                    *cur += 1;
                }
            }
            state.moka.insert(key, cookie.clone());
            return Ok(cookie);
        }

        let idx = state
            .valid
            .iter()
            .position(|c| is_allowed(c, &state.inflight))
            .ok_or(if bound.is_empty() {
                ClewdrError::NoCookieAvailable
            } else {
                ClewdrError::BoundAccountsUnavailable
            })?;

        let cookie = state.valid.remove(idx).unwrap();
        if let Some(aid) = cookie.account_id {
            if let Some((cur, _)) = state.inflight.get_mut(&aid) {
                *cur += 1;
            }
        }
        state.valid.push_back(cookie.clone());
        if let Some(key) = cache_key {
            state.moka.insert(key, cookie.clone());
        }
        Ok(cookie)
    }

    fn collect(state: &mut CookieActorState, cookie: CookieStatus, reason: Option<Reason>) {
        let aid = cookie.account_id;

        if let Some(id) = aid {
            state.probing.remove(&id);
        }

        // Remove from whichever set the cookie currently lives in
        let was_valid = state
            .valid
            .iter()
            .position(|c| *c == cookie)
            .map(|i| state.valid.remove(i).unwrap());
        let was_exhausted = state.exhausted.take(&cookie);
        let tmp = UselessCookie::new(cookie.cookie.clone(), Reason::Null);
        let was_invalid = state.invalid.take(&tmp);

        if was_valid.is_none() && was_exhausted.is_none() && was_invalid.is_none() {
            return;
        }

        let changed_set = match &reason {
            None => {
                if cookie.reset_time.is_some() {
                    let was_ex = was_exhausted.is_some();
                    state.exhausted.insert(cookie);
                    !was_ex
                } else {
                    let was_val = was_valid.is_some();
                    state.valid.push_back(cookie);
                    !was_val
                }
            }
            Some(Reason::TooManyRequest(i) | Reason::Restricted(i)) => {
                let mut cookie = cookie;
                cookie.reset_time = Some(*i);
                cookie.reset_window_usage();
                let was_ex = was_exhausted.is_some();
                state.exhausted.insert(cookie);
                !was_ex
            }
            Some(reason) => {
                let mut cookie = cookie;
                cookie.reset_window_usage();
                let was_inv = was_invalid.is_some();
                state.invalid.insert(UselessCookie::with_account_id(
                    cookie.cookie.clone(),
                    reason.clone(),
                    aid,
                ));
                !was_inv
            }
        };

        // Track invalid → valid/exhausted for DB status reactivation
        let moved_out_of_invalid = was_invalid.is_some()
            && matches!(
                &reason,
                None | Some(Reason::TooManyRequest(_) | Reason::Restricted(_))
            );
        if moved_out_of_invalid {
            if let Some(id) = aid {
                state.reactivated.insert(id);
            }
        }

        Self::mark_dirty(state, aid);
        if changed_set {
            Self::log(state);
        }
    }

    fn accept(state: &mut CookieActorState, cookie: CookieStatus) {
        if state.valid.contains(&cookie)
            || state.exhausted.contains(&cookie)
            || state.invalid.iter().any(|c| *c == cookie)
        {
            warn!("Cookie already exists");
            return;
        }
        let needs_probe = cookie.email.is_none() || cookie.account_type.is_none();
        let aid = cookie.account_id;
        state.valid.push_back(cookie.clone());
        Self::mark_dirty(state, aid);
        Self::log(state);

        if needs_probe {
            Self::spawn_probe_guarded(state, &cookie);
        }
    }

    fn spawn_probe_guarded(state: &mut CookieActorState, cookie: &CookieStatus) {
        let Some(account_id) = cookie.account_id else {
            return;
        };
        if state.probing.contains(&account_id) {
            return;
        }
        let Some(ref handle) = state.handle else {
            return;
        };
        state.probing.insert(account_id);
        let handle = handle.clone();
        let cookie = cookie.clone();
        let db = state.db.clone();
        let profile = stealth::global_profile().clone();
        tokio::spawn(async move {
            probe_cookie(account_id, cookie, handle, profile, db).await;
        });
    }

    fn spawn_probes_for_unprobed(state: &mut CookieActorState) {
        let unprobed: Vec<CookieStatus> = state
            .valid
            .iter()
            .filter(|c| c.email.is_none() || c.account_type.is_none())
            .cloned()
            .collect();
        for cookie in &unprobed {
            Self::spawn_probe_guarded(state, cookie);
        }
    }

    fn spawn_probe_all(state: &mut CookieActorState) {
        let cookies: Vec<CookieStatus> = state
            .valid
            .iter()
            .cloned()
            .chain(state.exhausted.iter().cloned())
            .collect();
        for cookie in &cookies {
            Self::spawn_probe_guarded(state, cookie);
        }

        let invalid_cookies: Vec<(ClewdrCookie, Option<i64>)> = state
            .invalid
            .iter()
            .map(|uc| (uc.cookie.clone(), uc.account_id))
            .collect();
        for (cookie_blob, account_id) in invalid_cookies {
            if let Ok(mut cs) = CookieStatus::new(&cookie_blob.to_string(), None) {
                cs.account_id = account_id;
                Self::spawn_probe_guarded(state, &cs);
            }
        }
    }

    fn report(state: &CookieActorState) -> CookieStatusInfo {
        CookieStatusInfo {
            valid: state.valid.clone().into(),
            exhausted: state.exhausted.iter().cloned().collect(),
            invalid: state.invalid.iter().cloned().collect(),
        }
    }

    fn delete(state: &mut CookieActorState, cookie: CookieStatus) -> Result<(), ClewdrError> {
        let mut found = false;
        state.valid.retain(|c| {
            found |= *c == cookie;
            *c != cookie
        });
        let useless = UselessCookie::new(cookie.cookie.clone(), Reason::Null);
        found |= state.exhausted.remove(&cookie) | state.invalid.remove(&useless);

        if found {
            Self::log(state);
            Ok(())
        } else {
            Err(ClewdrError::UnexpectedNone {
                msg: "Delete operation did not find the cookie",
            })
        }
    }

    fn update_1m_support(
        state: &mut CookieActorState,
        cookie: CookieStatus,
    ) -> Result<(), ClewdrError> {
        if let Some(existing) = state.valid.iter_mut().find(|c| **c == cookie) {
            existing.supports_claude_1m_sonnet = cookie.supports_claude_1m_sonnet;
            existing.supports_claude_1m_opus = cookie.supports_claude_1m_opus;
            let aid = existing.account_id;
            Self::mark_dirty(state, aid);
            return Ok(());
        }

        if !state.exhausted.is_empty() {
            let mut updated = false;
            let mut updated_id = None;
            let mut new_exhausted = HashSet::with_capacity(state.exhausted.len());
            for mut existing in state.exhausted.drain() {
                if existing == cookie {
                    existing.supports_claude_1m_sonnet = cookie.supports_claude_1m_sonnet;
                    existing.supports_claude_1m_opus = cookie.supports_claude_1m_opus;
                    updated = true;
                    updated_id = existing.account_id;
                }
                new_exhausted.insert(existing);
            }
            state.exhausted = new_exhausted;
            if updated {
                Self::mark_dirty(state, updated_id);
                return Ok(());
            }
        }

        Err(ClewdrError::UnexpectedNone {
            msg: "Update operation did not find the cookie",
        })
    }

    async fn do_flush(state: &mut CookieActorState) {
        if state.dirty.is_empty() {
            return;
        }
        let dirty_ids: HashSet<i64> = std::mem::take(&mut state.dirty);

        let mut params = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                if dirty_ids.contains(&id) {
                    params.push((id, cs.to_runtime_params()));
                }
            }
        }

        let mut disabled = Vec::new();
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                if dirty_ids.contains(&id) {
                    disabled.push((id, uc.reason.to_db_string()));
                }
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

    async fn do_reload(state: &mut CookieActorState) {
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
        let mut mem_cookies: HashMap<i64, CookieStatus> = HashMap::new();
        for cs in state.valid.drain(..) {
            if let Some(id) = cs.account_id {
                mem_cookies.insert(id, cs);
            }
        }
        for cs in state.exhausted.drain() {
            if let Some(id) = cs.account_id {
                mem_cookies.insert(id, cs);
            }
        }
        // Drain invalid set — will be rebuilt from DB
        state.invalid.clear();

        let mut replaced_ids = Vec::new();

        // Rebuild from DB
        for row in &accounts {
            if row.status == "disabled" {
                let reason = row
                    .invalid_reason
                    .as_deref()
                    .map(Reason::from_db_string)
                    .unwrap_or(Reason::Null);
                let cookie_str = &row.cookie_blob;
                if let Ok(cs) = CookieStatus::new(cookie_str, None) {
                    state.invalid.insert(UselessCookie::with_account_id(
                        cs.cookie,
                        reason,
                        Some(row.id),
                    ));
                }
                continue;
            }

            let cookie_str = &row.cookie_blob;
            let mut cs = match CookieStatus::new(cookie_str, None) {
                Ok(cs) => cs,
                Err(e) => {
                    warn!("Invalid cookie for account '{}': {e}", row.name);
                    continue;
                }
            };
            cs.account_id = Some(row.id);
            cs.email = row.email.clone();
            cs.account_type = row.account_type.clone();

            // Merge: if memory has same account_id with same cookie, preserve runtime
            if let Some(mem) = mem_cookies.remove(&row.id) {
                if mem.cookie == cs.cookie {
                    // Same credential — preserve runtime state from memory
                    cs.token = mem.token;
                    cs.reset_time = mem.reset_time;
                    cs.session_usage = mem.session_usage;
                    cs.weekly_usage = mem.weekly_usage;
                    cs.weekly_sonnet_usage = mem.weekly_sonnet_usage;
                    cs.weekly_opus_usage = mem.weekly_opus_usage;
                    cs.lifetime_usage = mem.lifetime_usage;
                    cs.session_resets_at = mem.session_resets_at;
                    cs.weekly_resets_at = mem.weekly_resets_at;
                    cs.weekly_sonnet_resets_at = mem.weekly_sonnet_resets_at;
                    cs.weekly_opus_resets_at = mem.weekly_opus_resets_at;
                    cs.resets_last_checked_at = mem.resets_last_checked_at;
                    cs.session_has_reset = mem.session_has_reset;
                    cs.weekly_has_reset = mem.weekly_has_reset;
                    cs.weekly_sonnet_has_reset = mem.weekly_sonnet_has_reset;
                    cs.weekly_opus_has_reset = mem.weekly_opus_has_reset;
                    cs.supports_claude_1m_sonnet = mem.supports_claude_1m_sonnet;
                    cs.supports_claude_1m_opus = mem.supports_claude_1m_opus;
                    cs.count_tokens_allowed = mem.count_tokens_allowed;
                    cs.session_utilization = mem.session_utilization;
                    cs.weekly_utilization = mem.weekly_utilization;
                    cs.weekly_sonnet_utilization = mem.weekly_sonnet_utilization;
                    cs.weekly_opus_utilization = mem.weekly_opus_utilization;
                    // Prefer memory email/account_type if DB is null but memory has it
                    if cs.email.is_none() {
                        cs.email = mem.email;
                    }
                    if cs.account_type.is_none() {
                        cs.account_type = mem.account_type;
                    }
                }
                // Cookie changed = credential replacement → use fresh defaults from new()
                else {
                    replaced_ids.push(row.id);
                }
            } else if let Some(ref runtime) = row.runtime {
                let params = runtime.to_params();
                cs.apply_runtime_state(&params);
            }

            // Normalize 1M defaults
            if cs.supports_claude_1m_sonnet.is_none() {
                cs.supports_claude_1m_sonnet = Some(true);
            }
            if cs.supports_claude_1m_opus.is_none() {
                cs.supports_claude_1m_opus = Some(true);
            }

            if cs.reset_time.is_some() {
                state.exhausted.insert(cs);
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
            let current = state.inflight.get(&row.id).map_or(0, |(cur, _)| *cur);
            new_inflight.insert(row.id, (current, row.max_slots as u32));
        }
        state.inflight = new_inflight;

        // Clean stale probing IDs (deleted accounts + cookie-replaced accounts)
        let current_ids: HashSet<i64> = accounts.iter().map(|r| r.id).collect();
        state.probing.retain(|id| current_ids.contains(id));
        for id in &replaced_ids {
            state.probing.remove(id);
        }

        Self::log(state);

        // Spawn probes for unprobed cookies
        Self::spawn_probes_for_unprobed(state);
    }
}

impl Actor for CookieActor {
    type Msg = CookieActorMessage;
    type State = CookieActorState;
    type Arguments = SqlitePool;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        db: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let moka = Cache::builder()
            .max_capacity(1000)
            .time_to_idle(std::time::Duration::from_secs(60 * 60))
            .build();

        let mut state = CookieActorState {
            valid: VecDeque::new(),
            exhausted: HashSet::new(),
            invalid: HashSet::new(),
            moka,
            db,
            dirty: HashSet::new(),
            handle: None,
            inflight: HashMap::new(),
            probing: HashSet::new(),
            reactivated: HashSet::new(),
            probe_errors: HashMap::new(),
        };

        // Load accounts from DB
        Self::do_reload(&mut state).await;

        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CookieActorMessage::Return(cookie, reason) => {
                Self::collect(state, cookie, reason);
            }
            CookieActorMessage::Submit(cookie) => {
                Self::accept(state, cookie);
            }
            CookieActorMessage::CheckReset => {
                Self::refresh_usage_windows(state);
                Self::reset(state);
            }
            CookieActorMessage::Request(cache_hash, bound, reply_port) => {
                let result = self.dispatch(state, cache_hash, &bound);
                reply_port.send(result)?;
            }
            CookieActorMessage::GetStatus(reply_port) => {
                Self::refresh_usage_windows(state);
                let status_info = Self::report(state);
                reply_port.send(status_info)?;
            }
            CookieActorMessage::Delete(cookie, reply_port) => {
                let result = Self::delete(state, cookie);
                reply_port.send(result)?;
            }
            CookieActorMessage::Update1mSupport(cookie, reply_port) => {
                let result = Self::update_1m_support(state, cookie);
                reply_port.send(result)?;
            }
            CookieActorMessage::ReloadFromDb => {
                Self::do_reload(state).await;
            }
            CookieActorMessage::ProbeAll(reply_port) => {
                Self::spawn_probe_all(state);
                reply_port.send(state.probing.iter().copied().collect())?;
            }
            CookieActorMessage::FlushDirty => {
                Self::do_flush(state).await;
            }
            CookieActorMessage::SetHandle(handle) => {
                state.handle = Some(handle);
                // Backfill probes missed during pre_start (handle was None then)
                Self::spawn_probes_for_unprobed(state);
            }
            CookieActorMessage::ReleaseSlot(account_id) => {
                if let Some((cur, _)) = state.inflight.get_mut(&account_id) {
                    *cur = cur.saturating_sub(1);
                }
            }
            CookieActorMessage::GetProbingIds(reply_port) => {
                reply_port.send(state.probing.iter().copied().collect())?;
            }
            CookieActorMessage::ClearProbing(account_id) => {
                state.probing.remove(&account_id);
            }
            CookieActorMessage::SetProbeError(account_id, msg) => {
                state.probe_errors.insert(account_id, msg);
            }
            CookieActorMessage::ClearProbeError(account_id) => {
                state.probe_errors.remove(&account_id);
            }
            CookieActorMessage::GetProbeErrors(reply_port) => {
                reply_port.send(state.probe_errors.clone())?;
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        // Flush all accounts on shutdown
        Self::mark_all_dirty(state);
        // Do synchronous flush (await directly in post_stop)
        let dirty_ids: HashSet<i64> = std::mem::take(&mut state.dirty);
        let mut params = Vec::new();
        for cs in state.valid.iter().chain(state.exhausted.iter()) {
            if let Some(id) = cs.account_id {
                if dirty_ids.contains(&id) {
                    params.push((id, cs.to_runtime_params()));
                }
            }
        }
        if let Err(e) = batch_upsert_runtime_states(&state.db, &params).await {
            error!("Failed to flush runtime states on shutdown: {e}");
        }
        for uc in &state.invalid {
            if let Some(id) = uc.account_id {
                if dirty_ids.contains(&id) {
                    if let Err(e) =
                        set_account_disabled(&state.db, id, &uc.reason.to_db_string()).await
                    {
                        error!("Failed to set account {id} disabled on shutdown: {e}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct CookieActorHandle {
    actor_ref: ActorRef<CookieActorMessage>,
}

impl std::fmt::Debug for CookieActorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieActorHandle").finish()
    }
}

impl CookieActorHandle {
    pub async fn start(db: SqlitePool) -> Result<Self, ractor::SpawnErr> {
        let (actor_ref, _join_handle) = Actor::spawn(None, CookieActor, db).await?;

        let handle = Self {
            actor_ref: actor_ref.clone(),
        };

        // Send the handle to the actor so it can spawn probe tasks
        let _ = ractor::cast!(actor_ref, CookieActorMessage::SetHandle(handle.clone()));

        handle.spawn_timeout_checker().await;
        handle.spawn_flush_timer().await;

        Ok(handle)
    }

    async fn spawn_timeout_checker(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, CookieActorMessage::CheckReset).is_err() {
                    break;
                }
            }
        });
    }

    async fn spawn_flush_timer(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(FLUSH_INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, CookieActorMessage::FlushDirty).is_err() {
                    break;
                }
            }
        });
    }

    pub async fn request(
        &self,
        cache_hash: Option<u64>,
        bound_account_ids: &[i64],
    ) -> Result<CookieStatus, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            CookieActorMessage::Request,
            cache_hash,
            bound_account_ids.to_vec()
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to communicate with CookieActor for request operation: {e}"),
        })?
    }

    pub async fn return_cookie(
        &self,
        cookie: CookieStatus,
        reason: Option<Reason>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::Return(cookie, reason)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for return operation: {e}"),
            }
        })
    }

    pub async fn submit(&self, cookie: CookieStatus) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::Submit(cookie)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for submit operation: {e}"),
            }
        })
    }

    pub async fn get_status(&self) -> Result<CookieStatusInfo, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::GetStatus).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with CookieActor for get status operation: {e}"
                ),
            }
        })
    }

    pub async fn delete_cookie(&self, cookie: CookieStatus) -> Result<(), ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::Delete, cookie).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for delete operation: {e}"),
            }
        })?
    }

    pub async fn update_cookie_1m_support(&self, cookie: CookieStatus) -> Result<(), ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::Update1mSupport, cookie).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for update operation: {e}"),
            }
        })?
    }

    pub async fn reload_from_db(&self) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::ReloadFromDb).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for reload operation: {e}"),
            }
        })
    }

    pub async fn probe_all(&self) -> Result<Vec<i64>, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::ProbeAll).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for probe operation: {e}"),
            }
        })
    }

    pub async fn release_slot(&self, account_id: i64) {
        let _ = ractor::cast!(self.actor_ref, CookieActorMessage::ReleaseSlot(account_id));
    }

    pub async fn get_probing_ids(&self) -> Result<Vec<i64>, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::GetProbingIds).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for get probing ids: {e}"),
            }
        })
    }

    pub async fn clear_probing(&self, account_id: i64) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::ClearProbing(account_id)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for clear probing: {e}"),
            }
        })
    }

    pub async fn set_probe_error(&self, account_id: i64, msg: String) {
        let _ = ractor::cast!(
            self.actor_ref,
            CookieActorMessage::SetProbeError(account_id, msg)
        );
    }

    pub async fn clear_probe_error(&self, account_id: i64) {
        let _ = ractor::cast!(
            self.actor_ref,
            CookieActorMessage::ClearProbeError(account_id)
        );
    }

    pub async fn get_probe_errors(&self) -> Result<HashMap<i64, String>, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::GetProbeErrors).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for get probe errors: {e}"),
            }
        })
    }
}
