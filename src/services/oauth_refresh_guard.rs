//! Per-account serialization for OAuth refresh operations.
//!
//! Anthropic rotates refresh tokens on every successful refresh, so two concurrent
//! refresh calls on the same account would both present the same stored refresh
//! token — one succeeds and rotates the token, the other fails with
//! `Refresh token not found or invalid` and risks flipping the account into
//! `auth_error`.
//!
//! Callers wrap their refresh path with [`guard().lock(account_id).await`]. After
//! acquiring the guard they **must** re-read the current token (e.g. from the
//! account pool or DB) — if a peer already refreshed while they waited, they
//! should reuse the fresh token instead of calling the upstream again.
//!
//! This module is deliberately kept free of knowledge about the account pool or
//! credential storage so it can move into the credential layer unchanged during
//! the dev-branch Step 3 credential plumbing cleanup.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

pub struct OAuthRefreshGuard {
    registry: Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>,
}

impl OAuthRefreshGuard {
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire the per-account refresh lock. The returned guard serializes this
    /// account's refreshes against other callers; drop it to release.
    pub async fn lock(&self, account_id: i64) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut registry = self
                .registry
                .lock()
                .expect("oauth refresh guard registry poisoned");
            registry
                .entry(account_id)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }

    /// Best-effort removal of the registry entry for an account that has been
    /// invalidated or deleted. Existing waiters still hold their `Arc` so this
    /// does not disrupt in-flight lock acquisitions.
    pub fn forget(&self, account_id: i64) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.remove(&account_id);
        }
    }
}

impl Default for OAuthRefreshGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide refresh guard used by production call sites.
pub fn guard() -> &'static OAuthRefreshGuard {
    static GUARD: OnceLock<OAuthRefreshGuard> = OnceLock::new();
    GUARD.get_or_init(OAuthRefreshGuard::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::Barrier;
    use tokio::time::{Duration, sleep, timeout};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refresh_same_account_serializes() {
        let guard = Arc::new(OAuthRefreshGuard::new());
        let current = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));

        let mut joins = Vec::new();
        for _ in 0..5 {
            let guard = Arc::clone(&guard);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            joins.push(tokio::spawn(async move {
                let _lock = guard.lock(1).await;
                let c = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(c, Ordering::SeqCst);
                sleep(Duration::from_millis(5)).await;
                current.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for j in joins {
            j.await.unwrap();
        }

        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "same-account refreshes must be serialized"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn concurrent_refresh_different_accounts_parallel() {
        let guard = Arc::new(OAuthRefreshGuard::new());
        let barrier = Arc::new(Barrier::new(5));

        let mut joins = Vec::new();
        for i in 1..=5 {
            let guard = Arc::clone(&guard);
            let barrier = Arc::clone(&barrier);
            joins.push(tokio::spawn(async move {
                let _lock = guard.lock(i).await;
                // All five tasks must be able to hold their lock simultaneously.
                // If the guard mistakenly serialized across accounts the barrier
                // would deadlock and the outer timeout would fire.
                barrier.wait().await;
            }));
        }

        let result = timeout(Duration::from_secs(2), async {
            for j in joins {
                j.await.unwrap();
            }
        })
        .await;

        assert!(
            result.is_ok(),
            "different accounts must not serialize — barrier deadlocked"
        );
    }

    #[tokio::test]
    async fn forget_removes_idle_entries() {
        let guard = OAuthRefreshGuard::new();
        let _ = guard.lock(42).await; // populate registry
        guard.forget(42);
        assert!(guard.registry.lock().unwrap().is_empty());
    }
}
