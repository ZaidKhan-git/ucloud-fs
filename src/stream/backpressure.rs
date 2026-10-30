use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

/// Global RAM Backpressure Valve enforcing a strict memory ceiling across all stream buffers.
#[derive(Debug)]
pub struct MemoryBackpressureValve {
    total_allocated_bytes: AtomicUsize,
    max_ceiling_bytes: usize,
}

impl MemoryBackpressureValve {
    /// Default global streaming RAM ceiling: 150 MB (accommodates high-bitrate video streaming buffers).
    pub const DEFAULT_MAX_CEILING_BYTES: usize = 150 * 1024 * 1024;

    /// Creates a valve with the default 150 MB ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ceiling(Self::DEFAULT_MAX_CEILING_BYTES)
    }

    /// Creates a valve with a custom memory ceiling (ideal for testing).
    #[must_use]
    pub fn with_ceiling(max_ceiling_bytes: usize) -> Self {
        Self {
            total_allocated_bytes: AtomicUsize::new(0),
            max_ceiling_bytes,
        }
    }

    /// Attempts to reserve `requested_bytes`. If not available, tries to reserve `min_bytes`
    /// (e.g. shrinking sliding window from 8 MB to 2 MB).
    #[must_use]
    pub fn acquire_or_shrink(
        self: &Arc<Self>,
        requested_bytes: usize,
        min_bytes: usize,
    ) -> Option<MemoryReservation> {
        // Try requested first
        if let Some(guard) = self.try_acquire(requested_bytes) {
            return Some(guard);
        }

        // Try minimum second
        if min_bytes < requested_bytes && let Some(guard) = self.try_acquire(min_bytes) {
            debug!(
                requested = requested_bytes,
                allocated = min_bytes,
                "Shrunk stream buffer window under RAM backpressure"
            );
            return Some(guard);
        }

        warn!(
            requested = requested_bytes,
            current_usage = self.current_usage(),
            ceiling = self.max_ceiling_bytes,
            "Stream buffer allocation rejected: global RAM ceiling reached"
        );
        None
    }

    /// Attempts to atomically reserve an exact amount of bytes.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>, bytes: usize) -> Option<MemoryReservation> {
        let mut current = self.total_allocated_bytes.load(Ordering::Relaxed);
        loop {
            if current.saturating_add(bytes) > self.max_ceiling_bytes {
                return None;
            }

            match self.total_allocated_bytes.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(MemoryReservation {
                        valve: Arc::clone(self),
                        allocated_bytes: bytes,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Releases previously allocated bytes back to the global pool.
    pub(crate) fn release(&self, bytes: usize) {
        self.total_allocated_bytes.fetch_sub(bytes, Ordering::SeqCst);
    }

    /// Current global active streaming RAM usage in bytes.
    #[must_use]
    pub fn current_usage(&self) -> usize {
        self.total_allocated_bytes.load(Ordering::SeqCst)
    }

    /// Configured global maximum memory ceiling in bytes.
    #[must_use]
    pub fn ceiling(&self) -> usize {
        self.max_ceiling_bytes
    }
}

impl Default for MemoryBackpressureValve {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII reservation guard that automatically returns allocated streaming bytes on drop.
#[derive(Debug)]
pub struct MemoryReservation {
    valve: Arc<MemoryBackpressureValve>,
    allocated_bytes: usize,
}

impl MemoryReservation {
    /// Number of bytes allocated in this reservation.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.valve.release(self.allocated_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valve_acquire_release_and_ceiling() {
        let valve = Arc::new(MemoryBackpressureValve::with_ceiling(10 * 1024 * 1024));
        assert_eq!(valve.current_usage(), 0);

        let res1 = valve.try_acquire(6 * 1024 * 1024).expect("res1 should succeed");
        assert_eq!(valve.current_usage(), 6 * 1024 * 1024);

        // Try to allocate 5 MB (6 + 5 = 11 > 10 MB ceiling) -> Should fail
        assert!(valve.try_acquire(5 * 1024 * 1024).is_none());

        // Shrink allocation: requested 5 MB, min 2 MB (6 + 2 = 8 <= 10) -> Should succeed with 2 MB
        let res2 = valve.acquire_or_shrink(5 * 1024 * 1024, 2 * 1024 * 1024).expect("res2 should shrink");
        assert_eq!(res2.allocated_bytes(), 2 * 1024 * 1024);
        assert_eq!(valve.current_usage(), 8 * 1024 * 1024);

        // Drop res1: releases 6 MB -> current usage drops to 2 MB
        drop(res1);
        assert_eq!(valve.current_usage(), 2 * 1024 * 1024);

        drop(res2);
        assert_eq!(valve.current_usage(), 0);
    }
}
