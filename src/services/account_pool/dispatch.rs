use std::collections::HashMap;

use crate::{config::AccountSlot, error::ClewdrError};

use super::state::{AccountPoolActor, AccountPoolState};

impl AccountPoolActor {
    pub(super) fn dispatch(
        &self,
        state: &mut AccountPoolState,
        hash: Option<u64>,
        bound: &[i64],
    ) -> Result<AccountSlot, ClewdrError> {
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

        // --- predicates ---
        let bound_ok = |id: i64| -> bool { bound.is_empty() || bound.contains(&id) };
        let slot_ok = |id: i64, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            inflight.get(&id).is_none_or(|(cur, max)| cur < max)
        };
        let is_usable = |c: &AccountSlot, inflight: &HashMap<i64, (u32, u32)>| -> bool {
            c.account_id
                .is_some_and(|id| bound_ok(id) && slot_ok(id, inflight))
        };

        // ---------- Phase A: affinity ----------
        // Check the prompt-hash → account_id binding first. If the cached
        // account is usable right now, return it without touching the cache
        // (no insert). If it's unusable only because its inflight slots are
        // saturated, we overflow-borrow another drain_first (or regular)
        // account — but we do NOT rewrite the cache, so affinity stays with
        // the original once it frees up. The cache is only invalidated when
        // the cached account has been removed from `valid` (Invalidate, delete,
        // or bound mismatch) — in that case we fall through to B/C which
        // rewrites it.
        let cached_id = cache_key.and_then(|k| state.moka.get(&k));
        if let Some(cached_id) = cached_id {
            let cached_pos = state
                .valid
                .iter()
                .position(|c| c.account_id == Some(cached_id));
            match cached_pos {
                None => {
                    // cached in moka but no longer in valid (Invalidate'd /
                    // account removed / filtered by bound). Let B/C pick a
                    // fresh account and rewrite the cache.
                    if let Some(k) = cache_key {
                        state.moka.invalidate(&k);
                    }
                }
                Some(pos) => {
                    if !state.drain_first_ids.contains(&cached_id)
                        && let Some(drain_pos) = state.valid.iter().position(|c| {
                            is_usable(c, &state.inflight)
                                && c.account_id
                                    .is_some_and(|id| state.drain_first_ids.contains(&id))
                        })
                    {
                        return Self::commit_dispatch(state, drain_pos, cache_key, true);
                    }
                    if is_usable(&state.valid[pos], &state.inflight) {
                        return Self::commit_dispatch(state, pos, cache_key, false);
                    }
                    if !bound_ok(cached_id) {
                        // Cached doesn't match this request's bound set — treat
                        // as stale. Drop cache, fall through to B/C to bind to
                        // an in-bound account.
                        if let Some(k) = cache_key {
                            state.moka.invalidate(&k);
                        }
                    } else {
                        // Only inflight saturation — overflow-borrow a sibling
                        // (drain_first preferred) without touching the cache.
                        let borrow_pos = state
                            .valid
                            .iter()
                            .position(|c| {
                                c.account_id != Some(cached_id)
                                    && is_usable(c, &state.inflight)
                                    && c.account_id
                                        .is_some_and(|id| state.drain_first_ids.contains(&id))
                            })
                            .or_else(|| {
                                state.valid.iter().position(|c| {
                                    c.account_id != Some(cached_id) && is_usable(c, &state.inflight)
                                })
                            });
                        return match borrow_pos {
                            Some(pos) => Self::commit_dispatch(state, pos, cache_key, false),
                            None => Err(Self::dispatch_empty_error(state, bound)),
                        };
                    }
                }
            }
        }

        // ---------- Phase B: prefer drain_first accounts ----------
        if !state.drain_first_ids.is_empty()
            && let Some(pos) = state.valid.iter().position(|c| {
                is_usable(c, &state.inflight)
                    && c.account_id
                        .is_some_and(|id| state.drain_first_ids.contains(&id))
            })
        {
            return Self::commit_dispatch(state, pos, cache_key, true);
        }

        // ---------- Phase C: round-robin ----------
        if let Some(pos) = state
            .valid
            .iter()
            .position(|c| is_usable(c, &state.inflight))
        {
            return Self::commit_dispatch(state, pos, cache_key, true);
        }

        Err(Self::dispatch_empty_error(state, bound))
    }

    /// Remove the slot at `pos` from `valid`, increment inflight, re-queue at
    /// the back (round-robin), and optionally rewrite the affinity cache.
    fn commit_dispatch(
        state: &mut AccountPoolState,
        pos: usize,
        cache_key: Option<u64>,
        rewrite_cache: bool,
    ) -> Result<AccountSlot, ClewdrError> {
        let cookie = state.valid.remove(pos).unwrap();
        if let Some(aid) = cookie.account_id
            && let Some((cur, _)) = state.inflight.get_mut(&aid)
        {
            *cur += 1;
        }
        state.valid.push_back(cookie.clone());
        if rewrite_cache
            && let Some(key) = cache_key
            && let Some(aid) = cookie.account_id
        {
            state.moka.insert(key, aid);
        }
        Ok(cookie)
    }

    /// Classify dispatch failure: if any in-bound account is still in `valid`
    /// or `exhausted` we return `UpstreamCoolingDown` (transient); otherwise
    /// there is no account to serve at all → `NoValidUpstreamAccounts`.
    fn dispatch_empty_error(state: &AccountPoolState, bound: &[i64]) -> ClewdrError {
        let has_relevant_valid = state
            .valid
            .iter()
            .any(|c| bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id)));
        let has_relevant_exhausted = state
            .exhausted
            .values()
            .any(|c| bound.is_empty() || c.account_id.is_some_and(|id| bound.contains(&id)));
        if has_relevant_valid || has_relevant_exhausted {
            ClewdrError::UpstreamCoolingDown
        } else {
            ClewdrError::NoValidUpstreamAccounts
        }
    }
}
