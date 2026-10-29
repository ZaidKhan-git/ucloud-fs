use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::error::VfsError;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trait for asking user permission to split an oversized file across accounts.
pub trait ChunkPrompter: Send + Sync {
    /// Asks the user whether to chunk an oversized file.
    ///
    /// Returns `Ok(true)` if granted, `Ok(false)` if denied.
    fn prompt_chunking<'a>(
        &'a self,
        filename: &'a str,
        file_size: u64,
        max_single_free: u64,
        total_pool_free: u64,
    ) -> BoxFuture<'a, Result<bool, VfsError>>;
}

/// Native OS modal prompt with 60-second deduplication cache.
///
/// Prevents Explorer from spamming the user with duplicate dialogs when repeating IRPs.
pub struct NativeModalPrompter {
    dedup_cache: DashMap<String, (bool, Instant)>,
    cache_ttl: Duration,
}

impl NativeModalPrompter {
    /// Default deduplication cache TTL: 60 seconds.
    pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(60);

    /// Creates a new `NativeModalPrompter`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dedup_cache: DashMap::new(),
            cache_ttl: Self::DEFAULT_CACHE_TTL,
        }
    }

    /// Creates a prompter with custom deduplication TTL (useful for testing).
    #[must_use]
    pub fn with_ttl(cache_ttl: Duration) -> Self {
        Self {
            dedup_cache: DashMap::new(),
            cache_ttl,
        }
    }

    /// Formats the official warning message per project specification.
    #[must_use]
    pub fn format_prompt_message(
        filename: &str,
        file_size: u64,
        max_single_free: u64,
        total_pool_free: u64,
    ) -> String {
        let size_gb = file_size as f64 / (1024.0 * 1024.0 * 1024.0);
        let max_single_gb = max_single_free as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_pool_gb = total_pool_free as f64 / (1024.0 * 1024.0 * 1024.0);

        format!(
            "The file '{filename}' ({size_gb:.2} GB) is larger than the free space on any single Google account \
            (Max single account free: {max_single_gb:.2} GB), but fits in your combined storage pool ({total_pool_gb:.2} GB free).\n\n\
            Do you grant permission to split it into 16 MB chunks across multiple accounts?\n\n\
            IMPORTANT NOTE: You will NOT be able to open or access this file from the Google Drive web interface \
            without downloading/copying it out through this unified drive."
        )
    }

    /// Clears the deduplication cache.
    pub fn clear_cache(&self) {
        self.dedup_cache.clear();
    }
}

impl Default for NativeModalPrompter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkPrompter for NativeModalPrompter {
    fn prompt_chunking<'a>(
        &'a self,
        filename: &'a str,
        file_size: u64,
        max_single_free: u64,
        total_pool_free: u64,
    ) -> BoxFuture<'a, Result<bool, VfsError>> {
        Box::pin(async move {
            let cache_key = format!("{filename}:{file_size}");

            // 1. Check deduplication cache
            {
                if let Some(entry) = self.dedup_cache.get(&cache_key) {
                    let (cached_decision, timestamp) = *entry.value();
                    if timestamp.elapsed() < self.cache_ttl {
                        info!(
                            filename,
                            cached_decision,
                            "Reusing recent chunking prompt decision from deduplication cache"
                        );
                        return Ok(cached_decision);
                    }
                }
            }

            let message = Self::format_prompt_message(
                filename,
                file_size,
                max_single_free,
                total_pool_free,
            );

            // 2. Spawn on a dedicated OS thread linked via oneshot channel to NEVER block Tokio workers
            let (tx, rx) = oneshot::channel();
            let fname_owned = filename.to_string();

            std::thread::spawn(move || {
                let res = rfd::MessageDialog::new()
                    .set_title("ucloud-fs: Oversized File Split Confirmation")
                    .set_description(&message)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();

                let approved = matches!(res, rfd::MessageDialogResult::Yes);
                let _ = tx.send(approved);
            });

            let approved = rx
                .await
                .map_err(|_| VfsError::InternalFault("Native dialog thread cancelled".to_string()))?;

            if approved {
                info!(filename = %fname_owned, "User approved multi-account file chunking");
            } else {
                warn!(filename = %fname_owned, "User rejected multi-account file chunking");
            }

            // 3. Cache the decision with current timestamp
            self.dedup_cache
                .insert(cache_key, (approved, Instant::now()));

            Ok(approved)
        })
    }
}

/// Headless prompter that automatically accepts chunking (for non-interactive runs / tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoAcceptPrompter;

impl ChunkPrompter for AutoAcceptPrompter {
    fn prompt_chunking<'a>(
        &'a self,
        _filename: &'a str,
        _file_size: u64,
        _max_single_free: u64,
        _total_pool_free: u64,
    ) -> BoxFuture<'a, Result<bool, VfsError>> {
        Box::pin(async move { Ok(true) })
    }
}

/// Headless prompter that automatically rejects chunking.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoRejectPrompter;

impl ChunkPrompter for AutoRejectPrompter {
    fn prompt_chunking<'a>(
        &'a self,
        _filename: &'a str,
        _file_size: u64,
        _max_single_free: u64,
        _total_pool_free: u64,
    ) -> BoxFuture<'a, Result<bool, VfsError>> {
        Box::pin(async move { Ok(false) })
    }
}
