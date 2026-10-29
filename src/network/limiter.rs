use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::error::VfsError;

/// Weighted quota costs for Google Drive v3 API operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaCost {
    /// `about.get` - 5 quota units
    AboutGet,
    /// `files.get` (metadata) - 5 quota units
    FilesGet,
    /// `changes.list` - 10 quota units
    ChangesList,
    /// `files.create` - 50 quota units
    FilesCreate,
    /// `files.update` - 50 quota units
    FilesUpdate,
    /// `files.list` - 100 quota units
    FilesList,
    /// `media stream` initiation / chunk transfer - 200 quota units
    MediaStream,
    /// Custom weighted cost (must be >= 1)
    Custom(u32),
}

impl QuotaCost {
    /// Returns the exact numeric quota units for this operation.
    #[must_use]
    pub const fn units(&self) -> u32 {
        match self {
            Self::AboutGet | Self::FilesGet => 5,
            Self::ChangesList => 10,
            Self::FilesCreate | Self::FilesUpdate => 50,
            Self::FilesList => 100,
            Self::MediaStream => 200,
            Self::Custom(n) => *n,
        }
    }

    /// Returns the quota cost as a `NonZeroU32` for `governor`.
    #[must_use]
    pub fn nonzero_units(&self) -> NonZeroU32 {
        let u = self.units();
        NonZeroU32::new(u).unwrap_or(NonZeroU32::MIN)
    }
}

/// RAII permit guard returned upon successful admission control.
///
/// Dropping this guard automatically releases the per-account concurrency permit
/// and updates active concurrency metrics.
pub struct ApiPermitGuard {
    _permit: OwnedSemaphorePermit,
    account_id: String,
    cost_units: u32,
    active_counter: Arc<AtomicUsize>,
}

impl ApiPermitGuard {
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    #[must_use]
    pub const fn cost_units(&self) -> u32 {
        self.cost_units
    }
}

impl Drop for ApiPermitGuard {
    fn drop(&mut self) {
        let prev = self.active_counter.fetch_sub(1, Ordering::SeqCst);
        debug!(
            account = %self.account_id,
            active_concurrency = prev.saturating_sub(1),
            cost_units = self.cost_units,
            "Released API permit guard"
        );
    }
}

/// Admission controller for a single linked Google account.
pub struct AccountLimiter {
    account_id: String,
    rate_limiter: Arc<DefaultDirectRateLimiter>,
    semaphore: Arc<Semaphore>,
    cooldown_until: Arc<RwLock<Option<Instant>>>,
    total_units_consumed: Arc<AtomicU64>,
    active_concurrency: Arc<AtomicUsize>,
    peak_concurrency: Arc<AtomicUsize>,
}

impl AccountLimiter {
    /// Creates a new per-account limiter with the specified quota per second and max concurrency.
    #[must_use]
    pub fn new(account_id: String, quota_per_sec: u32, max_concurrency: usize) -> Self {
        let nonzero_quota = NonZeroU32::new(quota_per_sec).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::per_second(nonzero_quota).allow_burst(nonzero_quota);
        let rate_limiter = Arc::new(RateLimiter::direct(quota));
        let semaphore = Arc::new(Semaphore::new(max_concurrency));

        Self {
            account_id,
            rate_limiter,
            semaphore,
            cooldown_until: Arc::new(RwLock::new(None)),
            total_units_consumed: Arc::new(AtomicU64::new(0)),
            active_concurrency: Arc::new(AtomicUsize::new(0)),
            peak_concurrency: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Trips the account circuit breaker for the given duration (e.g. following HTTP 429/5xx).
    pub async fn trip_cooldown(&self, duration: Duration) {
        let until = Instant::now() + duration;
        let mut guard = self.cooldown_until.write().await;
        *guard = Some(until);
        warn!(
            account = %self.account_id,
            duration_ms = duration.as_millis(),
            "Tripped account circuit breaker cooldown"
        );
    }

    /// Checks if the account is currently cooling down, and waits if necessary.
    pub async fn wait_if_cooling_down(&self) {
        let cooldown = {
            let guard = self.cooldown_until.read().await;
            *guard
        };

        if let Some(until) = cooldown {
            let now = Instant::now();
            if until > now {
                tokio::time::sleep(until - now).await;
            }
        }
    }

    /// Returns the total quota units consumed by this account since daemon start.
    #[must_use]
    pub fn total_units_consumed(&self) -> u64 {
        self.total_units_consumed.load(Ordering::Relaxed)
    }

    /// Returns the currently active in-flight request concurrency.
    #[must_use]
    pub fn current_concurrency(&self) -> usize {
        self.active_concurrency.load(Ordering::Relaxed)
    }

    /// Returns the peak in-flight request concurrency observed.
    #[must_use]
    pub fn peak_concurrency(&self) -> usize {
        self.peak_concurrency.load(Ordering::Relaxed)
    }
}

/// Three-layer multi-tier admission controller across all linked accounts.
pub struct MultiTierLimiter {
    global_limiter: Arc<DefaultDirectRateLimiter>,
    accounts: DashMap<String, Arc<AccountLimiter>>,
    per_account_quota_per_sec: u32,
    per_account_concurrency: usize,
    backpressure_timeout: Duration,
}

impl MultiTierLimiter {
    /// Default safe Google Drive quota ceiling: 4,500 quota units / second per account.
    pub const DEFAULT_PER_ACCOUNT_QUOTA_PER_SEC: u32 = 4_500;
    /// Default global IP rate limit: 10,000 quota units / second.
    pub const DEFAULT_GLOBAL_QUOTA_PER_SEC: u32 = 10_000;
    /// Strict cap of 24 concurrent in-flight HTTP streams per account.
    pub const DEFAULT_PER_ACCOUNT_CONCURRENCY: usize = 24;
    /// Bounded queue backpressure timeout: 25 seconds.
    pub const DEFAULT_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(25);

    /// Creates a new `MultiTierLimiter` with standard production parameters.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(
            Self::DEFAULT_PER_ACCOUNT_QUOTA_PER_SEC,
            Self::DEFAULT_GLOBAL_QUOTA_PER_SEC,
            Self::DEFAULT_PER_ACCOUNT_CONCURRENCY,
            Self::DEFAULT_BACKPRESSURE_TIMEOUT,
        )
    }

    /// Creates a new `MultiTierLimiter` with customizable parameters (useful for tests).
    #[must_use]
    pub fn with_config(
        per_account_quota_per_sec: u32,
        global_quota_per_sec: u32,
        per_account_concurrency: usize,
        backpressure_timeout: Duration,
    ) -> Self {
        let global_nonzero = NonZeroU32::new(global_quota_per_sec).unwrap_or(NonZeroU32::MIN);
        let global_quota = Quota::per_second(global_nonzero).allow_burst(global_nonzero);
        let global_limiter = Arc::new(RateLimiter::direct(global_quota));

        Self {
            global_limiter,
            accounts: DashMap::new(),
            per_account_quota_per_sec,
            per_account_concurrency,
            backpressure_timeout,
        }
    }

    /// Retrieves or registers the limiter for an account without holding map locks across I/O.
    #[must_use]
    pub fn get_or_create_account(&self, account_id: &str) -> Arc<AccountLimiter> {
        // Strict single-guard rule: drop DashMap guard immediately after lookup/insert
        if let Some(entry) = self.accounts.get(account_id) {
            return Arc::clone(entry.value());
        }

        self.accounts
            .entry(account_id.to_string())
            .or_insert_with(|| {
                Arc::new(AccountLimiter::new(
                    account_id.to_string(),
                    self.per_account_quota_per_sec,
                    self.per_account_concurrency,
                ))
            })
            .clone()
    }

    /// Trips the circuit breaker for a specific account.
    pub async fn trip_account_cooldown(&self, account_id: &str, duration: Duration) {
        let limiter = self.get_or_create_account(account_id);
        limiter.trip_cooldown(duration).await;
    }

    /// Three-layer admission controller:
    /// 1. Per-account circuit breaker check.
    /// 2. Layer 1: Per-account weighted token bucket (`governor`).
    /// 3. Layer 2: Global IP token bucket (`governor`).
    /// 4. Layer 3: Per-account concurrency semaphore (strict cap of 24).
    ///
    /// Wrapped in a backpressure timeout (default 25s). If budget cannot be acquired in time,
    /// fails gracefully with `VfsError::RateLimitTimeout` (`STATUS_DEVICE_BUSY` / `EBUSY`).
    pub async fn acquire_budget(
        &self,
        account_id: &str,
        cost: QuotaCost,
    ) -> Result<ApiPermitGuard, VfsError> {
        let limiter = self.get_or_create_account(account_id);
        let cost_units = cost.units();
        let nonzero_cost = cost.nonzero_units();
        let global_limiter = Arc::clone(&self.global_limiter);

        tokio::time::timeout(self.backpressure_timeout, async move {
            // Check & wait on circuit breaker cooldown if active
            limiter.wait_if_cooling_down().await;

            // Layer 1: Per-account token bucket
            if limiter.rate_limiter.until_n_ready(nonzero_cost).await.is_err() {
                return Err(VfsError::InternalFault(
                    "Requested quota units exceed maximum token bucket burst capacity".to_string(),
                ));
            }

            // Layer 2: Global IP rate limiter
            if global_limiter.until_n_ready(nonzero_cost).await.is_err() {
                return Err(VfsError::InternalFault(
                    "Requested quota units exceed global token bucket burst capacity".to_string(),
                ));
            }

            // Layer 3: Per-account concurrency semaphore (24 permits)
            let permit = match limiter.semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    return Err(VfsError::InternalFault(
                        "Account concurrency semaphore closed".to_string(),
                    ))
                }
            };

            // Update active concurrency metrics
            let current = limiter.active_concurrency.fetch_add(1, Ordering::SeqCst) + 1;
            let mut peak = limiter.peak_concurrency.load(Ordering::Relaxed);
            while current > peak {
                match limiter.peak_concurrency.compare_exchange_weak(
                    peak,
                    current,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => peak = actual,
                }
            }

            limiter
                .total_units_consumed
                .fetch_add(u64::from(cost_units), Ordering::Relaxed);

            Ok(ApiPermitGuard {
                _permit: permit,
                account_id: limiter.account_id.clone(),
                cost_units,
                active_counter: Arc::clone(&limiter.active_concurrency),
            })
        })
        .await
        .map_err(|_| VfsError::RateLimitTimeout)?
    }
}

impl Default for MultiTierLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_costs_mapping() {
        assert_eq!(QuotaCost::AboutGet.units(), 5);
        assert_eq!(QuotaCost::FilesGet.units(), 5);
        assert_eq!(QuotaCost::ChangesList.units(), 10);
        assert_eq!(QuotaCost::FilesCreate.units(), 50);
        assert_eq!(QuotaCost::FilesUpdate.units(), 50);
        assert_eq!(QuotaCost::FilesList.units(), 100);
        assert_eq!(QuotaCost::MediaStream.units(), 200);
        assert_eq!(QuotaCost::Custom(42).units(), 42);
    }

    #[tokio::test]
    async fn test_concurrency_semaphore_cap() {
        let limiter = Arc::new(MultiTierLimiter::with_config(
            10_000,
            20_000,
            3, // Max 3 concurrent permits
            Duration::from_millis(50),
        ));

        let guard1 = limiter.acquire_budget("acc1", QuotaCost::AboutGet).await;
        let guard2 = limiter.acquire_budget("acc1", QuotaCost::AboutGet).await;
        let guard3 = limiter.acquire_budget("acc1", QuotaCost::AboutGet).await;

        assert!(guard1.is_ok());
        assert!(guard2.is_ok());
        assert!(guard3.is_ok());

        // 4th concurrent request should time out because concurrency cap (3) is exhausted
        let guard4 = limiter.acquire_budget("acc1", QuotaCost::AboutGet).await;
        assert_eq!(guard4.err(), Some(VfsError::RateLimitTimeout));

        // Dropping guard1 frees 1 permit
        drop(guard1);

        // Now 5th request should succeed
        let guard5 = limiter.acquire_budget("acc1", QuotaCost::AboutGet).await;
        assert!(guard5.is_ok());
    }

    #[tokio::test]
    async fn test_circuit_breaker_cooldown() {
        let limiter = Arc::new(MultiTierLimiter::with_config(
            10_000,
            20_000,
            10,
            Duration::from_millis(500),
        ));

        // Trip cooldown for 100ms
        limiter
            .trip_account_cooldown("acc_cb", Duration::from_millis(100))
            .await;

        let start = Instant::now();
        let guard = limiter.acquire_budget("acc_cb", QuotaCost::FilesGet).await;
        let elapsed = start.elapsed();

        assert!(guard.is_ok());
        assert!(elapsed >= Duration::from_millis(90));
    }
}
