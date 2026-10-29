use std::sync::Arc;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::error::VfsError;
use crate::network::client::{ChunkUploadResult, GoogleDriveClient};
use crate::stream::backpressure::{MemoryBackpressureValve, MemoryReservation};

/// Google Drive Resumable Upload byte alignment requirement: 256 KB.
pub const GOOGLE_CHUNK_ALIGNMENT: usize = 256 * 1024;
/// Default intermediate flush threshold: 4 MB (16 x 256 KB).
pub const DEFAULT_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;
/// Maximum RAM staging window capacity: 8 MB.
pub const DEFAULT_MAX_STAGING_CAPACITY: usize = 8 * 1024 * 1024;

/// Result of successfully committing and closing an ephemeral write handle.
#[derive(Debug, Clone)]
pub struct CommitWriteResult {
    pub file_id: String,
    pub total_bytes: u64,
    pub sha256_hex: String,
}

/// Per-Handle 256 KB-Aligned Resumable Write Spooler.
///
/// Accumulates arbitrary unaligned OS writes in RAM, automatically reorders out-of-order writes
/// within the staging window, and flushes 256 KB-aligned slices to Google Drive with zero persistent disk cache.
pub struct EphemeralWriteHandle {
    fh: u64,
    client: Arc<GoogleDriveClient>,
    account_id: String,
    session_uri: String,
    accumulator: Vec<u8>,
    flushed_bytes: u64,
    total_declared_size: Option<u64>,
    sha256_hasher: Sha256,
    flush_threshold: usize,
    reservation: Option<MemoryReservation>,
    is_committed: bool,
    committed_file_id: Option<String>,
}

impl EphemeralWriteHandle {
    /// Creates a new `EphemeralWriteHandle` bound to OS handle `fh` with 8 MB bounded staging buffer.
    pub fn new(
        fh: u64,
        client: Arc<GoogleDriveClient>,
        account_id: String,
        session_uri: String,
        valve: &Arc<MemoryBackpressureValve>,
    ) -> Result<Self, VfsError> {
        Self::with_threshold(
            fh,
            client,
            account_id,
            session_uri,
            valve,
            DEFAULT_FLUSH_THRESHOLD,
        )
    }

    /// Creates an `EphemeralWriteHandle` with custom flush threshold.
    pub fn with_threshold(
        fh: u64,
        client: Arc<GoogleDriveClient>,
        account_id: String,
        session_uri: String,
        valve: &Arc<MemoryBackpressureValve>,
        flush_threshold: usize,
    ) -> Result<Self, VfsError> {
        let reservation = valve
            .acquire_or_shrink(DEFAULT_MAX_STAGING_CAPACITY, 2 * 1024 * 1024)
            .ok_or(VfsError::DiskFull)?;

        Ok(Self {
            fh,
            client,
            account_id,
            session_uri,
            accumulator: Vec::with_capacity(flush_threshold),
            flushed_bytes: 0,
            total_declared_size: None,
            sha256_hasher: Sha256::new(),
            flush_threshold,
            reservation: Some(reservation),
            is_committed: false,
            committed_file_id: None,
        })
    }

    /// Sets the declared total size (e.g. from Windows Explorer `set_file_size` allocation IRP).
    pub fn set_declared_size(&mut self, total_size: u64) {
        self.total_declared_size = Some(total_size);
    }

    /// Accepts arbitrary unaligned write chunks from the OS.
    ///
    /// Reorders writes seamlessly within the staging buffer.
    /// Rejects backward seeks past already-flushed boundaries with `VfsError::NotSupported`.
    pub async fn write(&mut self, offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        if data.is_empty() {
            return Ok(0);
        }

        // Backward seek check: Cannot seek backward into bytes already uploaded and evicted
        if offset < self.flushed_bytes {
            return Err(VfsError::InvalidSeekOrOutOfOrderWrite {
                expected: self.flushed_bytes,
                actual: offset,
            });
        }

        let rel_offset = (offset - self.flushed_bytes) as usize;

        // Ensure accumulator has room up to rel_offset
        if rel_offset > self.accumulator.len() {
            // Pad with zero bytes if application wrote out of order ahead of current offset
            self.accumulator.resize(rel_offset, 0);
            self.accumulator.extend_from_slice(data);
        } else if rel_offset + data.len() <= self.accumulator.len() {
            // In-place overwrite within existing staging buffer
            self.accumulator[rel_offset..rel_offset + data.len()].copy_from_slice(data);
        } else {
            // Partial overwrite and extend
            let overwrite_len = self.accumulator.len() - rel_offset;
            self.accumulator[rel_offset..].copy_from_slice(&data[..overwrite_len]);
            self.accumulator.extend_from_slice(&data[overwrite_len..]);
        }

        // Flush 256 KB-aligned chunks if threshold reached
        if self.accumulator.len() >= self.flush_threshold {
            self.flush_aligned_chunks().await?;
        }

        Ok(data.len())
    }

    /// Flushes all 256 KB-aligned chunks currently in the staging accumulator to Google Drive.
    pub async fn flush_aligned_chunks(&mut self) -> Result<(), VfsError> {
        let aligned_len = (self.accumulator.len() / GOOGLE_CHUNK_ALIGNMENT) * GOOGLE_CHUNK_ALIGNMENT;
        if aligned_len == 0 {
            return Ok(());
        }

        let slice_to_flush = &self.accumulator[..aligned_len];
        self.sha256_hasher.update(slice_to_flush);

        let start_byte = self.flushed_bytes;
        let end_byte = start_byte + aligned_len as u64 - 1;

        debug!(
            fh = self.fh,
            start_byte,
            end_byte,
            aligned_len,
            "Flushing 256 KB-aligned chunk to Google Drive Resumable URI"
        );

        let chunk_bytes = Bytes::copy_from_slice(slice_to_flush);
        let res = self
            .client
            .upload_resumable_chunk_opt(
                &self.account_id,
                &self.session_uri,
                chunk_bytes,
                start_byte,
                end_byte,
                self.total_declared_size,
            )
            .await?;

        match res {
            ChunkUploadResult::Incomplete { .. } => {
                self.flushed_bytes += aligned_len as u64;
                // Zeroize evicted slice before draining to ensure zero residual data in heap
                self.accumulator[..aligned_len].fill(0);
                self.accumulator.drain(..aligned_len);
                Ok(())
            }
            ChunkUploadResult::Complete { file_id } => {
                self.committed_file_id = Some(file_id);
                self.flushed_bytes += aligned_len as u64;
                self.accumulator[..aligned_len].fill(0);
                self.accumulator.drain(..aligned_len);
                Ok(())
            }
        }
    }

    /// Commits and closes the stream, flushing final remaining unaligned bytes.
    ///
    /// Clears and zeroizes the RAM accumulator, returning overall SHA-256 and Google file ID.
    pub async fn commit(&mut self) -> Result<CommitWriteResult, VfsError> {
        if self.is_committed {
            return Err(VfsError::InternalFault(
                "Write handle already committed".to_string(),
            ));
        }

        let rem_len = self.accumulator.len();
        let total_bytes = self.flushed_bytes + rem_len as u64;

        let file_id = if rem_len > 0 || (total_bytes == 0 && self.committed_file_id.is_none()) {
            self.sha256_hasher.update(&self.accumulator);

            let start_byte = self.flushed_bytes;
            let end_byte = total_bytes.saturating_sub(1);

            debug!(
                fh = self.fh,
                start_byte,
                end_byte,
                total_bytes,
                "Flushing final bytes to Google Drive"
            );

            let final_bytes = Bytes::copy_from_slice(&self.accumulator);
            let res = self
                .client
                .upload_resumable_chunk_opt(
                    &self.account_id,
                    &self.session_uri,
                    final_bytes,
                    start_byte,
                    end_byte,
                    Some(total_bytes),
                )
                .await?;

            match res {
                ChunkUploadResult::Complete { file_id } => file_id,
                ChunkUploadResult::Incomplete { .. } => {
                    return Err(VfsError::InternalFault(
                        "Google Drive returned incomplete status on final write chunk".to_string(),
                    ));
                }
            }
        } else if let Some(fid) = self.committed_file_id.take() {
            fid
        } else {
            "completed_empty".to_string()
        };

        let sha256_hex = format!("{:x}", self.sha256_hasher.clone().finalize());

        // Zeroize and drop accumulator RAM immediately
        self.accumulator.fill(0);
        self.accumulator.clear();
        self.accumulator.shrink_to_fit();

        // Release RAM reservation back to valve
        self.reservation = None;
        self.is_committed = true;

        info!(
            fh = self.fh,
            file_id = %file_id,
            total_bytes,
            sha256 = %sha256_hex,
            "Resumable file write committed successfully; RAM dropped to 0"
        );

        Ok(CommitWriteResult {
            file_id,
            total_bytes,
            sha256_hex,
        })
    }

    /// Current RAM occupied by the accumulator in bytes.
    #[must_use]
    pub fn current_ram_usage(&self) -> usize {
        self.accumulator.len()
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

    /// Bound Google account ID.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Flushed bytes so far.
    #[must_use]
    pub fn flushed_bytes(&self) -> u64 {
        self.flushed_bytes
    }
}

impl Drop for EphemeralWriteHandle {
    fn drop(&mut self) {
        // Zeroize all remaining bytes in buffer
        self.accumulator.fill(0);
        self.accumulator.clear();
        debug!(fh = self.fh, "EphemeralWriteHandle dropped and zeroized");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::client::BackoffConfig;
    use crate::network::limiter::MultiTierLimiter;

    #[tokio::test]
    async fn test_write_spool_out_of_order_and_backward_seek() {
        let limiter = Arc::new(MultiTierLimiter::new());
        let client = Arc::new(
            GoogleDriveClient::with_endpoints(
                limiter,
                "http://127.0.0.1:9".to_string(),
                "http://127.0.0.1:9".to_string(),
                BackoffConfig::default(),
            )
            .unwrap(),
        );
        let valve = Arc::new(MemoryBackpressureValve::new());

        let mut handle = EphemeralWriteHandle::with_threshold(
            1,
            client,
            "acc_1".to_string(),
            "http://127.0.0.1:9/session".to_string(),
            &valve,
            1024 * 1024, // 1 MB flush threshold
        )
        .unwrap();

        // 1. Write out-of-order within staging buffer: write offset 10..20, then offset 0..10
        handle.write(10, &[0xAA; 10]).await.unwrap();
        assert_eq!(handle.accumulator.len(), 20);
        assert_eq!(&handle.accumulator[0..10], &[0x00; 10]); // Padded with zeros
        assert_eq!(&handle.accumulator[10..20], &[0xAA; 10]);

        handle.write(0, &[0xBB; 10]).await.unwrap();
        assert_eq!(&handle.accumulator[0..10], &[0xBB; 10]);
        assert_eq!(&handle.accumulator[10..20], &[0xAA; 10]);

        // 2. Simulate flushed_bytes advanced to 100
        handle.flushed_bytes = 100;
        // Attempt backward seek to offset 50 (past flushed 100) -> Must return InvalidSeekOrOutOfOrderWrite
        let res = handle.write(50, &[0xCC; 10]).await;
        match res {
            Err(VfsError::InvalidSeekOrOutOfOrderWrite { expected, actual }) => {
                assert_eq!(expected, 100);
                assert_eq!(actual, 50);
            }
            _ => panic!("Expected InvalidSeekOrOutOfOrderWrite on backward seek"),
        }
    }
}
