use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, info, warn};

use crate::error::VfsError;
use crate::vfs::prompt::ChunkPrompter;

/// Exactly 16 MB (16,777,216 bytes = 64 * 256 KB), matching Google Resumable Upload multiples.
pub const CHUNK_SIZE: u64 = 16 * 1024 * 1024;
/// Safety margin required per account before routing writes (16 MB).
pub const SAFETY_MARGIN_BYTES: u64 = 16 * 1024 * 1024;

/// In-memory quota accounting tracking remote usage and local in-flight reservations.
#[derive(Debug)]
pub struct AccountQuota {
    pub account_id: String,
    pub limit_bytes: u64,
    pub used_bytes: Arc<AtomicU64>,
    pub reserved_in_flight_bytes: Arc<AtomicU64>,
}

impl AccountQuota {
    #[must_use]
    pub fn new(account_id: String, limit_bytes: u64, used_bytes: u64) -> Self {
        Self {
            account_id,
            limit_bytes,
            used_bytes: Arc::new(AtomicU64::new(used_bytes)),
            reserved_in_flight_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Computes real effective free space: `limit - used - reserved_in_flight`.
    #[must_use]
    pub fn effective_free_bytes(&self) -> u64 {
        let used = self.used_bytes.load(Ordering::Relaxed);
        let in_flight = self.reserved_in_flight_bytes.load(Ordering::Relaxed);
        self.limit_bytes.saturating_sub(used).saturating_sub(in_flight)
    }

    /// Reserves bytes in-flight atomically if effective free space permits.
    pub fn try_reserve(&self, bytes: u64, safety_margin: u64) -> bool {
        let needed = bytes.saturating_add(safety_margin);
        let mut in_flight = self.reserved_in_flight_bytes.load(Ordering::SeqCst);
        loop {
            let used = self.used_bytes.load(Ordering::SeqCst);
            let free = self.limit_bytes.saturating_sub(used).saturating_sub(in_flight);
            if free < needed {
                return false;
            }
            let new_in_flight = in_flight.saturating_add(bytes);
            match self.reserved_in_flight_bytes.compare_exchange_weak(
                in_flight,
                new_in_flight,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => in_flight = actual,
            }
        }
    }

    /// Releases reserved in-flight bytes.
    pub fn release_reservation(&self, bytes: u64) {
        self.reserved_in_flight_bytes.fetch_sub(bytes, Ordering::SeqCst);
    }
}

/// RAII reservation guard for a single account.
///
/// Automatically decrements `reserved_in_flight_bytes` if dropped without being committed.
#[derive(Debug)]
pub struct SingleReservationGuard {
    pub account_id: String,
    pub reserved_bytes: u64,
    counter: Arc<AtomicU64>,
    used_counter: Option<Arc<AtomicU64>>,
    committed: bool,
}

impl SingleReservationGuard {
    #[must_use]
    pub fn new(
        account_id: String,
        reserved_bytes: u64,
        counter: Arc<AtomicU64>,
        used_counter: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            account_id,
            reserved_bytes,
            counter,
            used_counter,
            committed: false,
        }
    }

    /// Commits the reservation when an upload finishes successfully.
    pub fn commit(&mut self) {
        if !self.committed {
            self.committed = true;
            if let Some(used) = &self.used_counter {
                used.fetch_add(self.reserved_bytes, Ordering::SeqCst);
            }
            self.counter.fetch_sub(self.reserved_bytes, Ordering::SeqCst);
        }
    }
}

impl Drop for SingleReservationGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.counter.fetch_sub(self.reserved_bytes, Ordering::SeqCst);
            debug!(
                account = %self.account_id,
                released_bytes = self.reserved_bytes,
                "Released uncommitted single-account write reservation"
            );
        }
    }
}

/// Planned chunk assignment for multi-account striping.
#[derive(Debug, Clone)]
pub struct ChunkAssignment {
    pub chunk_index: usize,
    pub account_id: String,
    pub byte_offset: u64,
    pub byte_len: u64,
}

/// Ordered plan for striping an oversized file across accounts.
#[derive(Debug, Clone)]
pub struct StripedPlacementPlan {
    pub manifest_id: String,
    pub filename: String,
    pub total_size: u64,
    pub chunks: Vec<ChunkAssignment>,
    pub manifest_account_id: String,
}

/// RAII reservation guard for a multi-account striped file.
#[derive(Debug)]
pub struct StripedReservationGuard {
    pub plan: StripedPlacementPlan,
    reservations: Vec<SingleReservationGuard>,
}

impl StripedReservationGuard {
    #[must_use]
    pub fn new(plan: StripedPlacementPlan, reservations: Vec<SingleReservationGuard>) -> Self {
        Self { plan, reservations }
    }

    /// Commits all chunk reservations upon successful manifest creation.
    pub fn commit(&mut self) {
        for res in &mut self.reservations {
            res.commit();
        }
    }
}

/// Placement decision produced by the Placement Engine.
#[derive(Debug)]
pub enum PlacementDecision {
    /// Fits entirely within a single account's quota.
    SingleAccount(SingleReservationGuard),
    /// Split into 16 MB chunks across multiple accounts.
    StripedManifest(StripedReservationGuard),
}

/// Smart placement engine managing three-tier allocation and quota reservations.
pub struct PlacementEngine {
    accounts: DashMap<String, Arc<AccountQuota>>,
    safety_margin: u64,
}

impl PlacementEngine {
    /// Creates a new `PlacementEngine` with the standard 16 MB safety margin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            safety_margin: SAFETY_MARGIN_BYTES,
        }
    }

    /// Creates an engine with a custom safety margin (useful for testing small buffers).
    #[must_use]
    pub fn with_safety_margin(safety_margin: u64) -> Self {
        Self {
            accounts: DashMap::new(),
            safety_margin,
        }
    }

    /// Registers or updates an account's quota in RAM.
    pub fn register_account(&self, account_id: &str, limit_bytes: u64, used_bytes: u64) {
        if let Some(existing) = self.accounts.get(account_id) {
            existing.used_bytes.store(used_bytes, Ordering::SeqCst);
            existing.reserved_in_flight_bytes.store(0, Ordering::SeqCst);
            return;
        }

        self.accounts.insert(
            account_id.to_string(),
            Arc::new(AccountQuota::new(
                account_id.to_string(),
                limit_bytes,
                used_bytes,
            )),
        );
    }

    /// Updates the actual used bytes reported by Google for an account.
    pub fn update_used_bytes(&self, account_id: &str, used_bytes: u64) {
        if let Some(acc) = self.accounts.get(account_id) {
            acc.used_bytes.store(used_bytes, Ordering::Relaxed);
        }
    }

    /// Releases used bytes after a cloud file deletion.
    pub fn release_used_bytes(&self, account_id: &str, bytes: u64) {
        if let Some(acc) = self.accounts.get(account_id) {
            let current = acc.used_bytes.load(Ordering::Relaxed);
            acc.used_bytes.store(current.saturating_sub(bytes), Ordering::Relaxed);
        }
    }

    /// Returns the effective free space for a specific account.
    #[must_use]
    pub fn effective_free_space(&self, account_id: &str) -> Option<u64> {
        let guard = self.accounts.get(account_id)?;
        Some(guard.effective_free_bytes())
    }

    /// Returns the combined effective free space across all registered accounts.
    #[must_use]
    pub fn total_combined_free_space(&self) -> u64 {
        self.accounts
            .iter()
            .map(|entry| entry.value().effective_free_bytes())
            .sum()
    }

    /// Returns the combined total capacity limit across all registered accounts.
    #[must_use]
    pub fn total_pool_capacity(&self) -> u64 {
        self.accounts
            .iter()
            .map(|entry| entry.value().limit_bytes)
            .sum()
    }

    /// Resolves file placement across the three tiers.
    pub async fn resolve_placement<P: ChunkPrompter + ?Sized>(
        &self,
        size: u64,
        filename: &str,
        prompter: &P,
    ) -> Result<PlacementDecision, VfsError> {
        self.resolve_placement_with_affinity(size, filename, prompter, None)
            .await
    }

    /// Resolves file placement across the three tiers with optional parent directory account affinity.
    pub async fn resolve_placement_with_affinity<P: ChunkPrompter + ?Sized>(
        &self,
        size: u64,
        filename: &str,
        prompter: &P,
        preferred_account: Option<&str>,
    ) -> Result<PlacementDecision, VfsError> {
        // Collect snapshots of accounts and sort by effective free space descending
        let mut candidates: Vec<Arc<AccountQuota>> = self
            .accounts
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect();

        candidates.sort_by_key(|b| std::cmp::Reverse(b.effective_free_bytes()));

        if candidates.is_empty() {
            return Err(VfsError::DiskFull);
        }

        let max_single_free = candidates
            .first()
            .map(|a| a.effective_free_bytes())
            .unwrap_or(0);
        let total_pool_free: u64 = candidates.iter().map(|a| a.effective_free_bytes()).sum();

        // Tier 1a: Check preferred account affinity first (folder account affinity / file overwrite)
        if let Some(pref_id) = preferred_account
            && let Some(acc) = self.accounts.get(pref_id)
            && acc.try_reserve(size, self.safety_margin)
        {
            info!(
                account = %acc.account_id,
                size,
                filename,
                "Tier 1 (Affinity): Routed file to preferred Google account"
            );
            return Ok(PlacementDecision::SingleAccount(SingleReservationGuard::new(
                acc.account_id.clone(),
                size,
                Arc::clone(&acc.reserved_in_flight_bytes),
                Some(Arc::clone(&acc.used_bytes)),
            )));
        }

        // Tier 1b: Fits single account with safety margin
        for acc in &candidates {
            if acc.try_reserve(size, self.safety_margin) {
                info!(
                    account = %acc.account_id,
                    size,
                    filename,
                    "Tier 1: Routed file to single Google account"
                );
                return Ok(PlacementDecision::SingleAccount(SingleReservationGuard::new(
                    acc.account_id.clone(),
                    size,
                    Arc::clone(&acc.reserved_in_flight_bytes),
                    Some(Arc::clone(&acc.used_bytes)),
                )));
            }
        }

        // Tier 2: Exceeds single account, but fits combined storage pool
        let num_accounts = candidates.len() as u64;
        let required_pool_margin = num_accounts.saturating_mul(self.safety_margin);

        if total_pool_free >= size.saturating_add(required_pool_margin) {
            info!(
                filename,
                size,
                max_single_free,
                total_pool_free,
                "Tier 2: File exceeds single accounts; prompting user for multi-account chunking"
            );

            // Trigger non-blocking native permission modal dialog
            let approved = prompter
                .prompt_chunking(filename, size, max_single_free, total_pool_free)
                .await?;

            if !approved {
                warn!(filename, "User cancelled multi-account file chunking");
                return Err(VfsError::UserCancelledChunking);
            }

            // User granted permission: generate optimal 16 MB chunk striping plan
            let (plan, reservations) = self.build_striping_plan(&candidates, filename, size)?;
            info!(
                filename,
                manifest_id = %plan.manifest_id,
                chunks_count = plan.chunks.len(),
                "Generated multi-account chunk striping plan and reserved quota"
            );

            return Ok(PlacementDecision::StripedManifest(
                StripedReservationGuard::new(plan, reservations),
            ));
        }

        // Tier 3: Exceeds combined pool
        warn!(
            filename,
            size,
            total_pool_free,
            "Tier 3: File exceeds combined storage pool; rejecting with DiskFull"
        );
        Err(VfsError::DiskFull)
    }

    /// Plans 16 MB chunk distribution and reserves quota across accounts.
    fn build_striping_plan(
        &self,
        candidates: &[Arc<AccountQuota>],
        filename: &str,
        total_size: u64,
    ) -> Result<(StripedPlacementPlan, Vec<SingleReservationGuard>), VfsError> {
        let manifest_id = uuid::Uuid::new_v4().to_string();
        let mut chunks = Vec::new();
        let mut reservations = Vec::new();

        // Calculate chunk boundaries
        let mut byte_offset = 0u64;
        let mut chunk_index = 0usize;

        // Track local account allocations during planning
        let mut remaining_account_free: Vec<(Arc<AccountQuota>, u64)> = candidates
            .iter()
            .map(|a| {
                let free = a.effective_free_bytes().saturating_sub(self.safety_margin);
                (Arc::clone(a), free)
            })
            .collect();

        while byte_offset < total_size {
            let remaining_file = total_size - byte_offset;

            // Pick account with largest remaining free space
            remaining_account_free.sort_by_key(|(_, free)| std::cmp::Reverse(*free));

            let (assigned_account, free) = match remaining_account_free.first_mut() {
                Some(entry) if entry.1 >= 256 * 1024 => entry,
                _ => return Err(VfsError::DiskFull),
            };

            // Prefer full 16 MB chunks, packing tightly into remaining account capacity (multiple of 256 KB)
            let chunk_len = if *free >= CHUNK_SIZE {
                CHUNK_SIZE.min(remaining_file)
            } else {
                let packable = *free - (*free % (256 * 1024));
                CHUNK_SIZE.min(remaining_file).min(packable)
            };

            if chunk_len == 0 {
                return Err(VfsError::DiskFull);
            }

            chunks.push(ChunkAssignment {
                chunk_index,
                account_id: assigned_account.account_id.clone(),
                byte_offset,
                byte_len: chunk_len,
            });

            *free = free.saturating_sub(chunk_len);
            byte_offset += chunk_len;
            chunk_index += 1;
        }

        // Account with the most remaining space holds the small manifest JSON
        remaining_account_free.sort_by_key(|(_, free)| std::cmp::Reverse(*free));
        let manifest_account_id = remaining_account_free
            .first()
            .map(|(a, _)| a.account_id.clone())
            .unwrap_or_else(|| candidates[0].account_id.clone());

        // Perform atomic reservations for all assigned chunks per account
        let mut account_totals: std::collections::HashMap<String, (Arc<AccountQuota>, u64)> =
            std::collections::HashMap::new();

        for c in &chunks {
            if let Some(entry) = account_totals.get_mut(&c.account_id) {
                entry.1 += c.byte_len;
            } else if let Some(a) = candidates.iter().find(|cand| cand.account_id == c.account_id) {
                account_totals.insert(c.account_id.clone(), (Arc::clone(a), c.byte_len));
            }
        }

        for (acc_id, (acc, needed)) in account_totals {
            acc.reserved_in_flight_bytes
                .fetch_add(needed, Ordering::SeqCst);
            reservations.push(SingleReservationGuard::new(
                acc_id,
                needed,
                Arc::clone(&acc.reserved_in_flight_bytes),
                Some(Arc::clone(&acc.used_bytes)),
            ));
        }

        let plan = StripedPlacementPlan {
            manifest_id,
            filename: filename.to_string(),
            total_size,
            chunks,
            manifest_account_id,
        };

        Ok((plan, reservations))
    }
}

impl Default for PlacementEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::prompt::{AutoAcceptPrompter, AutoRejectPrompter};

    #[tokio::test]
    async fn test_tier1_single_account_placement() {
        let engine = PlacementEngine::with_safety_margin(16 * 1024 * 1024);
        let gb = 1024 * 1024 * 1024;

        // Account A: 15 GB limit, 5 GB used -> 10 GB free
        engine.register_account("acc_a", 15 * gb, 5 * gb);
        // Account B: 15 GB limit, 10 GB used -> 5 GB free
        engine.register_account("acc_b", 15 * gb, 10 * gb);

        // Upload 4 GB file -> fits in Account A (largest free space)
        let prompter = AutoRejectPrompter;
        let decision = engine
            .resolve_placement(4 * gb, "video.mp4", &prompter)
            .await
            .unwrap();

        match decision {
            PlacementDecision::SingleAccount(guard) => {
                assert_eq!(guard.account_id, "acc_a");
                assert_eq!(guard.reserved_bytes, 4 * gb);

                // Check in-flight reservation
                let free_after = engine.effective_free_space("acc_a").unwrap();
                assert_eq!(free_after, 6 * gb);
            }
            PlacementDecision::StripedManifest(_) => panic!("Expected single account placement"),
        }

        // Dropping guard uncommitted should restore effective free space
        assert_eq!(engine.effective_free_space("acc_a").unwrap(), 10 * gb);
    }

    #[tokio::test]
    async fn test_tier2_exceeds_single_fits_combined() {
        let engine = PlacementEngine::with_safety_margin(16 * 1024 * 1024);
        let gb = 1024 * 1024 * 1024;

        // Two accounts with 5 GB free each -> Max single: 5 GB, Total pool: 10 GB
        engine.register_account("acc_1", 10 * gb, 5 * gb);
        engine.register_account("acc_2", 10 * gb, 5 * gb);

        // Upload 7 GB file (exceeds single 5 GB, but fits in pool 10 GB)
        // With AutoRejectPrompter -> should return UserCancelledChunking
        let reject_prompter = AutoRejectPrompter;
        let err = engine
            .resolve_placement(7 * gb, "huge.iso", &reject_prompter)
            .await
            .unwrap_err();
        assert_eq!(err, VfsError::UserCancelledChunking);

        // With AutoAcceptPrompter -> should return StripedManifest
        let accept_prompter = AutoAcceptPrompter;
        let decision = engine
            .resolve_placement(7 * gb, "huge.iso", &accept_prompter)
            .await
            .unwrap();

        match decision {
            PlacementDecision::StripedManifest(guard) => {
                assert_eq!(guard.plan.filename, "huge.iso");
                assert_eq!(guard.plan.total_size, 7 * gb);
                assert!(!guard.plan.chunks.is_empty());
            }
            PlacementDecision::SingleAccount(_) => panic!("Expected striped manifest placement"),
        }
    }

    #[tokio::test]
    async fn test_tier3_exceeds_combined_pool() {
        let engine = PlacementEngine::with_safety_margin(16 * 1024 * 1024);
        let gb = 1024 * 1024 * 1024;

        engine.register_account("acc_small", gb, 500 * 1024 * 1024);

        // Request 100 GB -> exceeds pool
        let prompter = AutoAcceptPrompter;
        let err = engine
            .resolve_placement(100 * gb, "infinite.dat", &prompter)
            .await
            .unwrap_err();
        assert_eq!(err, VfsError::DiskFull);
    }

    #[tokio::test]
    async fn test_parent_folder_account_affinity() {
        let engine = PlacementEngine::with_safety_margin(16 * 1024 * 1024);
        let gb = 1024 * 1024 * 1024;

        // Account A: 10 GB free
        engine.register_account("acc_a", 15 * gb, 5 * gb);
        // Account B: 30 GB free (3x more than Account A!)
        engine.register_account("acc_b", 50 * gb, 20 * gb);

        let prompter = AutoRejectPrompter;

        // Upload into folder owned by Account A:
        // Even though Account B has 30 GB free and Account A only has 10 GB free,
        // it MUST reserve quota on Account A because of parent affinity!
        let decision = engine
            .resolve_placement_with_affinity(2 * gb, "file_in_a.pdf", &prompter, Some("acc_a"))
            .await
            .unwrap();

        match decision {
            PlacementDecision::SingleAccount(guard) => {
                assert_eq!(guard.account_id, "acc_a", "Must place on preferred Account A");
                assert_eq!(guard.reserved_bytes, 2 * gb);
            }
            PlacementDecision::StripedManifest(_) => panic!("Expected single account placement"),
        }

        // Now test fallback when preferred account has insufficient space:
        // Account A has 10 GB free. Upload 12 GB file with preferred_account = Some("acc_a").
        // Since Account A cannot hold 12 GB, it falls back to Account B (30 GB free)!
        let decision_fallback = engine
            .resolve_placement_with_affinity(12 * gb, "big_in_a.zip", &prompter, Some("acc_a"))
            .await
            .unwrap();

        match decision_fallback {
            PlacementDecision::SingleAccount(guard) => {
                assert_eq!(
                    guard.account_id, "acc_b",
                    "Must fallback to Account B when Account A is full"
                );
                assert_eq!(guard.reserved_bytes, 12 * gb);
            }
            PlacementDecision::StripedManifest(_) => panic!("Expected single account placement"),
        }
    }
}
