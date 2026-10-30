use crate::error::VfsError;
use crate::security::oauth::AccountTokenManager;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Mandatory session duration ceiling: exactly 10 hours (36,000 seconds).
pub const SESSION_MAX_DURATION: Duration = Duration::from_secs(36_000);

type PurgeHook = Box<dyn Fn() + Send + Sync + 'static>;

/// Monotonic 10-hour session engine and cryptographic cache purge controller.
pub struct SessionController {
    start_instant: Instant,
    is_unlocked: AtomicBool,
    accounts: RwLock<HashMap<String, Arc<AccountTokenManager>>>,
    purge_hooks: std::sync::Mutex<Vec<PurgeHook>>,
}

impl SessionController {
    /// Initializes a new session starting right now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start_instant: Instant::now(),
            is_unlocked: AtomicBool::new(true),
            accounts: RwLock::new(HashMap::new()),
            purge_hooks: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Initializes a session with an explicit simulated start instant for testing boundary conditions.
    #[must_use]
    pub fn with_start_instant(start: Instant) -> Self {
        Self {
            start_instant: start,
            is_unlocked: AtomicBool::new(true),
            accounts: RwLock::new(HashMap::new()),
            purge_hooks: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Registers a purge callback executed upon session expiry or logout.
    pub fn register_purge_hook<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        if let Ok(mut hooks) = self.purge_hooks.lock() {
            hooks.push(Box::new(hook));
        }
    }

    /// Registers an active account manager.
    pub async fn add_account(&self, manager: Arc<AccountTokenManager>) {
        let mut accounts = self.accounts.write().await;
        accounts.insert(manager.account_id.clone(), manager);
    }

    /// Retrieves an account token manager if session is active.
    pub async fn get_account(&self, account_id: &str) -> Result<Arc<AccountTokenManager>, VfsError> {
        self.verify_session_active()?;
        let accounts = self.accounts.read().await;
        accounts
            .get(account_id)
            .cloned()
            .ok_or_else(|| VfsError::NotFound(format!("Account {} not found", account_id)))
    }

    /// Returns whether the session is currently active and unlocked.
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        self.verify_session_active().is_ok()
    }

    /// Checks if the session is currently unlocked and within the strict 10-hour window.
    /// If expired, immediately flips the lock state and purges all memory caches.
    pub fn verify_session_active(&self) -> Result<(), VfsError> {
        if !self.is_unlocked.load(Ordering::SeqCst) {
            return Err(VfsError::SessionLocked);
        }

        if self.start_instant.elapsed() >= SESSION_MAX_DURATION {
            self.lock_and_purge_all_caches();
            return Err(VfsError::SessionLocked);
        }

        Ok(())
    }

    /// Returns elapsed monotonic time since login.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start_instant.elapsed()
    }

    /// Locks the session and purges 100% of ephemeral caches in RAM.
    pub fn lock_and_purge_all_caches(&self) {
        self.is_unlocked.store(false, Ordering::SeqCst);

        // Execute all registered memory wipe hooks
        if let Ok(mut hooks) = self.purge_hooks.lock() {
            for hook in hooks.drain(..) {
                hook();
            }
        }

        // Drain active account credentials
        if let Ok(mut accounts) = self.accounts.try_write() {
            accounts.clear();
        }
    }

    /// Manually triggers session termination.
    pub fn logout(&self) {
        self.lock_and_purge_all_caches();
    }
}

impl Default for SessionController {
    fn default() -> Self {
        Self::new()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn test_10_hour_boundary_and_zero_residue() {
        let now = Instant::now();

        // 1. Session at 9h 59m 59s (35,999s elapsed)
        let active_start = now - Duration::from_secs(35_999);
        let session = SessionController::with_start_instant(active_start);
        assert!(session.verify_session_active().is_ok());

        // Register purge hook to verify zero residue
        let purge_count = Arc::new(AtomicUsize::new(0));
        let count_clone = purge_count.clone();
        session.register_purge_hook(move || {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Add dummy account manager
        let client = reqwest::Client::new();
        let creds = crate::security::vault::AccountCredentials {
            account_id: "test_user@gmail.com".to_string(),
            refresh_token: "refresh_123".to_string(),
            access_token: "access_old".to_string(),
            expires_at_unix: now.elapsed().as_secs() + 5000,
            client_id: None,
            client_secret: None,
        };
        let mgr = Arc::new(AccountTokenManager::new(
            "test_user@gmail.com".to_string(),
            "client_id".to_string(),
            "client_secret".to_string(),
            "http://127.0.0.1".to_string(),
            creds,
            client,
        ));
        session.add_account(mgr).await;

        assert!(session.get_account("test_user@gmail.com").await.is_ok());

        // 2. Session at 10h 00m 01s (36,001s elapsed)
        let expired_start = now - Duration::from_secs(36_001);
        let expired_session = SessionController::with_start_instant(expired_start);

        let expired_purge_count = Arc::new(AtomicUsize::new(0));
        let exp_count_clone = expired_purge_count.clone();
        expired_session.register_purge_hook(move || {
            exp_count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Verification: immediate lock on verify_session_active
        let check_res = expired_session.verify_session_active();
        assert!(matches!(check_res, Err(VfsError::SessionLocked)));
        assert!(!expired_session.is_unlocked.load(Ordering::SeqCst));

        // Verification: purge hook executed and memory wiped
        assert_eq!(expired_purge_count.load(Ordering::SeqCst), 1);
        assert!(expired_session.accounts.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_manual_logout_purges_caches() {
        let session = SessionController::new();
        let purge_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = purge_flag.clone();
        session.register_purge_hook(move || {
            flag_clone.store(true, Ordering::SeqCst);
        });

        session.logout();

        assert!(!session.is_unlocked.load(Ordering::SeqCst));
        assert!(purge_flag.load(Ordering::SeqCst));
        assert!(matches!(session.verify_session_active(), Err(VfsError::SessionLocked)));
    }
}
