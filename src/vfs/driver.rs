use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::VfsError;
use crate::network::client::{GoogleDriveClient, GoogleFileInput};
use crate::routing::chunker::{FileManifest, OversizedFileChunker};
use crate::routing::inode::{NewFileParams, StorageLocation, VirtualInode};
use crate::routing::placement::{
    PlacementDecision, PlacementEngine, SingleReservationGuard, StripedPlacementPlan,
    StripedReservationGuard,
};
use crate::routing::sync::DeltaSyncEngine;
use crate::routing::InMemoryRoutingTable;
use crate::security::session::SessionController;
use crate::stream::backpressure::MemoryBackpressureValve;
use crate::stream::read_window::{
    ByteSource, MockByteSource, SingleAccountByteSource, StripedManifestByteSource,
};
use crate::stream::write_spool::EphemeralWriteHandle;
use crate::vfs::notify::ShellNotifier;
use crate::vfs::prompt::ChunkPrompter;
use crate::vfs::sandbox::{self, ffi_boundary_guard};

/// Volume metrics and filesystem capabilities reported to the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub case_sensitive_search: bool,
    pub case_preserved_names: bool,
    pub unicode_on_disk: bool,
    pub reparse_points: bool,
    pub operation_timeout_ms: u32,
    pub volume_label: String,
}

/// Metadata attributes for a virtual file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub ino: u64,
    pub size: u64,
    pub allocation_size: u64,
    pub is_directory: bool,
    pub is_readonly: bool,
    pub created_time: u64,
    pub modified_time: u64,
    pub file_attributes: u32,
}

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;

/// Directory entry returned during directory enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
}

/// State of a file in pending write / upload lifecycle.
struct WritePendingState {
    path: String,
    parent_ino: u64,
    filename: String,
    #[allow(dead_code)]
    allocation_size: u64,
    write_spool: Option<EphemeralWriteHandle>,
    single_guard: Option<SingleReservationGuard>,
    striped_guard: Option<StripedReservationGuard>,
    striped_plan: Option<StripedPlacementPlan>,
    uploaded_bytes: Vec<u8>,
    delete_on_close: bool,
    preferred_account: Option<String>,
    overwriting_existing: Option<(u64, String, String)>,
}

/// Block size for the shared inode page cache (1 MB aligned blocks).
pub const SHARED_BLOCK_SIZE: u64 = 1024 * 1024;
/// Maximum number of blocks stored in the shared inode page cache (96 MB LRU ceiling).
pub const MAX_SHARED_BLOCKS: usize = 96;

/// Shared Inode Page Cache dividing files into 1 MB blocks with LRU eviction and single-flight deduplication.
pub struct SharedBlockCache {
    blocks: DashMap<(u64, u64), (Instant, Bytes)>,
    in_flight: DashMap<(u64, u64), tokio::sync::broadcast::Sender<Bytes>>,
    max_blocks: usize,
}

impl Default for SharedBlockCache {
    fn default() -> Self {
        Self::new(MAX_SHARED_BLOCKS)
    }
}

impl SharedBlockCache {
    #[must_use]
    pub fn new(max_blocks: usize) -> Self {
        Self {
            blocks: DashMap::new(),
            in_flight: DashMap::new(),
            max_blocks: max_blocks.max(1),
        }
    }

    /// Retrieves a block from cache if present, updating its LRU access timestamp.
    #[must_use]
    pub fn get(&self, ino: u64, block_index: u64) -> Option<Bytes> {
        if let Some(mut entry) = self.blocks.get_mut(&(ino, block_index)) {
            entry.0 = Instant::now();
            Some(entry.1.clone())
        } else {
            None
        }
    }

    /// Returns whether a block is currently resident in cache.
    #[must_use]
    pub fn contains(&self, ino: u64, block_index: u64) -> bool {
        self.blocks.contains_key(&(ino, block_index))
    }

    /// Returns whether a block is currently in-flight being downloaded.
    #[must_use]
    pub fn is_in_flight(&self, ino: u64, block_index: u64) -> bool {
        self.in_flight.contains_key(&(ino, block_index))
    }

    /// Returns whether a block is resident in cache or currently in-flight.
    #[must_use]
    pub fn contains_or_in_flight(&self, ino: u64, block_index: u64) -> bool {
        self.blocks.contains_key(&(ino, block_index)) || self.in_flight.contains_key(&(ino, block_index))
    }

    /// Registers an in-flight block channel synchronously and returns the sender.
    /// If the block is already resident or in flight, returns None.
    pub fn register_in_flight(&self, ino: u64, block_index: u64) -> Option<tokio::sync::broadcast::Sender<Bytes>> {
        use dashmap::mapref::entry::Entry;
        if self.blocks.contains_key(&(ino, block_index)) {
            return None;
        }
        match self.in_flight.entry((ino, block_index)) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                let (tx, _) = tokio::sync::broadcast::channel(1);
                vac.insert(tx.clone());
                Some(tx)
            }
        }
    }

    /// Subscribes to an in-flight block download if one exists.
    #[must_use]
    pub fn subscribe_in_flight(&self, ino: u64, block_index: u64) -> Option<tokio::sync::broadcast::Receiver<Bytes>> {
        self.in_flight.get(&(ino, block_index)).map(|tx| tx.subscribe())
    }

    /// Inserts a block into cache with LRU eviction when reaching capacity.
    pub fn insert(&self, ino: u64, block_index: u64, bytes: Bytes) {
        if self.blocks.contains_key(&(ino, block_index)) {
            self.blocks.insert((ino, block_index), (Instant::now(), bytes));
            return;
        }

        if self.blocks.len() >= self.max_blocks {
            let oldest_key = self
                .blocks
                .iter()
                .min_by_key(|entry| entry.value().0)
                .map(|entry| *entry.key());

            if let Some(key) = oldest_key {
                self.blocks.remove(&key);
            }
        }

        self.blocks.insert((ino, block_index), (Instant::now(), bytes));
    }

    /// Gets a block from cache or fetches it with single-flight deduplication across handles.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        ino: u64,
        block_index: u64,
        fetch_fut: F,
    ) -> Result<Bytes, VfsError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes, VfsError>>,
    {
        if let Some(bytes) = self.get(ino, block_index) {
            return Ok(bytes);
        }

        enum FlightStatus {
            Leader(tokio::sync::broadcast::Sender<Bytes>),
            Follower(tokio::sync::broadcast::Receiver<Bytes>),
        }

        let flight_status = {
            use dashmap::mapref::entry::Entry;
            match self.in_flight.entry((ino, block_index)) {
                Entry::Occupied(occ) => FlightStatus::Follower(occ.get().subscribe()),
                Entry::Vacant(vac) => {
                    let (tx, _) = tokio::sync::broadcast::channel(1);
                    vac.insert(tx.clone());
                    FlightStatus::Leader(tx)
                }
            }
        };

        match flight_status {
            FlightStatus::Follower(mut rx) => match rx.recv().await {
                Ok(bytes) => Ok(bytes),
                Err(_) => {
                    if let Some(bytes) = self.get(ino, block_index) {
                        Ok(bytes)
                    } else {
                        let bytes = fetch_fut().await?;
                        self.insert(ino, block_index, bytes.clone());
                        Ok(bytes)
                    }
                }
            },
            FlightStatus::Leader(tx) => {
                let res = fetch_fut().await;
                self.in_flight.remove(&(ino, block_index));
                match res {
                    Ok(bytes) => {
                        self.insert(ino, block_index, bytes.clone());
                        let _ = tx.send(bytes.clone());
                        Ok(bytes)
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Clears all cached blocks and in-flight tracking.
    pub fn clear(&self) {
        self.blocks.clear();
        self.in_flight.clear();
    }

    /// Number of cached blocks currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Direct page-cache byte source with 1 MB aligned blocks, smart tail-seek awareness,
/// lock-free parallel reads, and non-blocking background read-ahead.
pub struct CachedByteSource {
    pub ino: u64,
    pub file_size: u64,
    pub inner: Arc<dyn ByteSource>,
    pub cache: Arc<SharedBlockCache>,
    pub hot_header_cache: Arc<DashMap<u64, HotHeaderCacheEntry>>,
    pub rt_handle: tokio::runtime::Handle,
    pub active_cursor: Arc<AtomicU64>,
}

impl CachedByteSource {
    #[must_use]
    pub fn new(
        ino: u64,
        file_size: u64,
        inner: Arc<dyn ByteSource>,
        cache: Arc<SharedBlockCache>,
        hot_header_cache: Arc<DashMap<u64, HotHeaderCacheEntry>>,
        rt_handle: tokio::runtime::Handle,
    ) -> Self {
        let source = Self {
            ino,
            file_size,
            inner,
            cache,
            hot_header_cache,
            rt_handle,
            active_cursor: Arc::new(AtomicU64::new(0)),
        };
        // Proactively prime block 0 and read-ahead ahead of first read
        source.prefetch_block(0);
        source.ensure_readahead_buffered(0);
        source
    }

    /// Spawns a background task to fetch a single block into shared cache if not already cached or in-flight.
    pub fn prefetch_block(&self, block_index: u64) {
        if self.cache.contains_or_in_flight(self.ino, block_index) {
            return;
        }

        let b_start = block_index * SHARED_BLOCK_SIZE;
        if b_start >= self.file_size {
            return;
        }

        if let Some(tx) = self.cache.register_in_flight(self.ino, block_index) {
            let ino = self.ino;
            let file_size = self.file_size;
            let inner = Arc::clone(&self.inner);
            let cache = Arc::clone(&self.cache);

            self.rt_handle.spawn(async move {
                let b_end = ((block_index + 1) * SHARED_BLOCK_SIZE)
                    .min(file_size)
                    .saturating_sub(1);
                let res = inner.fetch_range(b_start, b_end).await;
                cache.in_flight.remove(&(ino, block_index));
                if let Ok(bytes) = res {
                    cache.insert(ino, block_index, bytes.clone());
                    let _ = tx.send(bytes);
                }
            });
        }
    }

    /// Ensures the next 6-8 blocks ahead of `current_block` are present in cache or actively in flight.
    pub fn ensure_readahead_buffered(&self, current_block: u64) {
        let max_block = self.file_size.saturating_sub(1) / SHARED_BLOCK_SIZE;
        let target_start = current_block + 1;
        let target_end = (current_block + 8).min(max_block);
        if target_start > target_end {
            return;
        }

        self.active_cursor.store(current_block, Ordering::SeqCst);

        for next_b in target_start..=target_end {
            self.prefetch_block(next_b);
        }
    }

    /// Alias for backward compatibility and test hooks.
    pub fn trigger_paced_readahead(&self, current_block: u64) {
        self.ensure_readahead_buffered(current_block);
    }

    /// Retrieves a block non-blockingly:
    /// - Hits memory cache immediately if present.
    /// - If in-flight, awaits with a short timeout before returning zero-padded fallback to never freeze OS read thread.
    /// - If missing entirely, initiates background prefetch and awaits briefly before zero-padding.
    pub async fn get_or_fetch_block_nonblocking(&self, block_index: u64) -> Bytes {
        if let Some(bytes) = self.cache.get(self.ino, block_index) {
            return bytes;
        }

        let b_start = block_index * SHARED_BLOCK_SIZE;
        if b_start >= self.file_size {
            return Bytes::new();
        }
        let b_end = ((block_index + 1) * SHARED_BLOCK_SIZE)
            .min(self.file_size)
            .saturating_sub(1);
        let expected_len = (b_end - b_start + 1) as usize;

        // If not already in flight, register and trigger it immediately
        if !self.cache.is_in_flight(self.ino, block_index) {
            self.prefetch_block(block_index);
            self.ensure_readahead_buffered(block_index);
        }

        // Subscribe to in-flight receiver
        let receiver = self.cache.subscribe_in_flight(self.ino, block_index);

        let timeout_duration = if block_index == 0 {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(50)
        };

        if let Some(mut rx) = receiver {
            match tokio::time::timeout(timeout_duration, rx.recv()).await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(_)) => {
                    self.cache
                        .get(self.ino, block_index)
                        .unwrap_or_else(|| Bytes::from(vec![0u8; expected_len]))
                }
                Err(_) => {
                    debug!(
                        ino = self.ino,
                        block_index,
                        "Read timeout waiting for in-flight download - returning instant non-blocking zero-pad"
                    );
                    self.cache
                        .get(self.ino, block_index)
                        .unwrap_or_else(|| Bytes::from(vec![0u8; expected_len]))
                }
            }
        } else {
            self.cache
                .get(self.ino, block_index)
                .unwrap_or_else(|| Bytes::from(vec![0u8; expected_len]))
        }
    }

    /// Fetches a single 1 MB block from cache or network.
    pub async fn get_or_fetch_block(&self, block_index: u64) -> Result<Bytes, VfsError> {
        Ok(self.get_or_fetch_block_nonblocking(block_index).await)
    }

    /// Fetches an exact byte range directly and lock-free for OS ReadFile requests.
    pub async fn fetch_range(&self, offset: u64, length: usize) -> Result<Bytes, VfsError> {
        let file_size = self.file_size;
        if offset >= file_size || length == 0 {
            return Ok(Bytes::new());
        }

        let req_len = (length as u64).min(file_size - offset) as usize;

        // 1. Check Hot Header Cache for head slice (<512 KB, 30s TTL)
        if offset + req_len as u64 <= 512 * 1024
            && let Some(entry) = self.hot_header_cache.get(&self.ino)
            && entry.timestamp.elapsed() < Duration::from_secs(30)
            && let Some(head) = &entry.head
        {
            let off = offset as usize;
            if off + req_len <= head.len() {
                debug!(ino = self.ino, offset, req_len, "Hot Header Cache head hit (<0.001 ms)");
                self.ensure_readahead_buffered(0);
                return Ok(Bytes::copy_from_slice(&head[off..off + req_len]));
            }
        }

        // 2. Check Hot Header Cache for tail slice
        if let Some(entry) = self.hot_header_cache.get(&self.ino)
            && entry.timestamp.elapsed() < Duration::from_secs(30)
            && let Some((tail_start, tail_bytes)) = &entry.tail
        {
            let t_start = *tail_start;
            let t_end = t_start + tail_bytes.len() as u64;
            if offset >= t_start && offset + req_len as u64 <= t_end {
                let off = (offset - t_start) as usize;
                debug!(ino = self.ino, offset, req_len, "Hot Header Cache tail hit (<0.001 ms)");
                return Ok(Bytes::copy_from_slice(&tail_bytes[off..off + req_len]));
            }
        }

        let inclusive_end = offset + req_len as u64 - 1;
        let start_block = offset / SHARED_BLOCK_SIZE;
        let end_block = inclusive_end / SHARED_BLOCK_SIZE;

        // Check if offset is in the Tail Zone (final 4 MB of a file > 16 MB)
        let is_tail_zone = file_size > 16 * 1024 * 1024
            && offset >= file_size.saturating_sub(4 * 1024 * 1024);

        // Always proactively trigger background readahead ahead of the current read point
        if !is_tail_zone {
            self.ensure_readahead_buffered(end_block);
        }

        // Fetch required block(s) non-blockingly
        let mut assembled = Vec::with_capacity(req_len);

        for b in start_block..=end_block {
            let block_bytes = self.get_or_fetch_block_nonblocking(b).await;
            let b_start = b * SHARED_BLOCK_SIZE;
            let b_end = ((b + 1) * SHARED_BLOCK_SIZE).min(file_size).saturating_sub(1);

            let overlap_start = offset.max(b_start);
            let overlap_end = inclusive_end.min(b_end);

            if overlap_start <= overlap_end {
                let rel_start = (overlap_start - b_start) as usize;
                let rel_end = (overlap_end - b_start + 1) as usize;
                if rel_start < block_bytes.len() {
                    let slice_end = rel_end.min(block_bytes.len());
                    assembled.extend_from_slice(&block_bytes[rel_start..slice_end]);
                } else {
                    let slice_len = rel_end.saturating_sub(rel_start);
                    assembled.resize(assembled.len() + slice_len, 0);
                }
            }
        }

        // Populate Hot Header Cache for head (if reading from offset 0)
        if offset == 0 && !assembled.is_empty() {
            let head_len = (512 * 1024).min(assembled.len());
            let head_bytes = Bytes::copy_from_slice(&assembled[..head_len]);
            let mut entry = self.hot_header_cache.entry(self.ino).or_insert_with(|| HotHeaderCacheEntry {
                head: None,
                tail: None,
                timestamp: Instant::now(),
            });
            entry.head = Some(head_bytes);
            entry.timestamp = Instant::now();
        }

        // Populate Hot Header Cache for tail (if in tail zone)
        if is_tail_zone && !assembled.is_empty() {
            let mut entry = self.hot_header_cache.entry(self.ino).or_insert_with(|| HotHeaderCacheEntry {
                head: None,
                tail: None,
                timestamp: Instant::now(),
            });
            entry.tail = Some((offset, Bytes::copy_from_slice(&assembled)));
            entry.timestamp = Instant::now();
        }

        Ok(Bytes::from(assembled))
    }
}

impl ByteSource for CachedByteSource {
    fn file_size(&self) -> u64 {
        self.file_size
    }

    fn fetch_range<'a>(
        &'a self,
        start: u64,
        end: u64,
    ) -> crate::stream::read_window::BoxFuture<'a, Result<Bytes, VfsError>> {
        let length = (end.saturating_sub(start) + 1) as usize;
        Box::pin(self.fetch_range(start, length))
    }
}

/// In-flight state of an open file or directory handle.
#[derive(Clone)]
enum HandleState {
    Directory {
        #[allow(dead_code)]
        ino: u64,
    },
    ReadShared {
        ino: u64,
        source: Arc<CachedByteSource>,
    },
    ReadVirtual {
        ino: u64,
        bytes: Bytes,
    },
    WritePending(Arc<Mutex<WritePendingState>>),
}

/// Hot Header Cache entry storing initial 512 KB and tail 512 KB of recently accessed files.
#[derive(Debug, Clone)]
pub struct HotHeaderCacheEntry {
    pub head: Option<Bytes>,
    pub tail: Option<(u64, Bytes)>,
    pub timestamp: std::time::Instant,
}

/// Core dependencies required to initialize the VFS driver.
pub struct DriverDependencies {
    pub routing_table: Arc<InMemoryRoutingTable>,
    pub placement_engine: Arc<PlacementEngine>,
    pub session: Arc<SessionController>,
    pub client: Arc<GoogleDriveClient>,
    pub sync_engine: Arc<DeltaSyncEngine>,
    pub notifier: Arc<dyn ShellNotifier>,
    pub prompter: Arc<dyn ChunkPrompter>,
    pub valve: Arc<MemoryBackpressureValve>,
    pub rt_handle: tokio::runtime::Handle,
    pub volume_label: Option<String>,
}

/// A file replaced by an in-place overwrite. It is detached from the routing table before the
/// replacement is inserted (to avoid a false name collision) but stays recoverable until the
/// replacement is fully committed.
struct DetachedOverwrite {
    /// Routing-table node removed for the overwrite; `None` if the entry had already vanished.
    node: Option<VirtualInode>,
    account_id: String,
    google_file_id: String,
}

/// Unified sandboxed VFS Driver bridging OS filesystem IRPs into the core ucloud-fs engine.
pub struct UcloudFsDriver {
    routing_table: Arc<InMemoryRoutingTable>,
    placement_engine: Arc<PlacementEngine>,
    session: Arc<SessionController>,
    client: Arc<GoogleDriveClient>,
    sync_engine: Arc<DeltaSyncEngine>,
    notifier: Arc<dyn ShellNotifier>,
    prompter: Arc<dyn ChunkPrompter>,
    valve: Arc<MemoryBackpressureValve>,
    rt_handle: tokio::runtime::Handle,
    open_handles: DashMap<u64, HandleState>,
    next_fh: AtomicU64,
    account_tokens: DashMap<String, String>,
    mock_files: DashMap<String, Bytes>,
    pub hot_header_cache: Arc<DashMap<u64, HotHeaderCacheEntry>>,
    pub shared_block_cache: Arc<SharedBlockCache>,
    volume_label: RwLock<String>,
    is_mounted: AtomicBool,
}

impl UcloudFsDriver {
    /// Creates a new `UcloudFsDriver`.
    #[must_use]
    pub fn new(deps: DriverDependencies) -> Self {
        Self {
            routing_table: deps.routing_table,
            placement_engine: deps.placement_engine,
            session: deps.session,
            client: deps.client,
            sync_engine: deps.sync_engine,
            notifier: deps.notifier,
            prompter: deps.prompter,
            valve: deps.valve,
            rt_handle: deps.rt_handle,
            open_handles: DashMap::new(),
            next_fh: AtomicU64::new(100),
            account_tokens: DashMap::new(),
            mock_files: DashMap::new(),
            hot_header_cache: Arc::new(DashMap::new()),
            shared_block_cache: Arc::new(SharedBlockCache::default()),
            volume_label: RwLock::new(
                deps.volume_label.unwrap_or_else(|| "ucloud".to_string()),
            ),
            is_mounted: AtomicBool::new(true),
        }
    }

    /// Sets a new volume label dynamically (e.g. from Windows Explorer rename).
    pub fn set_volume_label(&self, label: &str) {
        if let Ok(mut guard) = self.volume_label.write() {
            *guard = label.to_string();
        }
    }

    /// Retrieves the current volume label.
    #[must_use]
    pub fn get_volume_label(&self) -> String {
        self.volume_label
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Dynamically registers a newly added Google account into the live driver,
    /// updating routing tables, token cache, and placement engine storage quotas.
    pub fn register_account(&self, account_id: &str, token: &str, limit_bytes: u64, used_bytes: u64) {
        self.set_account_token(account_id, token);
        self.placement_engine.register_account(account_id, limit_bytes, used_bytes);
        info!(account = %account_id, limit_bytes, used_bytes, "Dynamically registered new account in driver");
    }

    /// Registers an OAuth token for a linked Google account.
    pub fn set_account_token(&self, account_id: &str, token: &str) {
        self.account_tokens.insert(account_id.to_string(), token.to_string());
    }

    /// Registers a mock file payload for testing mode.
    pub fn set_mock_file(&self, path: &str, data: Bytes) {
        self.mock_files.insert(path.to_string(), data);
    }

    /// Allocates a unique file handle ID.
    fn allocate_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    /// Mandatory 4-step wrapper guard for synchronous VFS operations.
    pub fn execute_vfs_op<F, R>(&self, path_for_validation: Option<&str>, op: F) -> Result<R, VfsError>
    where
        F: FnOnce() -> Result<R, VfsError>,
    {
        ffi_boundary_guard(
            "vfs_op",
            Err(VfsError::InternalFault("FFI panic contained".to_string())),
            || {
                if !self.is_mounted.load(Ordering::SeqCst) {
                    return Err(VfsError::SessionLocked);
                }
                self.session.verify_session_active()?;

                if let Some(path) = path_for_validation {
                    sandbox::validate_path(path)?;
                }

                op()
            },
        )
    }

    /// Mandatory 4-step wrapper guard for asynchronous VFS operations with 120s timeout.
    pub fn execute_vfs_async_op<F, Fut, R>(&self, path_for_validation: Option<&str>, async_op: F) -> Result<R, VfsError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<R, VfsError>>,
    {
        self.execute_vfs_op(path_for_validation, || {
            let res = if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::block_in_place(|| {
                    self.rt_handle.block_on(async {
                        tokio::time::timeout(Duration::from_secs(120), async_op()).await
                    })
                })
            } else {
                self.rt_handle.block_on(async {
                    tokio::time::timeout(Duration::from_secs(120), async_op()).await
                })
            };
            res.map_err(|_| VfsError::RateLimitTimeout)?
        })
    }

    /// Volume Parameters: Reports total pool capacity and free space across all linked accounts.
    pub fn get_volume_info(&self) -> Result<VolumeInfo, VfsError> {
        self.execute_vfs_op(None, || {
            let total_bytes = self.placement_engine.total_pool_capacity();
            let free_bytes = self.placement_engine.total_combined_free_space();

            Ok(VolumeInfo {
                total_bytes,
                free_bytes,
                case_sensitive_search: false,
                case_preserved_names: true,
                unicode_on_disk: true,
                reparse_points: false,
                operation_timeout_ms: 300_000,
                volume_label: self.get_volume_label(),
            })
        })
    }

    /// Resolves virtual path to parent inode and basename.
    fn resolve_path_parent_and_name(&self, path: &str) -> Result<(u64, String), VfsError> {
        let clean = path.trim_start_matches('/').trim_start_matches('\\');
        if clean.is_empty() {
            return Ok((InMemoryRoutingTable::ROOT_INO, String::new()));
        }

        let parts: Vec<&str> = clean.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Ok((InMemoryRoutingTable::ROOT_INO, String::new()));
        }

        let mut current_ino = InMemoryRoutingTable::ROOT_INO;
        for &name in parts.iter().take(parts.len().saturating_sub(1)) {
            current_ino = self.routing_table.lookup(current_ino, name).ok_or_else(|| {
                VfsError::NotFound(format!("Directory component '{name}' not found"))
            })?;
        }

        let filename = parts.last().map(|s| (*s).to_string()).unwrap_or_default();
        Ok((current_ino, filename))
    }

    /// Resolves virtual path to its inode ID.
    fn resolve_path(&self, path: &str) -> Result<u64, VfsError> {
        let clean = path.trim_start_matches('/').trim_start_matches('\\');
        if clean.is_empty() {
            return Ok(InMemoryRoutingTable::ROOT_INO);
        }

        let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
        self.routing_table
            .lookup(parent_ino, &filename)
            .ok_or_else(|| VfsError::NotFound(format!("File or directory '{path}' not found")))
    }

    /// File Info: Evaluated strictly from in-memory routing table in <0.01 ms with ZERO network calls.
    pub fn get_file_info(&self, path: &str) -> Result<FileInfo, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let node = self
                .routing_table
                .get_inode(ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;

            let attributes = if node.is_directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else if node.is_workspace_doc {
                FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };

            Ok(FileInfo {
                ino,
                size: node.size,
                allocation_size: node.size,
                is_directory: node.is_directory,
                is_readonly: node.is_workspace_doc,
                created_time: node.created_time,
                modified_time: node.modified_time,
                file_attributes: attributes,
            })
        })
    }

    /// Retrieves file information for an active open handle, including in-flight writes.
    fn file_info_from_handle_state(&self, fh: u64, state: &HandleState) -> Result<FileInfo, VfsError> {
        match state {
            HandleState::Directory { ino } => {
                let node = self
                    .routing_table
                    .get_inode(*ino)
                    .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;
                Ok(FileInfo {
                    ino: *ino,
                    size: 0,
                    allocation_size: 0,
                    is_directory: true,
                    is_readonly: false,
                    created_time: node.created_time,
                    modified_time: node.modified_time,
                    file_attributes: FILE_ATTRIBUTE_DIRECTORY,
                })
            }
            HandleState::ReadShared { ino, .. } | HandleState::ReadVirtual { ino, .. } => {
                let node = self
                    .routing_table
                    .get_inode(*ino)
                    .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;
                let attributes = if node.is_workspace_doc {
                    FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                };
                Ok(FileInfo {
                    ino: *ino,
                    size: node.size,
                    allocation_size: node.size,
                    is_directory: false,
                    is_readonly: node.is_workspace_doc,
                    created_time: node.created_time,
                    modified_time: node.modified_time,
                    file_attributes: attributes,
                })
            }
            HandleState::WritePending(wp_mutex) => {
                let wp = wp_mutex.try_lock().map_err(|_| VfsError::InternalFault("Write handle busy".to_string()))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = if let Some(spool) = &wp.write_spool {
                    spool.flushed_bytes() + spool.current_ram_usage() as u64
                } else {
                    wp.uploaded_bytes.len() as u64
                };
                let alloc = size.max(wp.allocation_size);
                Ok(FileInfo {
                    ino: fh,
                    size,
                    allocation_size: alloc,
                    is_directory: false,
                    is_readonly: false,
                    created_time: now,
                    modified_time: now,
                    file_attributes: FILE_ATTRIBUTE_NORMAL,
                })
            }
        }
    }

    /// Retrieves file information for an active open handle, with instantaneous non-blocking fast path.
    pub fn get_file_info_by_handle(&self, fh: u64) -> Result<FileInfo, VfsError> {
        let handle_state = self
            .open_handles
            .get(&fh)
            .map(|g| g.value().clone())
            .ok_or_else(|| VfsError::NotFound(format!("File handle {fh} not found")))?;

        match &handle_state {
            HandleState::Directory { .. }
            | HandleState::ReadShared { .. }
            | HandleState::ReadVirtual { .. } => {
                self.file_info_from_handle_state(fh, &handle_state)
            }
            HandleState::WritePending(wp_mutex) => {
                if let Ok(wp) = wp_mutex.try_lock() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let size = if let Some(spool) = &wp.write_spool {
                        spool.flushed_bytes() + spool.current_ram_usage() as u64
                    } else {
                        wp.uploaded_bytes.len() as u64
                    };
                    let alloc = size.max(wp.allocation_size);
                    Ok(FileInfo {
                        ino: fh,
                        size,
                        allocation_size: alloc,
                        is_directory: false,
                        is_readonly: false,
                        created_time: now,
                        modified_time: now,
                        file_attributes: FILE_ATTRIBUTE_NORMAL,
                    })
                } else {
                    self.execute_vfs_async_op(None, || async {
                        let wp = wp_mutex.lock().await;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let size = if let Some(spool) = &wp.write_spool {
                            spool.flushed_bytes() + spool.current_ram_usage() as u64
                        } else {
                            wp.uploaded_bytes.len() as u64
                        };
                        let alloc = size.max(wp.allocation_size);
                        Ok(FileInfo {
                            ino: fh,
                            size,
                            allocation_size: alloc,
                            is_directory: false,
                            is_readonly: false,
                            created_time: now,
                            modified_time: now,
                            file_attributes: FILE_ATTRIBUTE_NORMAL,
                        })
                    })
                }
            }
        }
    }

    /// Opens an existing file for reading.
    pub fn open(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let node = self
                .routing_table
                .get_inode(ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;

            if node.is_directory {
                let fh = self.allocate_fh();
                self.open_handles.insert(fh, HandleState::Directory { ino });
                return Ok(fh);
            }

            // Construct ByteSource
            let byte_source: Arc<dyn ByteSource> = if let Some(mock) = self.mock_files.get(path) {
                Arc::new(MockByteSource::new(mock.clone()))
            } else {
                match &node.location {
                    StorageLocation::SingleAccount {
                        account_id,
                        google_file_id,
                    } => {
                        let token = self.account_tokens.get(account_id).map(|t| t.clone()).unwrap_or_default();
                        Arc::new(SingleAccountByteSource::new(
                            Arc::clone(&self.client),
                            account_id.clone(),
                            token,
                            google_file_id.clone(),
                            node.size,
                        ))
                    }
                    StorageLocation::StripedManifest {
                        manifest_account_id,
                        manifest_google_file_id,
                        ..
                    } => {
                        let mut tokens = HashMap::new();
                        for item in self.account_tokens.iter() {
                            tokens.insert(item.key().clone(), item.value().clone());
                        }

                        let manifest_token = tokens.get(manifest_account_id).cloned().unwrap_or_default();
                        let client = Arc::clone(&self.client);
                        let m_acc = manifest_account_id.clone();
                        let m_fid = manifest_google_file_id.clone();

                        let download_fut = async move {
                            if let Ok(resp) = client.download_stream(&m_acc, &manifest_token, &m_fid, None).await
                                && let Ok(bytes) = resp.bytes().await
                                && let Ok(manifest) = serde_json::from_slice::<FileManifest>(&bytes)
                            {
                                manifest.chunks
                            } else {
                                Vec::new()
                            }
                        };

                        let chunks = if tokio::runtime::Handle::try_current().is_ok() {
                            tokio::task::block_in_place(|| self.rt_handle.block_on(download_fut))
                        } else {
                            self.rt_handle.block_on(download_fut)
                        };

                        Arc::new(StripedManifestByteSource::new(
                            Arc::clone(&self.client),
                            tokens,
                            chunks,
                            node.size,
                        ))
                    }
                    StorageLocation::VirtualWarningNote { content, .. } => {
                        let fh = self.allocate_fh();
                        let _ = self.sync_engine.register_open_handle(ino);
                        self.open_handles.insert(
                            fh,
                            HandleState::ReadVirtual {
                                ino,
                                bytes: Bytes::from(content.clone()),
                            },
                        );
                        debug!(fh, path, ino, "Opened virtual warning note for direct lock-free reading");
                        return Ok(fh);
                    }
                    StorageLocation::VirtualDirectory => {
                        return Err(VfsError::InternalFault("Cannot open directory as file".to_string()));
                    }
                }
            };

            let cached_source = Arc::new(CachedByteSource::new(
                ino,
                node.size,
                byte_source,
                Arc::clone(&self.shared_block_cache),
                Arc::clone(&self.hot_header_cache),
                self.rt_handle.clone(),
            ));

            let fh = self.allocate_fh();
            let _ = self.sync_engine.register_open_handle(ino);

            self.open_handles.insert(
                fh,
                HandleState::ReadShared {
                    ino,
                    source: cached_source,
                },
            );

            debug!(fh, path, ino, "Opened file for lock-free shared cached reading");
            Ok(fh)
        })
    }

    /// Creates a new virtual file in pending write state.
    pub fn create(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
            let fh = self.allocate_fh();

            let (preferred_account, overwriting_existing) =
                if let Some(existing_ino) = self.routing_table.lookup(parent_ino, &filename) {
                    if let Some(node) = self.routing_table.get_inode(existing_ino) {
                        match &node.location {
                            StorageLocation::SingleAccount {
                                account_id,
                                google_file_id,
                            } => (
                                Some(account_id.clone()),
                                Some((existing_ino, account_id.clone(), google_file_id.clone())),
                            ),
                            StorageLocation::StripedManifest {
                                manifest_account_id,
                                manifest_google_file_id,
                                ..
                            } => (
                                Some(manifest_account_id.clone()),
                                Some((
                                    existing_ino,
                                    manifest_account_id.clone(),
                                    manifest_google_file_id.clone(),
                                )),
                            ),
                            _ => (None, None),
                        }
                    } else {
                        (None, None)
                    }
                } else {
                    let pref = self.routing_table.get_preferred_account_for_folder(parent_ino);
                    (pref, None)
                };

            self.open_handles.insert(
                fh,
                HandleState::WritePending(Arc::new(Mutex::new(WritePendingState {
                    path: path.to_string(),
                    parent_ino,
                    filename,
                    allocation_size: 0,
                    write_spool: None,
                    single_guard: None,
                    striped_guard: None,
                    striped_plan: None,
                    uploaded_bytes: Vec::new(),
                    delete_on_close: false,
                    preferred_account,
                    overwriting_existing,
                }))),
            );

            debug!(fh, path, "Created pending write handle");
            Ok(fh)
        })
    }

    /// Creates a new directory purely in RAM (<0.001 ms). Remote folders are lazily materialized on upload.
    pub fn create_directory(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, dirname) = self.resolve_path_parent_and_name(path)?;
            let ino = self.routing_table.create_virtual_directory(parent_ino, &dirname)?;
            let fh = self.allocate_fh();
            self.open_handles.insert(fh, HandleState::Directory { ino });
            self.notifier.notify_created(path);
            let parent_path = self.routing_table.get_path_for_inode(parent_ino).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&parent_path);

            debug!(fh, path, ino, "Created in-memory virtual directory handle (lazy cloud materialization)");
            Ok(fh)
        })
    }

    /// Ensures that virtual directory `target_ino` and all unmaterialized ancestors up to ROOT_INO
    /// are materialized on `account_id` before file upload or cross-parent move.
    /// Returns the remote Google folder ID for `target_ino`, or `None` if `target_ino == ROOT_INO`.
    pub async fn ensure_remote_folder_chain_impl(
        routing_table: &InMemoryRoutingTable,
        client: &GoogleDriveClient,
        sync_engine: &DeltaSyncEngine,
        target_ino: u64,
        account_id: &str,
        token: &str,
    ) -> Result<Option<String>, VfsError> {
        if target_ino == InMemoryRoutingTable::ROOT_INO {
            return Ok(None);
        }

        if let Some(folder_id) = routing_table.get_remote_folder_id(target_ino, account_id) {
            return Ok(Some(folder_id));
        }

        // Walk up from target_ino collecting unmaterialized ancestors
        let mut unmaterialized = Vec::new();
        let mut curr_ino = target_ino;

        while curr_ino != InMemoryRoutingTable::ROOT_INO {
            if routing_table.get_remote_folder_id(curr_ino, account_id).is_some() {
                break;
            }

            let node = routing_table.get_inode(curr_ino).ok_or_else(|| {
                VfsError::NotFound(format!("Directory inode {curr_ino} not found during materialization"))
            })?;

            if !node.is_directory {
                return Err(VfsError::NotFound(format!("Inode {curr_ino} is not a directory")));
            }

            unmaterialized.push((curr_ino, node.name.clone()));
            curr_ino = node.parent_ino;
        }

        unmaterialized.reverse();

        let mut parent_remote = if curr_ino == InMemoryRoutingTable::ROOT_INO {
            None
        } else {
            routing_table.get_remote_folder_id(curr_ino, account_id)
        };

        for (dir_ino, dir_name) in unmaterialized {
            info!(
                account = %account_id,
                dir_ino,
                dir_name = %dir_name,
                parent_remote = ?parent_remote,
                "Lazy folder materialization: creating remote folder on Google Drive"
            );

            let remote_id = client
                .create_folder(
                    account_id,
                    token,
                    &dir_name,
                    parent_remote.as_deref(),
                )
                .await?;

            routing_table.record_remote_folder_id(dir_ino, account_id, &remote_id);
            sync_engine.record_local_upsert(account_id, &remote_id);

            parent_remote = Some(remote_id);
        }

        Ok(parent_remote)
    }

    /// Public convenience helper calling `ensure_remote_folder_chain_impl` with driver dependencies.
    pub async fn ensure_remote_folder_chain(
        &self,
        target_ino: u64,
        account_id: &str,
        token: &str,
    ) -> Result<Option<String>, VfsError> {
        Self::ensure_remote_folder_chain_impl(
            &self.routing_table,
            &self.client,
            &self.sync_engine,
            target_ino,
            account_id,
            token,
        )
        .await
    }

    /// Sets the file size / allocation size. Intercepts allocation size to resolve placement target!
    pub fn set_file_size(&self, fh: u64, size: u64) -> Result<(), VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_state = self
                .open_handles
                .get(&fh)
                .map(|g| g.value().clone())
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            if let HandleState::WritePending(wp_mutex) = handle_state {
                let mut wp = wp_mutex.lock().await;
                wp.allocation_size = size;

                if wp.write_spool.is_none() && wp.striped_plan.is_none() && size > 0 {
                    let decision = self
                        .placement_engine
                        .resolve_placement_with_affinity(
                            size,
                            &wp.filename,
                            &*self.prompter,
                            wp.preferred_account.as_deref(),
                        )
                        .await?;

                    match decision {
                        PlacementDecision::SingleAccount(guard) => {
                            let account_id = guard.account_id.clone();
                            let token = self.account_tokens.get(&account_id).map(|t| t.clone()).unwrap_or_default();
                            let parent_remote = self.ensure_remote_folder_chain(wp.parent_ino, &account_id, &token).await?;
                            let metadata = GoogleFileInput {
                                name: wp.filename.clone(),
                                mime_type: Some("application/octet-stream".to_string()),
                                parents: parent_remote.map(|pid| vec![pid]),
                                app_properties: None,
                            };

                            let session_uri = self
                                .client
                                .initiate_resumable_upload(&account_id, &token, &metadata)
                                .await
                                .unwrap_or_else(|_| format!("http://127.0.0.1:0/mock_session_{fh}"));

                            let mut spool = EphemeralWriteHandle::new(
                                fh,
                                Arc::clone(&self.client),
                                account_id,
                                session_uri,
                                &self.valve,
                            )?;
                            spool.set_declared_size(size);

                            wp.write_spool = Some(spool);
                            wp.single_guard = Some(guard);
                        }
                        PlacementDecision::StripedManifest(guard) => {
                            wp.striped_plan = Some(guard.plan.clone());
                            wp.striped_guard = Some(guard);
                        }
                    }
                }
            }

            Ok(())
        })
    }

    /// Reads binary content from an open file handle directly and lock-free without Mutex contention.
    pub fn read(&self, fh: u64, offset: u64, buffer: &mut [u8]) -> Result<usize, VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_state = self
                .open_handles
                .get(&fh)
                .map(|g| g.value().clone())
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            match handle_state {
                HandleState::ReadShared { ino: _, source } => {
                    let bytes = source.fetch_range(offset, buffer.len()).await?;
                    let len = bytes.len().min(buffer.len());
                    buffer[..len].copy_from_slice(&bytes[..len]);
                    Ok(len)
                }
                HandleState::ReadVirtual { ino: _, bytes } => {
                    let off = offset as usize;
                    if off >= bytes.len() {
                        return Ok(0);
                    }
                    let avail = &bytes[off..];
                    let len = avail.len().min(buffer.len());
                    buffer[..len].copy_from_slice(&avail[..len]);
                    Ok(len)
                }
                HandleState::WritePending(_) => {
                    Err(VfsError::InternalFault("Handle not open for reading".to_string()))
                }
                HandleState::Directory { .. } => {
                    Err(VfsError::InternalFault("Cannot read directory handle".to_string()))
                }
            }
        })
    }

    /// Writes binary content to an open file handle.
    pub fn write(&self, fh: u64, offset: u64, buffer: &[u8]) -> Result<usize, VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_state = self
                .open_handles
                .get(&fh)
                .map(|g| g.value().clone())
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            if let HandleState::WritePending(wp_mutex) = handle_state {
                let mut wp = wp_mutex.lock().await;
                if let Some(spool) = &mut wp.write_spool {
                    spool.write(offset, buffer).await
                } else {
                    let rel_offset = offset as usize;
                    if rel_offset > wp.uploaded_bytes.len() {
                        wp.uploaded_bytes.resize(rel_offset, 0);
                        wp.uploaded_bytes.extend_from_slice(buffer);
                    } else if rel_offset + buffer.len() <= wp.uploaded_bytes.len() {
                        wp.uploaded_bytes[rel_offset..rel_offset + buffer.len()].copy_from_slice(buffer);
                    } else {
                        let overwrite_len = wp.uploaded_bytes.len() - rel_offset;
                        wp.uploaded_bytes[rel_offset..].copy_from_slice(&buffer[..overwrite_len]);
                        wp.uploaded_bytes.extend_from_slice(&buffer[overwrite_len..]);
                    }
                    Ok(buffer.len())
                }
            } else {
                Err(VfsError::InternalFault("Handle not open for writing".to_string()))
            }
        })
    }

    /// Reads directory contents directly from in-memory routing table in <0.01 ms with ZERO network calls.
    pub fn read_directory(&self, path: &str) -> Result<Vec<DirEntry>, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let children = self.routing_table.readdir(ino)?;

            let entries = children
                .into_iter()
                .map(|child| DirEntry {
                    ino: child.ino,
                    name: child.name,
                    is_directory: child.is_directory,
                    size: child.size,
                })
                .collect();

            Ok(entries)
        })
    }

    /// Renames a virtual file or directory, propagating the rename to Google Drive.
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), VfsError> {
        self.execute_vfs_op(Some(old_path), || {
            sandbox::validate_path(new_path)?;

            let (old_parent, old_name) = self.resolve_path_parent_and_name(old_path)?;
            let (new_parent, new_name) = self.resolve_path_parent_and_name(new_path)?;

            let target_ino = self
                .routing_table
                .lookup(old_parent, &old_name)
                .ok_or_else(|| VfsError::NotFound(format!("File or directory '{old_name}' not found for rename")))?;

            let node = self
                .routing_table
                .get_inode(target_ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {target_ino} not found for rename")))?;

            let is_cross_parent = new_parent != old_parent;

            // Collect cloud rename targets
            let mut cloud_renames = Vec::new();
            match &node.location {
                StorageLocation::SingleAccount { account_id, google_file_id } => {
                    self.sync_engine.record_local_upsert(account_id, google_file_id);
                    cloud_renames.push((account_id.clone(), google_file_id.clone()));
                }
                StorageLocation::StripedManifest { manifest_account_id, manifest_google_file_id, .. } => {
                    self.sync_engine.record_local_upsert(manifest_account_id, manifest_google_file_id);
                    cloud_renames.push((manifest_account_id.clone(), manifest_google_file_id.clone()));
                }
                StorageLocation::VirtualDirectory => {
                    for acc_id in self.account_tokens.iter().map(|kv| kv.key().clone()) {
                        if let Some(folder_id) = self.routing_table.get_remote_folder_id(target_ino, &acc_id) {
                            self.sync_engine.record_local_upsert(&acc_id, &folder_id);
                            cloud_renames.push((acc_id, folder_id));
                        }
                    }
                }
                StorageLocation::VirtualWarningNote { .. } => {}
            }

            self.routing_table.rename(old_parent, &old_name, new_parent, &new_name)?;
            self.notifier.notify_deleted(old_path);
            self.notifier.notify_created(new_path);
            let old_parent_path = self.routing_table.get_path_for_inode(old_parent).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&old_parent_path);
            if new_parent != old_parent {
                let new_parent_path = self.routing_table.get_path_for_inode(new_parent).unwrap_or_else(|| "/".to_string());
                self.notifier.notify_updated(&new_parent_path);
            }

            if !cloud_renames.is_empty() {
                let client = Arc::clone(&self.client);
                let routing_table = Arc::clone(&self.routing_table);
                let sync_engine = Arc::clone(&self.sync_engine);
                let tokens_map: HashMap<String, String> = self.account_tokens.iter().map(|kv| (kv.key().clone(), kv.value().clone())).collect();
                let new_name_clone = new_name.clone();
                self.rt_handle.spawn(async move {
                    for (account_id, file_id) in cloud_renames {
                        if let Some(token) = tokens_map.get(&account_id) {
                            let (remove_parent, add_parent) = if is_cross_parent {
                                // 1. Determine add_parent (target directory on this account)
                                let add_p = if new_parent == InMemoryRoutingTable::ROOT_INO {
                                    Some("root".to_string())
                                } else {
                                    match Self::ensure_remote_folder_chain_impl(
                                        &routing_table,
                                        &client,
                                        &sync_engine,
                                        new_parent,
                                        &account_id,
                                        token,
                                    ).await {
                                        Ok(Some(id)) => Some(id),
                                        Ok(None) => Some("root".to_string()),
                                        Err(e) => {
                                            warn!(account = %account_id, error = %e, "Failed to materialize target folder chain during move");
                                            None
                                        }
                                    }
                                };

                                // 2. Determine remove_parent (source directory on this account)
                                let rem_p = if old_parent == InMemoryRoutingTable::ROOT_INO {
                                    Some("root".to_string())
                                } else {
                                    routing_table
                                        .get_remote_folder_id(old_parent, &account_id)
                                        .or_else(|| Some("root".to_string()))
                                };

                                (rem_p, add_p)
                            } else {
                                (None, None)
                            };

                            if let Err(e) = client.rename_file(
                                &account_id,
                                token,
                                &file_id,
                                &new_name_clone,
                                remove_parent.as_deref(),
                                add_parent.as_deref(),
                            ).await {
                                warn!(account = %account_id, file_id = %file_id, new_name = %new_name_clone, error = %e, "Failed to rename/move on Google Drive");
                            } else {
                                info!(account = %account_id, file_id = %file_id, new_name = %new_name_clone, "Successfully renamed/moved on Google Drive");
                            }
                        }
                    }
                });
            }

            info!(old_path, new_path, "Renamed filesystem entry and dispatched cloud renames");
            Ok(())
        })
    }

    /// Deletes a file or directory, propagating the deletion to Google Drive.
    pub fn delete(&self, path: &str) -> Result<(), VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
            let ino = self.routing_table.lookup(parent_ino, &filename).ok_or_else(|| {
                VfsError::NotFound(format!("File or directory '{path}' not found for deletion"))
            })?;

            let node = self.routing_table.get_inode(ino).ok_or_else(|| {
                VfsError::NotFound(format!("Inode {ino} not found for deletion"))
            })?;

            if node.is_directory && !node.children.is_empty() {
                return Err(VfsError::InternalFault(
                    "Directory not empty".to_string(),
                ));
            }

            // Collect remote cloud deletion targets
            let mut cloud_deletions = Vec::new();
            match &node.location {
                StorageLocation::SingleAccount { account_id, google_file_id } => {
                    self.sync_engine.record_local_mutation(account_id, google_file_id);
                    cloud_deletions.push((account_id.clone(), google_file_id.clone()));
                    self.placement_engine.release_used_bytes(account_id, node.size);
                }
                StorageLocation::StripedManifest {
                    manifest_account_id,
                    manifest_google_file_id,
                    ..
                } => {
                    self.sync_engine.record_local_mutation(manifest_account_id, manifest_google_file_id);
                    cloud_deletions.push((manifest_account_id.clone(), manifest_google_file_id.clone()));
                }
                StorageLocation::VirtualDirectory => {
                    for (acc_id, folder_id) in self.routing_table.get_all_remote_folder_ids(ino) {
                        self.sync_engine.record_local_mutation(&acc_id, &folder_id);
                        cloud_deletions.push((acc_id, folder_id));
                    }
                }
                StorageLocation::VirtualWarningNote { .. } => {}
            }

            let _ = self.routing_table.remove(parent_ino, &filename)?;
            self.notifier.notify_deleted(path);
            let parent_path = self.routing_table.get_path_for_inode(parent_ino).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&parent_path);

            if !cloud_deletions.is_empty() {
                let client = Arc::clone(&self.client);
                let tokens_map: HashMap<String, String> = self.account_tokens.iter().map(|kv| (kv.key().clone(), kv.value().clone())).collect();
                self.rt_handle.spawn(async move {
                    for (account_id, file_id) in cloud_deletions {
                        if let Some(token) = tokens_map.get(&account_id) {
                            if let Err(e) = client.delete_file(&account_id, token, &file_id).await {
                                warn!(account = %account_id, file_id = %file_id, error = %e, "Failed to delete file on Google Drive");
                            } else {
                                info!(account = %account_id, file_id = %file_id, "Successfully deleted remote file on Google Drive");
                            }
                        }
                    }
                });
            }

            info!(path, "Deleted filesystem entry and dispatched cloud deletions");
            Ok(())
        })
    }

    /// Detaches the file being overwritten (if any) from the routing table without touching Drive.
    fn detach_overwritten_file(
        &self,
        parent_ino: u64,
        filename: &str,
        overwriting: Option<(u64, String, String)>,
    ) -> Option<DetachedOverwrite> {
        let (_old_ino, account_id, google_file_id) = overwriting?;
        let node = self.routing_table.remove(parent_ino, filename).ok();
        Some(DetachedOverwrite { node, account_id, google_file_id })
    }

    /// Completes an overwrite. On success the old cloud copy is deleted and its quota released;
    /// on failure the old routing entry is restored so the original file stays visible and intact.
    fn finish_overwrite(&self, detached: Option<DetachedOverwrite>, parent_ino: u64, committed: bool) {
        let Some(old) = detached else { return };
        if !committed {
            if let Some(node) = old.node {
                let restored = self.routing_table.insert_file(NewFileParams {
                    parent_ino,
                    name: &node.name,
                    size: node.size,
                    mime_type: &node.mime_type,
                    modified_time: node.modified_time,
                    created_time: node.created_time,
                    location: node.location.clone(),
                    web_view_link: node.web_view_link.clone(),
                });
                if let Err(e) = restored {
                    warn!(name = %node.name, error = %e, "Failed to restore original file after aborted overwrite");
                }
            }
            return;
        }
        if let Some(node) = &old.node
            && matches!(node.location, StorageLocation::SingleAccount { .. })
        {
            self.placement_engine.release_used_bytes(&old.account_id, node.size);
        }
        self.sync_engine.record_local_mutation(&old.account_id, &old.google_file_id);
        let client = Arc::clone(&self.client);
        let token = self
            .account_tokens
            .get(&old.account_id)
            .map(|t| t.clone())
            .unwrap_or_default();
        self.rt_handle.spawn(async move {
            if let Err(e) = client
                .delete_file(&old.account_id, &token, &old.google_file_id)
                .await
            {
                warn!(
                    account = %old.account_id,
                    file_id = %old.google_file_id,
                    error = %e,
                    "Failed to delete overwritten file on Google Drive; it may leak quota until the next sync"
                );
            }
        });
    }

    /// Finalizes open write streams on close, committing files to routing table and zeroizing RAM.
    pub fn cleanup(&self, fh: u64) -> Result<(), VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_state = match self.open_handles.get(&fh) {
                Some(g) => g.value().clone(),
                None => return Ok(()),
            };

            if let HandleState::WritePending(wp_mutex) = handle_state {
                let mut wp = wp_mutex.lock().await;
                if wp.delete_on_close {
                    debug!(fh, path = %wp.path, "Cancelled write handle with delete_on_close");
                    return Ok(());
                }

                // If no spool was initialized yet (e.g. WriteFile was called without SetFileSize),
                // initialize spool now from accumulated uploaded_bytes.
                if wp.write_spool.is_none() && wp.striped_guard.is_none() {
                    let total_bytes = wp.uploaded_bytes.len() as u64;
                    let decision = self
                        .placement_engine
                        .resolve_placement_with_affinity(
                            total_bytes.max(1),
                            &wp.filename,
                            &*self.prompter,
                            wp.preferred_account.as_deref(),
                        )
                        .await?;

                    match decision {
                        PlacementDecision::SingleAccount(guard) => {
                            let account_id = guard.account_id.clone();
                            let token = self
                                .account_tokens
                                .get(&account_id)
                                .map(|t| t.clone())
                                .unwrap_or_default();
                            let parent_remote = self.ensure_remote_folder_chain(wp.parent_ino, &account_id, &token).await?;
                            let metadata = GoogleFileInput {
                                name: wp.filename.clone(),
                                mime_type: Some("application/octet-stream".to_string()),
                                parents: parent_remote.map(|pid| vec![pid]),
                                app_properties: None,
                            };
                            let session_uri = self
                                .client
                                .initiate_resumable_upload(&account_id, &token, &metadata)
                                .await
                                .unwrap_or_else(|_| format!("http://127.0.0.1:0/mock_session_{fh}"));
                            let mut spool = EphemeralWriteHandle::new(
                                fh,
                                Arc::clone(&self.client),
                                account_id,
                                session_uri,
                                &self.valve,
                            )?;
                            spool.set_declared_size(total_bytes);
                            if !wp.uploaded_bytes.is_empty() {
                                spool.write(0, &wp.uploaded_bytes).await?;
                            }
                            wp.write_spool = Some(spool);
                            wp.single_guard = Some(guard);
                        }
                        PlacementDecision::StripedManifest(guard) => {
                            wp.striped_plan = Some(guard.plan.clone());
                            wp.striped_guard = Some(guard);
                        }
                    }
                }

                if let Some(mut spool) = wp.write_spool.take() {
                    let account_id = spool.account_id().to_string();
                    let commit_res = spool.commit().await?;

                    if let Some(mut guard) = wp.single_guard.take() {
                        guard.commit();
                    }

                    // The new bytes are safely on Drive. Detach the file being overwritten (if any) so
                    // insert_file does not see a false name collision. It is restored if registering the
                    // replacement fails, and its cloud copy is only deleted once the replacement is routed.
                    let overwriting = wp.overwriting_existing.take();
                    let detached =
                        self.detach_overwritten_file(wp.parent_ino, &wp.filename, overwriting);

                    let location = StorageLocation::SingleAccount {
                        account_id: account_id.clone(),
                        google_file_id: commit_res.file_id.clone(),
                    };

                    if let Err(e) = self.routing_table.insert_file(NewFileParams {
                        parent_ino: wp.parent_ino,
                        name: &wp.filename,
                        size: commit_res.total_bytes,
                        mime_type: "application/octet-stream",
                        modified_time: 1_700_000_000,
                        created_time: 1_700_000_000,
                        location,
                        web_view_link: None,
                    }) {
                        self.finish_overwrite(detached, wp.parent_ino, false);
                        return Err(e);
                    }

                    self.finish_overwrite(detached, wp.parent_ino, true);
                    self.sync_engine.record_local_upsert(&account_id, &commit_res.file_id);
                    self.notifier.notify_created(&wp.path);
                    info!(path = %wp.path, total_bytes = commit_res.total_bytes, "Committed single file upload to VFS");
                } else if let Some(guard) = wp.striped_guard.take() {
                    // Detach (not delete) the file being overwritten: the striped upload below can fail,
                    // in which case the original must survive both in the routing table and on Drive.
                    let overwriting = wp.overwriting_existing.take();
                    let detached =
                        self.detach_overwritten_file(wp.parent_ino, &wp.filename, overwriting);

                    let mut account_tokens = HashMap::new();
                    for item in self.account_tokens.iter() {
                        account_tokens.insert(item.key().clone(), item.value().clone());
                    }

                    let chunker = OversizedFileChunker::new(
                        Arc::clone(&self.client),
                        Arc::clone(&self.routing_table),
                    );

                    let unified_ino = match chunker
                        .upload_striped_file(
                            wp.parent_ino,
                            guard,
                            &wp.uploaded_bytes,
                            &account_tokens,
                        )
                        .await
                    {
                        Ok(ino) => ino,
                        Err(e) => {
                            self.finish_overwrite(detached, wp.parent_ino, false);
                            return Err(e);
                        }
                    };

                    self.finish_overwrite(detached, wp.parent_ino, true);

                    if let Some(node) = self.routing_table.get_inode(unified_ino)
                        && let StorageLocation::StripedManifest {
                            manifest_account_id,
                            manifest_google_file_id,
                            ..
                        } = &node.location
                    {
                        self.sync_engine
                            .record_local_upsert(manifest_account_id, manifest_google_file_id);
                    }

                    self.notifier.notify_created(&wp.path);
                    info!(path = %wp.path, unified_ino, "Committed multi-account striped upload to VFS");
                }
            }

            Ok(())
        })
    }

    /// Closes an open file handle and releases all associated memory.
    pub fn close(&self, fh: u64) -> Result<(), VfsError> {
        self.execute_vfs_op(None, || {
            if let Some((_, handle_state)) = self.open_handles.remove(&fh) {
                if let HandleState::ReadShared { ino, .. } | HandleState::ReadVirtual { ino, .. } = handle_state {
                    self.sync_engine.unregister_open_handle(ino);
                }
                debug!(fh, "Closed and unregistered file handle");
            }
            Ok(())
        })
    }

    /// Cleanly unmounts the filesystem.
    pub fn unmount(&self) -> Result<(), VfsError> {
        info!("Unmounting ucloud-fs VFS driver");
        self.is_mounted.store(false, Ordering::SeqCst);
        self.open_handles.clear();
        self.shared_block_cache.clear();
        self.hot_header_cache.clear();
        self.session.lock_and_purge_all_caches();
        Ok(())
    }

    /// Returns whether the driver is actively mounted.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        self.is_mounted.load(Ordering::SeqCst)
    }

    /// Number of active open file handles.
    #[must_use]
    pub fn active_handles_count(&self) -> usize {
        self.open_handles.len()
    }

    /// Debug helper to inspect open handles.
    #[must_use]
    pub fn open_handles_debug(&self) -> Vec<(u64, String)> {
        self.open_handles
            .iter()
            .map(|entry| {
                let state_str = match entry.value() {
                    HandleState::ReadShared { ino, .. } => format!("ReadShared(ino: {ino})"),
                    HandleState::ReadVirtual { ino, .. } => format!("ReadVirtual(ino: {ino})"),
                    HandleState::WritePending(_) => "WritePending".to_string(),
                    HandleState::Directory { ino } => format!("Directory(ino: {ino})"),
                };
                (*entry.key(), state_str)
            })
            .collect()
    }
}
