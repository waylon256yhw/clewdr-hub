use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use crate::error::ClewdrError;

struct UserLimiter {
    concurrency: Arc<Semaphore>,
    rpm: std::sync::Mutex<VecDeque<Instant>>,
    max_concurrent: u32,
    rpm_limit: u32,
}

/// RAII guard holding a per-user concurrency permit.
/// Dropped when the response (including streaming) completes.
/// Wrapped in Arc to satisfy http::Extensions Clone requirement.
#[derive(Clone)]
pub struct UserPermit {
    _permit: Arc<OwnedSemaphorePermit>,
}

#[derive(Clone)]
pub struct UserLimiterMap {
    inner: Arc<RwLock<HashMap<i64, Arc<UserLimiter>>>>,
}

impl Default for UserLimiterMap {
    fn default() -> Self {
        Self::new()
    }
}

impl UserLimiterMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Remove a user's limiter entry (e.g. on user deletion/disable).
    pub async fn remove(&self, user_id: i64) {
        let mut map = self.inner.write().await;
        map.remove(&user_id);
    }

    /// Acquire a per-user concurrency permit and check RPM.
    /// Returns None if user_id/limits are not provided (legacy auth).
    pub async fn acquire(
        &self,
        user_id: i64,
        max_concurrent: i32,
        rpm_limit: i32,
    ) -> Result<UserPermit, ClewdrError> {
        let max_concurrent = max_concurrent.max(1) as u32;
        let rpm_limit = rpm_limit.max(1) as u32;

        let limiter = self.get_or_create(user_id, max_concurrent, rpm_limit).await;

        // Concurrency permit first — rejected requests must NOT consume RPM budget
        let permit = limiter
            .concurrency
            .clone()
            .try_acquire_owned()
            .map_err(|_| ClewdrError::UserConcurrencyExceeded)?;

        // RPM check (std Mutex — held briefly, no await inside)
        {
            let mut rpm = limiter.rpm.lock().unwrap();
            let now = Instant::now();
            while rpm
                .front()
                .is_some_and(|t| now.duration_since(*t).as_secs() >= 60)
            {
                rpm.pop_front();
            }
            if rpm.len() >= rpm_limit as usize {
                return Err(ClewdrError::RpmExceeded);
            }
            rpm.push_back(now);
        }

        Ok(UserPermit {
            _permit: Arc::new(permit),
        })
    }

    async fn get_or_create(
        &self,
        user_id: i64,
        max_concurrent: u32,
        rpm_limit: u32,
    ) -> Arc<UserLimiter> {
        // Fast path: read lock
        {
            let map = self.inner.read().await;
            if let Some(limiter) = map.get(&user_id) {
                // If policy limits changed, we'll replace below
                if limiter.max_concurrent == max_concurrent && limiter.rpm_limit == rpm_limit {
                    return limiter.clone();
                }
            }
        }

        // Slow path: write lock, create or replace
        let mut map = self.inner.write().await;
        // Double-check after acquiring write lock
        if let Some(limiter) = map.get(&user_id)
            && limiter.max_concurrent == max_concurrent
            && limiter.rpm_limit == rpm_limit
        {
            return limiter.clone();
        }

        let limiter = Arc::new(UserLimiter {
            concurrency: Arc::new(Semaphore::new(max_concurrent as usize)),
            rpm: std::sync::Mutex::new(VecDeque::new()),
            max_concurrent,
            rpm_limit,
        });
        map.insert(user_id, limiter.clone());
        limiter
    }
}
