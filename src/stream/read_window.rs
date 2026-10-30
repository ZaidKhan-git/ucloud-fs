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
}

impl EphemeralReadHandle {
    /// Default forward sliding window capacity: 8 MB (smooth 1080p video streaming).
    pub const DEFAULT_FORWARD_CAPACITY: usize = 8 * 1024 * 1024;
    /// Default tail header slot capacity: 256 KB.
    pub const DEFAULT_TAIL_CAPACITY: usize = 256 * 1024;
    /// Tail threshold offset: within the final 512 KB of the file.
    pub const TAIL_THRESHOLD_OFFSET: u64 = 512 * 1024;

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
        })
    }

    /// Toggles proactive background prefetching for smooth video/audio playback.
    pub fn enable_prefetch(&mut self, enable: bool) {
        self.enable_prefetch = enable;
    }

    /// Reads up to `len` bytes starting at `offset`.
    ///
    /// Coalesces tiny OS reads via sliding forward buffer and services tail seeks from the tail slot.
    pub async fn read(&mut self, offset: u64, len: usize) -> Result<Bytes, VfsError> {
        if offset >= self.file_size || len == 0 {
            return Ok(Bytes::new());
        }

        let effective_len = (len as u64).min(self.file_size - offset) as usize;

        // 1. Media Player Tail-Seek Optimization: Check if read lands in final 512 KB
        let tail_boundary = self.file_size.saturating_sub(Self::TAIL_THRESHOLD_OFFSET);
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

        // Tail cache miss: fetch up to tail_capacity bytes ending at EOF
        let tail_fetch_start = self.file_size.saturating_sub(self.tail_capacity as u64).min(offset);
        let tail_fetch_end = self.file_size.saturating_sub(1);

        debug!(
            tail_fetch_start,
            tail_fetch_end,
            "Tail slot cache miss: fetching header slice"
        );

        let fetched = self
            .source
            .fetch_range(tail_fetch_start, tail_fetch_end)
            .await?;

        // Replace tail buffer
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
            let rel_offset = (offset - self.forward_start_offset) as usize;
            if rel_offset > 0 {
                self.forward_buffer.drain(..rel_offset);
                self.forward_start_offset = offset;
            }

            let copy_len = len.min(self.forward_buffer.len());
            let (s1, s2) = self.forward_buffer.as_slices();
            let result = if copy_len <= s1.len() {
                Bytes::copy_from_slice(&s1[..copy_len])
            } else {
                let mut out = Vec::with_capacity(copy_len);
                out.extend_from_slice(s1);
                out.extend_from_slice(&s2[..copy_len - s1.len()]);
                Bytes::from(out)
            };

            // Non-blocking proactive prefetch: spawn background fetch if buffer is below half capacity
            if self.enable_prefetch && self.prefetch_task.is_none() && self.forward_buffer.len() <= self.forward_capacity / 2 {
                let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
                if current_end < self.file_size {
                    let prefetch_cap = (self.forward_capacity / 2).max(1024 * 1024);
                    let fetch_len = (prefetch_cap as u64).min(self.file_size - current_end);
                    if fetch_len > 0 {
                        let fetch_end = (current_end + fetch_len).saturating_sub(1);
                        let source = Arc::clone(&self.source);
                        self.prefetch_next_offset = current_end;
                        self.prefetch_task = Some(tokio::spawn(async move {
                            source.fetch_range(current_end, fetch_end).await
                        }));
                    }
                }
            }

            return Ok(result);
        }

        // 3. Forward Buffer exhausted: If an in-flight prefetch covers this sequential continuation, await it
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

        // Re-check after awaiting in-flight prefetch
        let forward_end = self.forward_start_offset + self.forward_buffer.len() as u64;
        if offset >= self.forward_start_offset && offset + len as u64 <= forward_end {
            let rel_offset = (offset - self.forward_start_offset) as usize;
            if rel_offset > 0 {
                self.forward_buffer.drain(..rel_offset);
                self.forward_start_offset = offset;
            }

            let copy_len = len.min(self.forward_buffer.len());
            let (s1, s2) = self.forward_buffer.as_slices();
            let result = if copy_len <= s1.len() {
                Bytes::copy_from_slice(&s1[..copy_len])
            } else {
                let mut out = Vec::with_capacity(copy_len);
                out.extend_from_slice(s1);
                out.extend_from_slice(&s2[..copy_len - s1.len()]);
                Bytes::from(out)
            };

            // Spawn next prefetch if needed
            if self.enable_prefetch && self.prefetch_task.is_none() && self.forward_buffer.len() <= self.forward_capacity / 2 {
                let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
                if current_end < self.file_size {
                    let prefetch_cap = (self.forward_capacity / 2).max(1024 * 1024);
                    let fetch_len = (prefetch_cap as u64).min(self.file_size - current_end);
                    if fetch_len > 0 {
                        let fetch_end = (current_end + fetch_len).saturating_sub(1);
                        let source = Arc::clone(&self.source);
                        self.prefetch_next_offset = current_end;
                        self.prefetch_task = Some(tokio::spawn(async move {
                            source.fetch_range(current_end, fetch_end).await
                        }));
                    }
                }
            }

            return Ok(result);
        }

        // 4. Genuine Seek or Initial Load: Clear old forward window
        if let Some(task) = self.prefetch_task.take() {
            task.abort();
        }
        self.forward_buffer.make_contiguous().fill(0);
        self.forward_buffer.clear();

        let fetch_len = (self.forward_capacity as u64).min(self.file_size - offset);
        let fetch_end = (offset + fetch_len).saturating_sub(1);

        debug!(
            offset,
            fetch_end,
            len,
            "Forward window cache miss: issuing Range GET"
        );

        let fetched = self.source.fetch_range(offset, fetch_end).await?;

        self.forward_buffer = VecDeque::from(fetched.to_vec());
        self.forward_start_offset = offset;

        let copy_len = len.min(self.forward_buffer.len());
        let (s1, s2) = self.forward_buffer.as_slices();
        let result = if copy_len <= s1.len() {
            Bytes::copy_from_slice(&s1[..copy_len])
        } else {
            let mut out = Vec::with_capacity(copy_len);
            out.extend_from_slice(s1);
            out.extend_from_slice(&s2[..copy_len - s1.len()]);
            Bytes::from(out)
        };

        // Immediately start background prefetch for the NEXT segment
        if self.enable_prefetch {
            let current_end = self.forward_start_offset + self.forward_buffer.len() as u64;
            if current_end < self.file_size {
                let prefetch_cap = (self.forward_capacity / 2).max(1024 * 1024);
                let prefetch_len = (prefetch_cap as u64).min(self.file_size - current_end);
                if prefetch_len > 0 {
                    let prefetch_end = (current_end + prefetch_len).saturating_sub(1);
                    let source = Arc::clone(&self.source);
                    self.prefetch_next_offset = current_end;
                    self.prefetch_task = Some(tokio::spawn(async move {
                        source.fetch_range(current_end, prefetch_end).await
                    }));
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
}
