use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use bytes::Bytes;
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
    prefetch_next_offset: u64,
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
            prefetch_next_offset: 0,
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
                        self.prefetch_next_offset = forward_end;
                        self.prefetch_task = self.spawn_prefetch_task(source, forward_end, prefetch_end);
                    }
                }
            }
        }
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
                self.prefetch_next_offset = forward_end;
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
        // 1. Ingest finished background prefetch task without blocking
        if let Some(task) = &self.prefetch_task
            && task.is_finished()
            && let Some(task) = self.prefetch_task.take()
            && let Ok(Ok(bytes)) = task.await
        {
            let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if self.prefetch_next_offset == current_end {
                self.forward_buffer.extend(bytes);
            }
        }

        // 2. Check for Forward Cache Hit
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if offset >= self.forward_start_offset && offset + len as u64 <= forward_end {
            if self.enable_progressive {
                // Keep preceding bytes up to forward_capacity to cushion interleaved audio/video demuxer seeks.
                // Only evict when buffer exceeds forward_capacity, preserving at least LOOK_BEHIND_MARGIN if possible.
                if self.forward_buffer.len() > self.forward_capacity && self.forward_start_offset < offset {
                    let excess = self.forward_buffer.len() - self.forward_capacity;
                    let safe_drain_point = offset.saturating_sub(Self::LOOK_BEHIND_MARGIN);
                    let max_safe_drain = if safe_drain_point > self.forward_start_offset {
                        (safe_drain_point - self.forward_start_offset) as usize
                    } else {
                        (offset - self.forward_start_offset) as usize
                    };
                    let extra_drain = excess.min(max_safe_drain);
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

            let rel_offset = (offset - self.forward_start_offset) as usize;
            let copy_len = len.min(self.forward_buffer.len().saturating_sub(rel_offset));
            let (s1, s2) = self.forward_buffer.as_slices();
            let result = if rel_offset + copy_len <= s1.len() {
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
            };

            self.last_read_end = offset + len as u64;

            // Proactive continuous pipeline prefetch: keep ahead of decoder so buffer never runs dry
            self.maybe_trigger_prefetch(offset + len as u64);

            return Ok(result);
        }

        // 3. Forward Buffer exhausted / read beyond forward_end:
        // If an in-flight prefetch covers this continuation, await and absorb it!
        if let Some(task) = self.prefetch_task.take() {
            let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if self.prefetch_next_offset == current_end {
                if let Ok(Ok(bytes)) = task.await {
                    self.forward_buffer.extend(bytes);
                }
            } else {
                task.abort();
            }
        }

        // Re-check after absorbing in-flight prefetch
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if offset >= self.forward_start_offset && offset + len as u64 <= forward_end {
            if self.enable_progressive {
                if self.forward_buffer.len() > self.forward_capacity && self.forward_start_offset < offset {
                    let excess = self.forward_buffer.len() - self.forward_capacity;
                    let safe_drain_point = offset.saturating_sub(Self::LOOK_BEHIND_MARGIN);
                    let max_safe_drain = if safe_drain_point > self.forward_start_offset {
                        (safe_drain_point - self.forward_start_offset) as usize
                    } else {
                        (offset - self.forward_start_offset) as usize
                    };
                    let extra_drain = excess.min(max_safe_drain);
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

            let rel_offset = (offset - self.forward_start_offset) as usize;
            let copy_len = len.min(self.forward_buffer.len().saturating_sub(rel_offset));
            let (s1, s2) = self.forward_buffer.as_slices();
            let result = if rel_offset + copy_len <= s1.len() {
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
            };

            self.last_read_end = offset + len as u64;
            self.sequential_hit_streak = self.sequential_hit_streak.saturating_add(1);

            // Proactive continuous pipeline prefetch
            self.maybe_trigger_prefetch(offset + len as u64);

            return Ok(result);
        }

        // 4. Genuine Seek or Cold Initial Load:
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

        let rel_offset = (offset - self.forward_start_offset) as usize;
        let copy_len = len.min(self.forward_buffer.len().saturating_sub(rel_offset));
        let (s1, s2) = self.forward_buffer.as_slices();
        let result = if rel_offset + copy_len <= s1.len() {
            Bytes::copy_from_slice(&s1[rel_offset..rel_offset + copy_len])
        } else if rel_offset >= s1.len() {
            let s2_off = rel_offset - s1.len();
            Bytes::copy_from_slice(&s2[s2_off..s2_off + copy_len])
        } else {
            let mut out = Vec::with_capacity(copy_len);
            out.extend_from_slice(s1);
            out.extend_from_slice(&s2[..copy_len - s1.len()]);
            Bytes::from(out)
        };

        // Immediately start background prefetch for the NEXT segment if prefetch_size > 0
        if self.enable_prefetch && prefetch_size > 0 {
            let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if current_end < self.file_size {
                let prefetch_len = (prefetch_size as u64).min(self.file_size - current_end);
                if prefetch_len > 0 {
                    let prefetch_end = (current_end + prefetch_len).saturating_sub(1);
                    let source = Arc::clone(&self.source);
                    self.prefetch_next_offset = current_end;
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
}
