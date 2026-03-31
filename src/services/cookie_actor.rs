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
    config::{CookieStatus, Reason, UsageBreakdown, UselessCookie},
    db::accounts::{batch_upsert_runtime_states, load_all_accounts, set_account_disabled},
    error::ClewdrError,
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
    Request(Option<u64>, RpcReplyPort<Result<CookieStatus, ClewdrError>>),
    GetStatus(RpcReplyPort<CookieStatusInfo>),
    Delete(CookieStatus, RpcReplyPort<Result<(), ClewdrError>>),
    Update1mSupport(CookieStatus, RpcReplyPort<Result<(), ClewdrError>>),
    ReloadFromDb,
    FlushDirty,
}

#[derive(Debug)]
struct CookieActorState {
    valid: VecDeque<CookieStatus>,
    exhausted: HashSet<CookieStatus>,
    invalid: HashSet<UselessCookie>,
    moka: Cache<u64, CookieStatus>,
    db: SqlitePool,
    dirty: HashSet<i64>,
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
            window_secs: i64,
            now: i64,
        ) -> bool {
            if has_reset == Some(true) && resets_at.map(|ts| now >= ts).unwrap_or(false) {
                *usage = UsageBreakdown::default();
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
                SESSION_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_has_reset,
                &mut cookie.weekly_resets_at,
                &mut cookie.weekly_usage,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_sonnet_has_reset,
                &mut cookie.weekly_sonnet_resets_at,
                &mut cookie.weekly_sonnet_usage,
                WEEKLY_WINDOW_SECS,
                now,
            );
            cookie_changed |= reset_if_due(
                cookie.weekly_opus_has_reset,
                &mut cookie.weekly_opus_resets_at,
                &mut cookie.weekly_opus_usage,
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
    ) -> Result<CookieStatus, ClewdrError> {
        Self::reset(state);
        if let Some(hash) = hash
            && let Some(cookie) = state.moka.get(&hash)
            && let Some(cookie) = state.valid.iter().find(|&c| c == &cookie)
        {
            state.moka.insert(hash, cookie.clone());
            return Ok(cookie.clone());
        }
        let cookie = state
            .valid
            .pop_front()
            .ok_or(ClewdrError::NoCookieAvailable)?;
        state.valid.push_back(cookie.clone());
        if let Some(hash) = hash {
            state.moka.insert(hash, cookie.clone());
        }
        Ok(cookie)
    }

    fn collect(state: &mut CookieActorState, cookie: CookieStatus, reason: Option<Reason>) {
        let Some(reason) = reason else {
            if let Some(existing) = state.valid.iter_mut().find(|c| **c == cookie) {
                let aid = existing.account_id;
                *existing = cookie;
                existing.account_id = aid;
                Self::mark_dirty(state, aid);
            }
            return;
        };
        let mut find_remove = |cookie: &CookieStatus| {
            state.valid.retain(|c| c != cookie);
        };
        match reason {
            Reason::TooManyRequest(i) | Reason::Restricted(i) => {
                find_remove(&cookie);
                let mut cookie = cookie;
                cookie.reset_time = Some(i);
                cookie.reset_window_usage();
                let aid = cookie.account_id;
                if !state.exhausted.insert(cookie) {
                    return;
                }
                Self::mark_dirty(state, aid);
            }
            reason => {
                find_remove(&cookie);
                let aid = cookie.account_id;
                let mut cookie = cookie;
                cookie.reset_window_usage();
                if !state.invalid.insert(UselessCookie::with_account_id(
                    cookie.cookie.clone(),
                    reason,
                    aid,
                )) {
                    return;
                }
                Self::mark_dirty(state, aid);
            }
        }
        Self::log(state);
    }

    fn accept(state: &mut CookieActorState, cookie: CookieStatus) {
        if state.valid.contains(&cookie)
            || state.exhausted.contains(&cookie)
            || state.invalid.iter().any(|c| *c == cookie)
        {
            warn!("Cookie already exists");
            return;
        }
        let aid = cookie.account_id;
        state.valid.push_back(cookie);
        Self::mark_dirty(state, aid);
        Self::log(state);
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
                }
                // Cookie changed = credential replacement → use fresh defaults from new()
            } else if let Some(runtime) = &row.runtime {
                // New account from DB with persisted runtime state
                cs.apply_runtime_state(&runtime.to_params());
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

        Self::log(state);
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
            CookieActorMessage::Request(cache_hash, reply_port) => {
                let result = self.dispatch(state, cache_hash);
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
            CookieActorMessage::FlushDirty => {
                Self::do_flush(state).await;
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
                    if let Err(e) = set_account_disabled(&state.db, id, &uc.reason.to_db_string()).await {
                        error!("Failed to set account {id} disabled on shutdown: {e}");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Handle for interacting with the CookieActor
#[derive(Clone)]
pub struct CookieActorHandle {
    actor_ref: ActorRef<CookieActorMessage>,
}

impl CookieActorHandle {
    pub async fn start(db: SqlitePool) -> Result<Self, ractor::SpawnErr> {
        let (actor_ref, _join_handle) = Actor::spawn(None, CookieActor, db).await?;

        let handle = Self {
            actor_ref: actor_ref.clone(),
        };
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

    pub async fn request(&self, cache_hash: Option<u64>) -> Result<CookieStatus, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::Request, cache_hash).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to communicate with CookieActor for request operation: {e}"),
            }
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
}
