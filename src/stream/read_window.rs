use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::broadcast;
use tracing::debug;

use crate::error::VfsError;
use crate::network::client::GoogleDriveClient;
use crate::routing::chunker::UploadedChunk;
use crate::stream::backpressure::{MemoryBackpressureValve, MemoryReservation};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trait abstracting binary data sources for ephemeral sliding window reads.
pub trait ByteSource: Send + Sync {
    fn file_size(&self) -> u64;
    fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>>;
}

/// Byte source reading from a single Google Drive file via HTTP/2 Range GETs.
pub struct SingleAccountByteSource {
    client: Arc<GoogleDriveClient>,
    account_id: String,
    access_token: String,
    file_id: String,
    size: u64,
}

impl SingleAccountByteSource {
    #[must_use]
    pub fn new(
        client: Arc<GoogleDriveClient>,
        account_id: String,
        access_token: String,
        file_id: String,
        size: u64,
    ) -> Self {
        Self {
            client,
            account_id,
            access_token,
            file_id,
            size,
        }
    }
}

impl ByteSource for SingleAccountByteSource {
    fn file_size(&self) -> u64 {
        self.size
    }

    fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
        Box::pin(async move {
            let resp = self
                .client
                .download_stream(
                    &self.account_id,
                    &self.access_token,
                    &self.file_id,
                    Some((start, end)),
                )
                .await?;

            resp.bytes()
                .await
                .map_err(|e| VfsError::InternalFault(format!("Failed to stream bytes from Google Drive: {e}")))
        })
    }
}

/// Byte source reading across multi-account striped chunks.
pub struct StripedManifestByteSource {
    client: Arc<GoogleDriveClient>,
    account_tokens: HashMap<String, String>,
    chunks: Vec<UploadedChunk>,
    size: u64,
}

impl StripedManifestByteSource {
    #[must_use]
    pub fn new(
        client: Arc<GoogleDriveClient>,
        account_tokens: HashMap<String, String>,
        chunks: Vec<UploadedChunk>,
        size: u64,
    ) -> Self {
        Self {
            client,
            account_tokens,
            chunks,
            size,
        }
    }
}

impl ByteSource for StripedManifestByteSource {
    fn file_size(&self) -> u64 {
        self.size
    }

    fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
        Box::pin(async move {
            if start > end || start >= self.size {
                return Ok(Bytes::new());
            }

            let mut assembled = Vec::with_capacity((end - start + 1) as usize);

            for chunk in &self.chunks {
                let chunk_start = chunk.byte_offset;
                let chunk_end = chunk_start + chunk.byte_len - 1;

                if start <= chunk_end && end >= chunk_start {
                    let overlap_start = start.max(chunk_start);
                    let overlap_end = end.min(chunk_end);

                    let rel_start = overlap_start - chunk_start;
                    let rel_end = overlap_end - chunk_start;

                    let token = self.account_tokens.get(&chunk.account_id).ok_or_else(|| {
                        VfsError::PermissionDenied(format!("Missing token for account '{}'", chunk.account_id))
                    })?;

                    let resp = self
                        .client
                        .download_stream(
                            &chunk.account_id,
                            token,
                            &chunk.google_file_id,
                            Some((rel_start, rel_end)),
                        )
                        .await?;

                    let chunk_bytes = resp.bytes().await.map_err(|e| {
                        VfsError::InternalFault(format!("Failed to stream striped chunk: {e}"))
                    })?;

                    assembled.extend_from_slice(&chunk_bytes);
                }
            }

            Ok(Bytes::from(assembled))
        })
    }
}

/// In-memory mock byte source for testing seek patterns and range coalescing.
pub struct MockByteSource {
    data: Bytes,
}

impl MockByteSource {
    #[must_use]
    pub fn new(data: Bytes) -> Self {
        Self { data }
    }
}

impl ByteSource for MockByteSource {
    fn file_size(&self) -> u64 {
        self.data.len() as u64
    }

    fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
        Box::pin(async move {
            if start >= self.file_size() {
                return Ok(Bytes::new());
            }
            let end = (end + 1).min(self.file_size()) as usize;
            let start = start as usize;
            Ok(self.data.slice(start..end))
        })
    }
}

/// Shared Inode Block Cache dividing files into 2 MB blocks with LRU eviction and single-flight deduplication.
pub struct SharedInodeBlockCache {
    blocks: DashMap<(u64, u64), (Instant, Bytes)>,
    in_flight: DashMap<(u64, u64), broadcast::Sender<Bytes>>,
    max_blocks: usize,
}

impl Default for SharedInodeBlockCache {
    fn default() -> Self {
        Self::new(48) // 48 blocks * 2 MB = 96 MB LRU ceiling
    }
}

impl SharedInodeBlockCache {
    pub const BLOCK_SIZE: usize = 2 * 1024 * 1024; // 2 MB

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
        Fut: Future<Output = Result<Bytes, VfsError>>,
    {
        if let Some(bytes) = self.get(ino, block_index) {
            return Ok(bytes);
        }

        enum FlightStatus {
            Leader(broadcast::Sender<Bytes>),
            Follower(broadcast::Receiver<Bytes>),
        }

        let flight_status = {
            use dashmap::mapref::entry::Entry;
            match self.in_flight.entry((ino, block_index)) {
                Entry::Occupied(occ) => FlightStatus::Follower(occ.get().subscribe()),
                Entry::Vacant(vac) => {
                    let (tx, _) = broadcast::channel(1);
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

/// Cached byte source wrapping an inner `ByteSource` with 2 MB block alignment,
/// shared inode caching, and 3-block concurrent background read-ahead.
pub struct CachedByteSource {
    ino: u64,
    inner: Arc<dyn ByteSource>,
    cache: Arc<SharedInodeBlockCache>,
    rt_handle: Option<tokio::runtime::Handle>,
}

impl CachedByteSource {
    #[must_use]
    pub fn new(
        ino: u64,
        inner: Arc<dyn ByteSource>,
        cache: Arc<SharedInodeBlockCache>,
        rt_handle: Option<tokio::runtime::Handle>,
    ) -> Self {
        Self {
            ino,
            inner,
            cache,
            rt_handle,
        }
    }

    /// Spawns background tasks to ensure subsequent blocks (K+1, K+2, K+3) are pre-populated.
    fn trigger_multi_block_read_ahead(&self, start_block: u64, count: u64) {
        let Some(rt) = &self.rt_handle else {
            return;
        };
        let block_size = SharedInodeBlockCache::BLOCK_SIZE as u64;
        let file_size = self.inner.file_size();

        for i in 0..count {
            let b = start_block + i;
            let b_start = b * block_size;
            if b_start >= file_size {
                break;
            }
            if self.cache.get(self.ino, b).is_some() {
                continue;
            }

            let b_end = ((b + 1) * block_size).min(file_size).saturating_sub(1);
            let inner = Arc::clone(&self.inner);
            let cache = Arc::clone(&self.cache);
            let ino = self.ino;

            let fut = async move {
                let _ = cache
                    .get_or_fetch(ino, b, move || async move {
                        inner.fetch_range(b_start, b_end).await
                    })
                    .await;
            };

            rt.spawn(fut);
        }
    }
}

impl ByteSource for CachedByteSource {
    fn file_size(&self) -> u64 {
        self.inner.file_size()
    }

    fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
        Box::pin(async move {
            let file_size = self.file_size();
            if start > end || start >= file_size {
                return Ok(Bytes::new());
            }
            let end = end.min(file_size.saturating_sub(1));

            let block_size = SharedInodeBlockCache::BLOCK_SIZE as u64;
            let start_block = start / block_size;
            let end_block = end / block_size;

            // Fast-start optimization: On first probe at offset < 512 KB,
            // fetch 512 KB directly for sub-100ms startup and asynchronously warm blocks 0, 1, 2!
            if start == 0 && (end + 1) <= 512 * 1024 && self.cache.get(self.ino, 0).is_none() {
                let fast_bytes = self.inner.fetch_range(start, end).await?;
                self.trigger_multi_block_read_ahead(0, 3);
                return Ok(fast_bytes);
            }

            let mut assembled = Vec::with_capacity((end - start + 1) as usize);

            for b in start_block..=end_block {
                let b_start = b * block_size;
                let b_end = ((b + 1) * block_size).min(file_size).saturating_sub(1);

                let inner = Arc::clone(&self.inner);
                let block_bytes = self
                    .cache
                    .get_or_fetch(self.ino, b, move || async move {
                        inner.fetch_range(b_start, b_end).await
                    })
                    .await?;

                let overlap_start = start.max(b_start);
                let overlap_end = end.min(b_end);

                let rel_start = (overlap_start - b_start) as usize;
                let rel_end = (overlap_end - b_start + 1) as usize;

                if rel_start < block_bytes.len() {
                    let slice_end = rel_end.min(block_bytes.len());
                    assembled.extend_from_slice(&block_bytes[rel_start..slice_end]);
                }
            }

            self.trigger_multi_block_read_ahead(end_block + 1, 3);

            Ok(Bytes::from(assembled))
        })
    }
}

/// Per-Handle Ephemeral Read Ring Buffer with Media Player Tail-Seek Optimization.
pub struct EphemeralReadHandle {
    fh: u64,
    source: Arc<dyn ByteSource>,
    file_size: u64,
    forward_start_offset: u64,
    forward_buffer: VecDeque<u8>,
    forward_capacity: usize,
    tail_start_offset: Option<u64>,
    tail_buffer: Vec<u8>,
    tail_capacity: usize,
    reservation: Option<MemoryReservation>,
    enable_prefetch: bool,
    prefetch_task: Option<tokio::task::JoinHandle<Result<Bytes, VfsError>>>,
    prefetch_range: Option<(u64, u64)>,
    sequential_hit_streak: u32,
    enable_progressive: bool,
    last_read_end: u64,
    rt_handle: Option<tokio::runtime::Handle>,
}

impl EphemeralReadHandle {
    /// Default forward sliding window capacity: 8 MB (smooth 1080p video streaming).
    pub const DEFAULT_FORWARD_CAPACITY: usize = 8 * 1024 * 1024;
    /// Default tail header slot capacity: 256 KB.
    pub const DEFAULT_TAIL_CAPACITY: usize = 256 * 1024;
    /// Tail threshold offset: within the final 512 KB of the file.
    pub const TAIL_THRESHOLD_OFFSET: u64 = 512 * 1024;
    /// Expanded tail threshold for files > 16 MB: 8 MB (covers MP4 moov & MKV Cues).
    pub const EXPANDED_TAIL_THRESHOLD_OFFSET: u64 = 8 * 1024 * 1024;
    /// 4 MB look-behind retention margin to support interleaved audio/video demuxer reads.
    pub const LOOK_BEHIND_MARGIN: u64 = 4 * 1024 * 1024;

    /// Creates an `EphemeralReadHandle` bound to an OS file handle `fh` with global backpressure checks.
    pub fn new(
        fh: u64,
        source: Arc<dyn ByteSource>,
        valve: &Arc<MemoryBackpressureValve>,
    ) -> Result<Self, VfsError> {
        Self::with_capacities(
            fh,
            source,
            valve,
            Self::DEFAULT_FORWARD_CAPACITY,
            Self::DEFAULT_TAIL_CAPACITY,
        )
    }

    /// Creates an `EphemeralReadHandle` with custom window capacities.
    pub fn with_capacities(
        fh: u64,
        source: Arc<dyn ByteSource>,
        valve: &Arc<MemoryBackpressureValve>,
        forward_capacity: usize,
        tail_capacity: usize,
    ) -> Result<Self, VfsError> {
        let file_size = source.file_size();
        let forward_capacity = forward_capacity.min(file_size as usize).max(64 * 1024);
        let tail_capacity = tail_capacity.min(file_size as usize);

        let total_needed = forward_capacity + tail_capacity;
        let min_needed = (64 * 1024).min(total_needed);

        let reservation = valve
            .acquire_or_shrink(total_needed, min_needed)
            .ok_or(VfsError::DiskFull)?;

        let actual_forward_cap = reservation.allocated_bytes().saturating_sub(tail_capacity).max(64 * 1024);

        Ok(Self {
            fh,
            source,
            file_size,
            forward_start_offset: 0,
            forward_buffer: VecDeque::new(),
            forward_capacity: actual_forward_cap,
            tail_start_offset: None,
            tail_buffer: Vec::new(),
            tail_capacity,
            reservation: Some(reservation),
            enable_prefetch: false,
            prefetch_task: None,
            prefetch_range: None,
            sequential_hit_streak: 0,
            enable_progressive: false,
            last_read_end: 0,
            rt_handle: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// Sets the Tokio runtime handle to use for background prefetching.
    pub fn set_runtime_handle(&mut self, rt: tokio::runtime::Handle) {
        self.rt_handle = Some(rt);
    }

    /// Safely spawns a prefetch task using the configured runtime handle, ambient runtime, or skips if none.
    fn spawn_prefetch_task(
        &self,
        source: Arc<dyn ByteSource>,
        start: u64,
        end: u64,
    ) -> Option<tokio::task::JoinHandle<Result<Bytes, VfsError>>> {
        let fut = async move { source.fetch_range(start, end).await };
        if let Some(rt) = &self.rt_handle {
            Some(rt.spawn(fut))
        } else if let Ok(rt) = tokio::runtime::Handle::try_current() {
            Some(rt.spawn(fut))
        } else {
            tracing::warn!("No Tokio runtime context available to spawn prefetch task");
            None
        }
    }

    /// Seeds an initial header slice into `forward_buffer` (e.g. from HotHeaderCache).
    pub fn seed_initial_header(&mut self, offset: u64, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        if self.forward_buffer.is_empty() {
            let bytes_len = bytes.len() as u64;
            self.forward_start_offset = offset;
            self.forward_buffer = VecDeque::from(bytes.to_vec());
            self.last_read_end = offset + bytes_len;
            self.sequential_hit_streak = self.sequential_hit_streak.max(1);

            // Proactively trigger prefetch for the next segment if prefetch is enabled
            if self.enable_prefetch && self.prefetch_task.is_none() {
                let forward_end = offset + bytes_len;
                if forward_end < self.file_size {
                    let (_, prefetch_size) = self.progressive_fetch_and_prefetch_sizes();
                    let prefetch_len = (prefetch_size as u64).min(self.file_size - forward_end);
                    if prefetch_len > 0 {
                        let prefetch_end = (forward_end + prefetch_len).saturating_sub(1);
                        let source = Arc::clone(&self.source);
                        self.prefetch_range = Some((forward_end, prefetch_end + 1));
                        self.prefetch_task = self.spawn_prefetch_task(source, forward_end, prefetch_end);
                    }
                }
            }
        }
    }

    /// Appends incoming data to `forward_buffer`, ensuring that front bytes below
    /// `min_retain_offset` are drained only as needed, and that `forward_start_offset`
    /// NEVER exceeds `min_retain_offset`.
    pub fn append_to_forward_buffer(&mut self, data: &[u8], min_retain_offset: u64) {
        if data.is_empty() {
            return;
        }
        let incoming_len = data.len();
        let max_cap = self.forward_capacity;

        if self.forward_buffer.len() + incoming_len > max_cap {
            let excess = (self.forward_buffer.len() + incoming_len) - max_cap;
            let max_drainable = min_retain_offset.saturating_sub(self.forward_start_offset) as usize;
            let drain_amount = excess.min(max_drainable).min(self.forward_buffer.len());
            if drain_amount > 0 {
                self.forward_buffer.drain(..drain_amount);
                self.forward_start_offset += drain_amount as u64;
            }
        }

        self.forward_buffer.extend(data.iter().copied());
    }

    /// Awaits and absorbs an in-flight background prefetch task if it covers `required_offset`.
    /// Returns true if an in-flight prefetch was absorbed.
    pub async fn try_absorb_prefetch_if_covers(&mut self, required_offset: u64, min_retain_offset: u64) -> bool {
        if let Some((start_offset, end_offset)) = self.prefetch_range
            && required_offset >= start_offset
            && required_offset < end_offset
            && let Some(task) = self.prefetch_task.take()
        {
            self.prefetch_range = None;
            if let Ok(Ok(bytes)) = task.await {
                let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
                if start_offset == forward_end {
                    self.append_to_forward_buffer(&bytes, min_retain_offset);
                } else if start_offset > forward_end {
                    self.forward_buffer.make_contiguous().fill(0);
                    self.forward_buffer.clear();
                    self.forward_start_offset = start_offset;
                    self.forward_buffer = VecDeque::from(bytes.to_vec());
                } else {
                    let overlap = (forward_end - start_offset) as usize;
                    if overlap < bytes.len() {
                        self.append_to_forward_buffer(&bytes[overlap..], min_retain_offset);
                    }
                }
                return true;
            }
        }
        false
    }

    /// Copies out a slice of up to `len` bytes starting at `offset` from `forward_buffer`.
    fn copy_out_slice(&self, offset: u64, len: usize) -> Bytes {
        let rel_offset = (offset - self.forward_start_offset) as usize;
        let copy_len = len.min(self.forward_buffer.len().saturating_sub(rel_offset));
        let (s1, s2) = self.forward_buffer.as_slices();
        if rel_offset + copy_len <= s1.len() {
            Bytes::copy_from_slice(&s1[rel_offset..rel_offset + copy_len])
        } else if rel_offset >= s1.len() {
            let s2_off = rel_offset - s1.len();
            Bytes::copy_from_slice(&s2[s2_off..s2_off + copy_len])
        } else {
            let mut out = Vec::with_capacity(copy_len);
            out.extend_from_slice(&s1[rel_offset..]);
            let remaining = copy_len - (s1.len() - rel_offset);
            out.extend_from_slice(&s2[..remaining]);
            Bytes::from(out)
        }
    }

    /// Calculates safe minimum retain offset so demuxer look-behind margin is preserved.
    fn calculate_min_retain_offset(&self, offset: u64) -> u64 {
        let look_behind = if self.enable_progressive {
            Self::LOOK_BEHIND_MARGIN.min((self.forward_capacity / 4) as u64)
        } else {
            0
        };
        offset.saturating_sub(look_behind)
    }

    /// Toggles proactive background prefetching for smooth video/audio playback.
    pub fn enable_prefetch(&mut self, enable: bool) {
        self.enable_prefetch = enable;
    }

    /// Toggles progressive exponential fetch sizing (Fast-Start -> High-Bandwidth).
    pub fn enable_progressive(&mut self, enable: bool) {
        self.enable_progressive = enable;
    }

    /// Returns whether progressive window sizing is active.
    #[must_use]
    pub fn is_progressive(&self) -> bool {
        self.enable_progressive
    }

    /// Returns the current sequential hit streak count.
    #[must_use]
    pub fn sequential_hit_streak(&self) -> u32 {
        self.sequential_hit_streak
    }

    /// Returns a copy of the front bytes if the forward buffer starts at 0.
    #[must_use]
    pub fn peek_front_buffer(&self, max_len: usize) -> Option<Bytes> {
        if self.forward_start_offset == 0 && !self.forward_buffer.is_empty() {
            let copy_len = max_len.min(self.forward_buffer.len());
            let (s1, s2) = self.forward_buffer.as_slices();
            if copy_len <= s1.len() {
                Some(Bytes::copy_from_slice(&s1[..copy_len]))
            } else {
                let mut out = Vec::with_capacity(copy_len);
                out.extend_from_slice(s1);
                out.extend_from_slice(&s2[..copy_len - s1.len()]);
                Some(Bytes::from(out))
            }
        } else {
            None
        }
    }

    /// Returns a copy of the tail buffer and its start offset if populated.
    #[must_use]
    pub fn peek_tail_buffer(&self) -> Option<(u64, Bytes)> {
        if let Some(start) = self.tail_start_offset && !self.tail_buffer.is_empty() {
            Some((start, Bytes::copy_from_slice(&self.tail_buffer)))
        } else {
            None
        }
    }

    /// Computes (fetch_size, prefetch_size) based on sequential streak when progressive mode is enabled.
    #[must_use]
    pub fn progressive_fetch_and_prefetch_sizes(&self) -> (usize, usize) {
        if !self.enable_progressive {
            return (self.forward_capacity, (self.forward_capacity / 2).max(1024 * 1024));
        }
        match self.sequential_hit_streak {
            0 => (512 * 1024, 2 * 1024 * 1024),
            1 => (2 * 1024 * 1024, 2 * 1024 * 1024),
            2 => (4 * 1024 * 1024, 4 * 1024 * 1024),
            _ => (8 * 1024 * 1024, 8 * 1024 * 1024),
        }
    }

    /// Proactively triggers the next background prefetch task if the forward buffer has room
    /// or if the unread look-ahead distance is getting close to the low-water mark.
    fn maybe_trigger_prefetch(&mut self, read_cursor: u64) {
        if !self.enable_prefetch || self.prefetch_task.is_some() {
            return;
        }
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if forward_end >= self.file_size {
            return;
        }

        let bytes_ahead = forward_end.saturating_sub(read_cursor);
        let (fetch_size, _) = self.progressive_fetch_and_prefetch_sizes();

        // In progressive streaming mode, maintain at least 8 MB of look-ahead cushion so network jitter never stalls decoders.
        // Also prefetch whenever the forward buffer has capacity to grow.
        let trigger_threshold = if self.enable_progressive {
            (8 * 1024 * 1024u64).max((fetch_size * 2) as u64)
        } else {
            (fetch_size / 2) as u64
        };

        let buffer_has_room = self.forward_buffer.len() < self.forward_capacity;
        let should_trigger = if self.enable_progressive {
            bytes_ahead <= trigger_threshold || buffer_has_room
        } else {
            bytes_ahead <= trigger_threshold
        };

        if should_trigger && buffer_has_room {
            self.sequential_hit_streak = self.sequential_hit_streak.saturating_add(1);
            let (_, next_prefetch_size) = self.progressive_fetch_and_prefetch_sizes();
            let prefetch_len = (next_prefetch_size as u64).min(self.file_size - forward_end);
            if prefetch_len > 0 {
                let prefetch_end = (forward_end + prefetch_len).saturating_sub(1);
                let source = Arc::clone(&self.source);
                self.prefetch_range = Some((forward_end, prefetch_end + 1));
                self.prefetch_task = self.spawn_prefetch_task(source, forward_end, prefetch_end);
            }
        }
    }

    /// Reads up to `len` bytes starting at `offset`.
    ///
    /// Coalesces tiny OS reads via sliding forward buffer and services tail seeks from the tail slot.
    pub async fn read(&mut self, offset: u64, len: usize) -> Result<Bytes, VfsError> {
        if offset >= self.file_size || len == 0 {
            return Ok(Bytes::new());
        }

        let effective_len = (len as u64).min(self.file_size - offset) as usize;

        // 1. Media Player Tail-Seek Optimization: Check if read lands in final 8 MB (files > 16 MB) or 512 KB
        let tail_boundary = if self.file_size > 16 * 1024 * 1024 {
            self.file_size.saturating_sub(Self::EXPANDED_TAIL_THRESHOLD_OFFSET)
        } else {
            self.file_size.saturating_sub(Self::TAIL_THRESHOLD_OFFSET)
        };

        if self.file_size > Self::TAIL_THRESHOLD_OFFSET && offset >= tail_boundary {
            return self.read_from_tail_slot(offset, effective_len).await;
        }

        // 2. Forward Streaming Window
        self.read_from_forward_window(offset, effective_len).await
    }

    /// Serves tail header reads (e.g. MP4 `moov` atom / zip directory) without clearing forward window.
    async fn read_from_tail_slot(&mut self, offset: u64, len: usize) -> Result<Bytes, VfsError> {
        // Check if tail slot already covers this request
        if let Some(tail_start) = self.tail_start_offset {
            let tail_end = tail_start + self.tail_buffer.len() as u64;
            if offset >= tail_start && offset + len as u64 <= tail_end {
                let start_idx = (offset - tail_start) as usize;
                let end_idx = start_idx + len;
                debug!(offset, len, "Tail slot cache hit (<0.005 ms)");
                return Ok(Bytes::copy_from_slice(&self.tail_buffer[start_idx..end_idx]));
            }
        }

        // Tail cache miss: fetch a 512 KB slice around offset ending at EOF or offset + 512 KB
        let slice_len = (512 * 1024).max(self.tail_capacity as u64);
        let tail_fetch_start = offset;
        let tail_fetch_end = (offset + slice_len).min(self.file_size).saturating_sub(1);

        debug!(
            tail_fetch_start,
            tail_fetch_end,
            "Tail slot cache miss: fetching header slice"
        );

        let fetched = self
            .source
            .fetch_range(tail_fetch_start, tail_fetch_end)
            .await?;

        // Replace tail buffer without clearing forward buffer, resetting streak, or aborting prefetch
        self.tail_buffer.fill(0);
        self.tail_buffer = fetched.to_vec();
        self.tail_start_offset = Some(tail_fetch_start);

        let start_idx = (offset - tail_fetch_start) as usize;
        let end_idx = (start_idx + len).min(self.tail_buffer.len());
        Ok(Bytes::copy_from_slice(&self.tail_buffer[start_idx..end_idx]))
    }

    /// Serves sequential and small forward reads using the sliding window with fast slice copying.
    async fn read_from_forward_window(&mut self, offset: u64, len: usize) -> Result<Bytes, VfsError> {
        let min_retain = self.calculate_min_retain_offset(offset);

        // 1. Ingest finished background prefetch task without blocking
        if let Some(task) = &self.prefetch_task
            && task.is_finished()
            && let Some(task) = self.prefetch_task.take()
        {
            let range = self.prefetch_range.take();
            if let Ok(Ok(bytes)) = task.await
                && let Some((start_offset, _end_offset)) = range
            {
                let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
                if start_offset == forward_end {
                    self.append_to_forward_buffer(&bytes, min_retain);
                } else if start_offset > forward_end && offset >= start_offset {
                    self.forward_buffer.make_contiguous().fill(0);
                    self.forward_buffer.clear();
                    self.forward_start_offset = start_offset;
                    self.forward_buffer = VecDeque::from(bytes.to_vec());
                }
            }
        }

        // 2. Full hit in forward_buffer
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if offset >= self.forward_start_offset && offset + len as u64 <= forward_end {
            if self.enable_progressive {
                // Drain excess preceding bytes if buffer exceeds capacity, bounded by min_retain
                if self.forward_buffer.len() > self.forward_capacity && self.forward_start_offset < min_retain {
                    let excess = self.forward_buffer.len() - self.forward_capacity;
                    let max_drainable = (min_retain - self.forward_start_offset) as usize;
                    let extra_drain = excess.min(max_drainable);
                    if extra_drain > 0 {
                        self.forward_buffer.drain(..extra_drain);
                        self.forward_start_offset += extra_drain as u64;
                    }
                }
            } else {
                let rel_offset = (offset - self.forward_start_offset) as usize;
                if rel_offset > 0 {
                    self.forward_buffer.drain(..rel_offset);
                    self.forward_start_offset = offset;
                }
            }

            let result = self.copy_out_slice(offset, len);
            self.last_read_end = offset + len as u64;
            self.maybe_trigger_prefetch(offset + len as u64);
            return Ok(result);
        }

        // 3. Range-inclusive prefetch absorption: check if in-flight prefetch covers offset
        if self.try_absorb_prefetch_if_covers(offset, min_retain).await {
            let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if offset >= self.forward_start_offset && offset + len as u64 <= forward_end {
                let result = self.copy_out_slice(offset, len);
                self.last_read_end = offset + len as u64;
                self.sequential_hit_streak = self.sequential_hit_streak.saturating_add(1);
                self.maybe_trigger_prefetch(offset + len as u64);
                return Ok(result);
            }
        }

        // 4. Partial hit or sequential continuation:
        // If offset is inside [forward_start_offset, forward_end], we already have the start!
        // We only fetch the missing continuation without tossing away existing bytes!
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if !self.forward_buffer.is_empty()
            && offset >= self.forward_start_offset
            && offset <= forward_end
            && forward_end < self.file_size
        {
            let (fetch_size, _) = self.progressive_fetch_and_prefetch_sizes();
            let needed_len = ((offset + len as u64) - forward_end).max(fetch_size as u64).min(self.file_size - forward_end);
            if needed_len > 0 {
                let fetch_end = (forward_end + needed_len).saturating_sub(1);
                let fetched = self.source.fetch_range(forward_end, fetch_end).await?;
                self.append_to_forward_buffer(&fetched, min_retain);

                let new_forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
                if offset >= self.forward_start_offset && offset + len as u64 <= new_forward_end {
                    let result = self.copy_out_slice(offset, len);
                    self.last_read_end = offset + len as u64;
                    self.sequential_hit_streak = self.sequential_hit_streak.saturating_add(1);
                    self.maybe_trigger_prefetch(offset + len as u64);
                    return Ok(result);
                }
            }
        }

        // 5. Genuine Seek or Cold Initial Load
        let is_initial = self.forward_buffer.is_empty() && self.forward_start_offset == 0;
        let is_major_jump = if offset > forward_end {
            offset - forward_end > 4 * 1024 * 1024
        } else if offset < self.forward_start_offset {
            self.forward_start_offset - offset > 4 * 1024 * 1024
        } else {
            false
        };

        if is_initial || is_major_jump {
            self.sequential_hit_streak = 0;
        } else {
            self.sequential_hit_streak = self.sequential_hit_streak.max(1);
        }

        if let Some(task) = self.prefetch_task.take() {
            task.abort();
        }
        self.prefetch_range = None;
        self.forward_buffer.make_contiguous().fill(0);
        self.forward_buffer.clear();

        let (fetch_size, prefetch_size) = self.progressive_fetch_and_prefetch_sizes();
        let fetch_len = (fetch_size as u64).min(self.file_size - offset);
        let fetch_end = (offset + fetch_len).saturating_sub(1);

        debug!(
            offset,
            fetch_end,
            streak = self.sequential_hit_streak,
            fetch_size,
            prefetch_size,
            "Forward window cache miss: issuing Range GET"
        );

        let fetched = self.source.fetch_range(offset, fetch_end).await?;

        self.forward_buffer = VecDeque::from(fetched.to_vec());
        self.forward_start_offset = offset;
        self.last_read_end = offset + len as u64;

        let result = self.copy_out_slice(offset, len);

        // Immediately start background prefetch for the NEXT segment if prefetch_size > 0
        if self.enable_prefetch && prefetch_size > 0 {
            let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if current_end < self.file_size {
                let prefetch_len = (prefetch_size as u64).min(self.file_size - current_end);
                if prefetch_len > 0 {
                    let prefetch_end = (current_end + prefetch_len).saturating_sub(1);
                    let source = Arc::clone(&self.source);
                    self.prefetch_range = Some((current_end, prefetch_end + 1));
                    self.prefetch_task = self.spawn_prefetch_task(source, current_end, prefetch_end);
                }
            }
        }

        Ok(result)
    }

    /// Total active heap RAM currently occupied by forward and tail buffers in bytes.
    #[must_use]
    pub fn current_ram_usage(&self) -> usize {
        self.forward_buffer.len() + self.tail_buffer.len()
    }

    /// Number of bytes reserved in the global backpressure valve.
    #[must_use]
    pub fn allocated_memory_bytes(&self) -> usize {
        self.reservation.as_ref().map_or(0, MemoryReservation::allocated_bytes)
    }

    /// Bound file handle ID.
    #[must_use]
    pub fn fh(&self) -> u64 {
        self.fh
    }
}

impl Drop for EphemeralReadHandle {
    fn drop(&mut self) {
        if let Some(task) = self.prefetch_task.take() {
            task.abort();
        }
        self.prefetch_range = None;

        // Zeroize memory buffers before dropping
        self.forward_buffer.make_contiguous().fill(0);
        self.forward_buffer.clear();

        self.tail_buffer.fill(0);
        self.tail_buffer.clear();

        debug!(fh = self.fh, "EphemeralReadHandle dropped and buffers zeroized");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_window_coalescing_and_eviction() {
        // Create 1 MB synthetic data
        let mut data = Vec::with_capacity(1024 * 1024);
        for i in 0..(1024 * 1024) {
            data.push((i % 256) as u8);
        }
        let source = Arc::new(MockByteSource::new(Bytes::from(data.clone())));
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralReadHandle::with_capacities(
            1,
            source,
            &valve,
            64 * 1024, // 64 KB forward window
            4 * 1024,  // 4 KB tail window
        )
        .expect("Handle creation failed");

        // 1. Read first 4 KB (offset 0, len 4096)
        let chunk1 = handle.read(0, 4096).await.expect("Read failed");
        assert_eq!(chunk1.len(), 4096);
        assert_eq!(&chunk1[..], &data[0..4096]);
        assert_eq!(handle.current_ram_usage(), 64 * 1024);

        // 2. Read next 4 KB (offset 4096, len 4096) -> Cache hit + eviction of previous 4096 bytes
        let chunk2 = handle.read(4096, 4096).await.expect("Read failed");
        assert_eq!(chunk2.len(), 4096);
        assert_eq!(&chunk2[..], &data[4096..8192]);
        // Forward buffer dropped the first 4096 bytes, so length is now 60 KB
        assert_eq!(handle.current_ram_usage(), 60 * 1024);

        // 3. Jump forward to offset 80 KB (cache miss -> loads new 64 KB window)
        let chunk3 = handle.read(80 * 1024, 1024).await.expect("Read failed");
        assert_eq!(chunk3.len(), 1024);
        assert_eq!(&chunk3[..], &data[80 * 1024..(80 * 1024 + 1024)]);
    }

    #[tokio::test]
    async fn test_tail_seek_preserves_forward_window() {
        // 2 MB file (threshold 512 KB)
        let size = 2 * 1024 * 1024;
        let mut data = Vec::with_capacity(size);
        for i in 0..size {
            data.push((i % 251) as u8);
        }
        let source = Arc::new(MockByteSource::new(Bytes::from(data.clone())));
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralReadHandle::with_capacities(
            2,
            source,
            &valve,
            128 * 1024, // 128 KB forward window
            16 * 1024,  // 16 KB tail window
        )
        .expect("Handle creation failed");

        // 1. Read start of file
        let chunk_start = handle.read(0, 1024).await.expect("Start read failed");
        assert_eq!(&chunk_start[..], &data[0..1024]);
        assert_eq!(handle.forward_start_offset, 0);

        // 2. Jump to tail (e.g. final 100 KB, within 512 KB tail boundary)
        let tail_offset = (size - 100 * 1024) as u64;
        let chunk_tail = handle.read(tail_offset, 512).await.expect("Tail read failed");
        assert_eq!(&chunk_tail[..], &data[tail_offset as usize..(tail_offset as usize + 512)]);
        assert!(handle.tail_start_offset.is_some());

        // 3. Jump back to near-start (offset 1024)
        // Forward window should NOT have been blown away!
        assert_eq!(handle.forward_start_offset, 0);
        let chunk_back = handle.read(1024, 1024).await.expect("Back read failed");
        assert_eq!(&chunk_back[..], &data[1024..2048]);
    }

    struct SyntheticByteSource(u64);
    impl ByteSource for SyntheticByteSource {
        fn file_size(&self) -> u64 {
            self.0
        }
        fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
            Box::pin(async move {
                if start >= self.0 {
                    return Ok(Bytes::new());
                }
                let end = (end + 1).min(self.0);
                let len = (end.saturating_sub(start)) as usize;
                Ok(Bytes::from(vec![0xAA; len]))
            })
        }
    }

    #[tokio::test]
    async fn test_progressive_fast_start_and_hot_header_cache() {
        // 50 MB synthetic media file
        let size = 50 * 1024 * 1024;
        let source = Arc::new(SyntheticByteSource(size));
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralReadHandle::with_capacities(
            10,
            source,
            &valve,
            16 * 1024 * 1024, // 16 MB forward window
            512 * 1024,       // 512 KB tail window
        )
        .expect("Handle creation failed");

        handle.enable_progressive(true);
        assert!(handle.is_progressive());
        assert_eq!(handle.sequential_hit_streak(), 0);

        // 1. Initial Probe (Streak 0): Fetch size must be 512 KB with 2 MB background prefetch
        let (fetch_size, prefetch_size) = handle.progressive_fetch_and_prefetch_sizes();
        assert_eq!(fetch_size, 512 * 1024, "Streak 0 must fetch 512 KB");
        assert_eq!(prefetch_size, 2 * 1024 * 1024, "Streak 0 must spawn 2 MB prefetch");

        let probe = handle.read(0, 4096).await.expect("Initial probe read failed");
        assert_eq!(probe.len(), 4096);
        // Only 512 KB loaded into RAM, not 16 MB!
        assert_eq!(handle.current_ram_usage(), 512 * 1024);
        assert_eq!(handle.sequential_hit_streak(), 0);

        // 2. Expanded 8 MB Tail Probe on large file
        let tail_offset = size - 1024 * 1024; // 1 MB before EOF (within 8 MB)
        let _tail_chunk = handle.read(tail_offset, 4096).await.expect("Tail probe failed");
        // Forward start offset must remain 0 (not blown away!)
        assert_eq!(handle.forward_start_offset, 0);
        assert_eq!(handle.sequential_hit_streak(), 0);

        // 3. Sequential continuation: Read through the initial 512 KB block
        let step = 64 * 1024;
        let mut cur = 4096;
        while cur + step <= 512 * 1024 {
            let chunk = handle.read(cur as u64, step).await.expect("Read failed");
            assert_eq!(chunk.len(), step);
            cur += step;
        }

        // Reading past 512 KB boundary triggers Streak 1 ramp-up (2 MB fetch, 2 MB prefetch)
        let chunk_ramp1 = handle.read(512 * 1024, 4096).await.expect("Ramp 1 failed");
        assert_eq!(chunk_ramp1.len(), 4096);
        assert!(handle.sequential_hit_streak() >= 1);
        let (fetch_size, prefetch_size) = handle.progressive_fetch_and_prefetch_sizes();
        assert_eq!(fetch_size, 2 * 1024 * 1024);
        assert_eq!(prefetch_size, 2 * 1024 * 1024);

        // 4. Random seek resets streak back to 0
        let _random_seek = handle.read(8 * 1024 * 1024, 4096).await.expect("Random seek failed");
        assert_eq!(handle.sequential_hit_streak(), 0);
        let (fetch_size, prefetch_size) = handle.progressive_fetch_and_prefetch_sizes();
        assert_eq!(fetch_size, 512 * 1024);
        assert_eq!(prefetch_size, 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn test_continuous_video_playback_zero_stalls_and_interleaved_av() {
        // 50 MB synthetic media file
        let size = 50 * 1024 * 1024;
        let source = Arc::new(SyntheticByteSource(size));
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralReadHandle::with_capacities(
            20,
            source,
            &valve,
            16 * 1024 * 1024, // 16 MB forward window
            512 * 1024,       // 512 KB tail window
        )
        .expect("Handle creation failed");

        handle.enable_prefetch(true);
        handle.enable_progressive(true);

        // 1. Simulate hot_header_cache seeding 0..512 KB
        let initial_header = Bytes::from(vec![0xBB; 512 * 1024]);
        handle.seed_initial_header(0, initial_header);

        assert!(handle.sequential_hit_streak() >= 1);
        assert_eq!(handle.forward_start_offset, 0);
        assert_eq!(handle.forward_buffer.len(), 512 * 1024);

        // 2. Interleaved Audio/Video reading up to 20 MB
        // Video advances by +64 KB, Audio reads interleaved -32 KB
        let mut video_offset = 0u64;
        let video_chunk = 64 * 1024;
        let audio_chunk = 32 * 1024;
        let target_playback = 20 * 1024 * 1024;

        let mut prefetch_triggered_count = 0;

        while video_offset < target_playback {
            // Read video packet (+64 KB)
            let v_data = handle.read(video_offset, video_chunk).await.expect("Video read failed");
            assert_eq!(v_data.len(), video_chunk);
            assert!(handle.sequential_hit_streak() >= 1, "Streak must not reset on video read");

            // Check if prefetch task is active / triggered during playback
            if handle.prefetch_task.is_some() {
                prefetch_triggered_count += 1;
            }

            // Read interleaved audio packet (backward by 32 KB within current forward buffer)
            if video_offset >= audio_chunk as u64 {
                let audio_offset = video_offset.saturating_sub(audio_chunk as u64);
                let a_data = handle.read(audio_offset, audio_chunk).await.expect("Audio read failed");
                assert_eq!(a_data.len(), audio_chunk);
                // Crucial assertion: sequential_hit_streak MUST NOT reset on interleaved audio seek!
                assert!(
                    handle.sequential_hit_streak() >= 1,
                    "Streak must never reset on interleaved audio reads within 1 MB look-behind!"
                );
            }

            video_offset += video_chunk as u64;
        }

        // Verify that low-water-mark proactive prefetching triggered repeatedly throughout playback
        assert!(
            prefetch_triggered_count > 0,
            "Low-water-mark prefetching must trigger during cache hits before buffer runs dry"
        );
        // Verify ramp-up to sustained streaming (streak >= 3)
        assert!(handle.sequential_hit_streak() >= 3);
        let (fetch_size, prefetch_size) = handle.progressive_fetch_and_prefetch_sizes();
        assert_eq!(fetch_size, 8 * 1024 * 1024);
        assert_eq!(prefetch_size, 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn test_partial_overlap_never_corrupts_offsets() {
        let size = 32 * 1024 * 1024; // 32 MB
        let mut data = Vec::with_capacity(size);
        for i in 0..size {
            data.push((i % 251) as u8);
        }
        let source = Arc::new(MockByteSource::new(Bytes::from(data.clone())));
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralReadHandle::with_capacities(
            30,
            source,
            &valve,
            8 * 1024 * 1024, // 8 MB forward window
            256 * 1024,      // 256 KB tail window
        )
        .expect("Handle creation failed");

        handle.enable_progressive(true);

        // 1. Fill forward_buffer to exact capacity (8 MB)
        handle.forward_buffer = VecDeque::from(data[0..8 * 1024 * 1024].to_vec());
        handle.forward_start_offset = 0;
        assert_eq!(handle.forward_start_offset, 0);
        assert_eq!(handle.forward_buffer.len(), 8 * 1024 * 1024);

        // 2. Read crossing the exact 8 MB boundary: offset = 8 MB - 16 KB, len = 64 KB
        let offset = 8 * 1024 * 1024 - 16 * 1024;
        let len = 64 * 1024;
        let chunk2 = handle.read(offset, len).await.expect("Boundary read failed");
        assert_eq!(chunk2.len(), len);

        // Assert that EVERY byte returned matches exactly (offset + i) % 251 with ZERO corruption or shift!
        for (i, &byte) in chunk2.iter().enumerate() {
            let expected = ((offset + i as u64) % 251) as u8;
            assert_eq!(
                byte, expected,
                "Byte corruption at index {i} (offset {}): got {byte:#04x}, expected {expected:#04x}",
                offset + i as u64
            );
        }

        // Verify that forward_start_offset NEVER exceeded offset!
        assert!(
            handle.forward_start_offset <= offset,
            "forward_start_offset ({}) must never exceed read offset ({})",
            handle.forward_start_offset,
            offset
        );
    }

    #[tokio::test]
    async fn test_multi_handle_shared_inode_cache_deduplication() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingByteSource {
            inner: MockByteSource,
            fetch_count: Arc<AtomicUsize>,
        }

        impl ByteSource for CountingByteSource {
            fn file_size(&self) -> u64 {
                self.inner.file_size()
            }
            fn fetch_range<'a>(&'a self, start: u64, end: u64) -> BoxFuture<'a, Result<Bytes, VfsError>> {
                self.fetch_count.fetch_add(1, Ordering::SeqCst);
                self.inner.fetch_range(start, end)
            }
        }

        let size = 16 * 1024 * 1024; // 16 MB
        let mut data = Vec::with_capacity(size);
        for i in 0..size {
            data.push((i % 251) as u8);
        }
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let counting_source = Arc::new(CountingByteSource {
            inner: MockByteSource::new(Bytes::from(data.clone())),
            fetch_count: Arc::clone(&fetch_count),
        });

        let shared_cache = Arc::new(SharedInodeBlockCache::default());
        let valve = Arc::new(MemoryBackpressureValve::new());

        let cached_source_1 = Arc::new(CachedByteSource::new(
            1001,
            Arc::clone(&counting_source) as Arc<dyn ByteSource>,
            Arc::clone(&shared_cache),
            None,
        ));
        let cached_source_2 = Arc::new(CachedByteSource::new(
            1001,
            Arc::clone(&counting_source) as Arc<dyn ByteSource>,
            Arc::clone(&shared_cache),
            None,
        ));

        let mut handle_1 = EphemeralReadHandle::with_capacities(
            101,
            cached_source_1,
            &valve,
            8 * 1024 * 1024,
            256 * 1024,
        )
        .expect("Handle 1 failed");

        let mut handle_2 = EphemeralReadHandle::with_capacities(
            102,
            cached_source_2,
            &valve,
            8 * 1024 * 1024,
            256 * 1024,
        )
        .expect("Handle 2 failed");

        // Handle 1 reads first 8 MB (Blocks 0..3)
        let chunk_h1 = handle_1.read(0, 8 * 1024 * 1024).await.expect("H1 read failed");
        assert_eq!(chunk_h1.len(), 8 * 1024 * 1024);
        assert_eq!(&chunk_h1[..], &data[0..8 * 1024 * 1024]);

        let count_after_h1 = fetch_count.load(Ordering::SeqCst);
        assert!(count_after_h1 >= 1, "H1 must fetch blocks from inner source");

        // Handle 2 reads from the same video file (overlapping range in Block 0)
        let chunk_h2 = handle_2.read(0, 2 * 1024 * 1024).await.expect("H2 read failed");
        assert_eq!(chunk_h2.len(), 2 * 1024 * 1024);
        assert_eq!(&chunk_h2[..], &data[0..2 * 1024 * 1024]);

        // Assert 0 duplicate network calls occurred!
        let count_after_h2 = fetch_count.load(Ordering::SeqCst);
        assert_eq!(
            count_after_h2, count_after_h1,
            "Handle 2 must hit SharedInodeBlockCache with 0 duplicate HTTP calls!"
        );
    }
}
