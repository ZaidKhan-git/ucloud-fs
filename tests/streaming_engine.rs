use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use ucloud_fs::error::VfsError;
use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::stream::backpressure::MemoryBackpressureValve;
use ucloud_fs::stream::read_window::{EphemeralReadHandle, SingleAccountByteSource};
use ucloud_fs::stream::write_spool::EphemeralWriteHandle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Deterministic synthetic byte generator for virtual 500 MB file.
fn synthetic_byte_at(offset: u64) -> u8 {
    ((offset ^ (offset >> 8) ^ 0x5A) % 251) as u8
}

/// Dynamic HTTP Range responder synthesizing slices of a 500 MB file on demand.
struct SyntheticRangeResponder {
    file_size: u64,
    request_counter: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl Respond for SyntheticRangeResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.request_counter.fetch_add(1, Ordering::SeqCst);

        let range_hdr = request
            .headers
            .get(wiremock::http::HeaderName::from_static("range"))
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();

        // Parse bytes=start-end
        let (start, end) = if let Some(stripped) = range_hdr.strip_prefix("bytes=") {
            let mut parts = stripped.split('-');
            let start = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let end = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(self.file_size.saturating_sub(1));
            (start, end)
        } else {
            (0, self.file_size.saturating_sub(1))
        };

        if let Ok(mut g) = self.ranges.lock() {
            g.push((start, end));
        }

        let slice_len = (end.saturating_sub(start) + 1) as usize;
        let mut body = Vec::with_capacity(slice_len);
        for off in start..=end {
            body.push(synthetic_byte_at(off));
        }

        ResponseTemplate::new(206)
            .insert_header(
                "Content-Range",
                format!("bytes {}-{}/{}", start, end, self.file_size).as_str(),
            )
            .set_body_bytes(body)
    }
}

#[tokio::test]
async fn test_mp4_media_player_seek_simulation() {
    let server = MockServer::start().await;

    let file_size: u64 = 500_000_000; // 500 MB (decimal)
    let request_counter = Arc::new(AtomicUsize::new(0));
    let ranges = Arc::new(Mutex::new(Vec::new()));

    let responder = SyntheticRangeResponder {
        file_size,
        request_counter: Arc::clone(&request_counter),
        ranges: Arc::clone(&ranges),
    };

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/video_500mb"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .expect("Client build failed"),
    );

    let valve = Arc::new(MemoryBackpressureValve::new());
    let source = Arc::new(SingleAccountByteSource::new(
        client,
        "acc_1".to_string(),
        "mock_token".to_string(),
        "video_500mb".to_string(),
        file_size,
    ));

    let mut handle = EphemeralReadHandle::new(101, source, &valve).expect("Handle creation failed");

    // 1. Initial read: bytes 0..4096 (Request 1: triggers forward sliding window 0..8MB)
    let chunk1 = handle.read(0, 4096).await.expect("Read 0..4096 failed");
    assert_eq!(chunk1.len(), 4096);
    for (i, &b) in chunk1.iter().enumerate() {
        assert_eq!(b, synthetic_byte_at(i as u64));
    }
    assert_eq!(request_counter.load(Ordering::SeqCst), 1);
    assert!(handle.current_ram_usage() <= 8 * 1024 * 1024);

    // 2. Jump to tail: 499_900_000..500_000_000 (Request 2: lands in Tail Slot within last 512 KB)
    // Does NOT blow away forward sliding window!
    let tail_len = (500_000_000 - 499_900_000) as usize;
    let chunk_tail = handle
        .read(499_900_000, tail_len)
        .await
        .expect("Tail read failed");
    assert_eq!(chunk_tail.len(), tail_len);
    for (i, &b) in chunk_tail.iter().enumerate() {
        assert_eq!(b, synthetic_byte_at(499_900_000 + i as u64));
    }
    assert_eq!(request_counter.load(Ordering::SeqCst), 2);
    // RAM usage must not exceed 8.25 MB (8 MB forward + 256 KB tail)
    let max_ram_ceiling = (8 * 1024 * 1024) + (256 * 1024);
    assert!(
        handle.current_ram_usage() <= max_ram_ceiling,
        "RAM usage {} exceeded 8.25 MB",
        handle.current_ram_usage()
    );

    // 3. Jump back to near-start: 4096..65536 (Cache Hit! Forward window was preserved)
    let chunk_back = handle
        .read(4096, 65536 - 4096)
        .await
        .expect("Back read failed");
    assert_eq!(chunk_back.len(), 65536 - 4096);
    for (i, &b) in chunk_back.iter().enumerate() {
        assert_eq!(b, synthetic_byte_at(4096 + i as u64));
    }
    // Zero new HTTP requests!
    assert_eq!(
        request_counter.load(Ordering::SeqCst),
        2,
        "Forward window should have served 4096..65536 from cache without network request"
    );

    // 4. Read sequentially in 16 KB increments up to 16 MB and 20 MB
    let step_size = 16 * 1024;
    let mut current_offset: u64 = 65536;

    // Read up to 16 MB
    while current_offset + (step_size as u64) <= 16 * 1024 * 1024 {
        let chunk = handle
            .read(current_offset, step_size)
            .await
            .expect("Sequential read failed");
        assert_eq!(chunk.len(), step_size);
        current_offset += step_size as u64;

        // Verify RAM never exceeds 8.25 MB ceiling
        assert!(handle.current_ram_usage() <= max_ram_ceiling);
    }

    // At 16 MB boundary: exactly 3 HTTP Range requests made!
    // (1: 0..8MB, 2: Tail Slot, 3: 8..16MB)
    let requests_at_16mb = request_counter.load(Ordering::SeqCst);
    println!("Total HTTP Range requests at 16 MB: {}", requests_at_16mb);
    assert_eq!(
        requests_at_16mb, 3,
        "Expected exactly 3 HTTP Range requests at 16 MB"
    );

    // Read further up to 20 MB
    while current_offset + (step_size as u64) <= 20 * 1024 * 1024 {
        let chunk = handle
            .read(current_offset, step_size)
            .await
            .expect("Sequential read failed");
        assert_eq!(chunk.len(), step_size);
        current_offset += step_size as u64;

        assert!(handle.current_ram_usage() <= max_ram_ceiling);
    }

    // Total requests across entire 20 MB reading: at most 4 requests (contrasting with 1,250 without sliding window!)
    let total_requests = request_counter.load(Ordering::SeqCst);
    println!("Total HTTP Range requests across 20 MB: {}", total_requests);
    assert!(
        total_requests <= 4,
        "Expected at most 4 HTTP Range requests across 20 MB, got {}",
        total_requests
    );

    // Drop handle: RAM drops to 0
    drop(handle);
    assert_eq!(valve.current_usage(), 0);
}

/// Responder for Resumable Upload testing 256 KB chunk alignment and SHA-256 match.
struct ResumableUploadMockResponder {
    intermediate_chunks: Arc<Mutex<Vec<Bytes>>>,
    final_chunk: Arc<Mutex<Option<Bytes>>>,
    intermediate_count: Arc<AtomicUsize>,
    misaligned_count: Arc<AtomicUsize>,
}

impl Respond for ResumableUploadMockResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let content_range = request
            .headers
            .get(wiremock::http::HeaderName::from_static("content-range"))
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();

        let body_bytes = Bytes::copy_from_slice(&request.body);

        if content_range.ends_with("/*") {
            // Intermediate chunk: Must be an exact multiple of 262,144 bytes (256 KB)
            if !body_bytes.len().is_multiple_of(262_144) {
                self.misaligned_count.fetch_add(1, Ordering::SeqCst);
            }
            self.intermediate_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut g) = self.intermediate_chunks.lock() {
                g.push(body_bytes);
            }

            // Google returns 308 Resume Incomplete
            ResponseTemplate::new(308).insert_header("Range", "bytes=0-0")
        } else {
            // Final chunk: Returns 200 OK with metadata
            if let Ok(mut g) = self.final_chunk.lock() {
                *g = Some(body_bytes);
            }

            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "committed_google_fid_10k",
                "name": "large_stream.bin",
                "mimeType": "application/octet-stream"
            }))
        }
    }
}

#[tokio::test]
async fn test_unaligned_write_stream_and_memory_ceiling() {
    let server = MockServer::start().await;

    let intermediate_chunks = Arc::new(Mutex::new(Vec::new()));
    let final_chunk = Arc::new(Mutex::new(None));
    let intermediate_count = Arc::new(AtomicUsize::new(0));
    let misaligned_count = Arc::new(AtomicUsize::new(0));

    let responder = ResumableUploadMockResponder {
        intermediate_chunks: Arc::clone(&intermediate_chunks),
        final_chunk: Arc::clone(&final_chunk),
        intermediate_count: Arc::clone(&intermediate_count),
        misaligned_count: Arc::clone(&misaligned_count),
    };

    Mock::given(method("PUT"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .expect("Client build failed"),
    );

    let valve = Arc::new(MemoryBackpressureValve::new());
    let mut write_handle = EphemeralWriteHandle::with_threshold(
        202,
        client,
        "acc_1".to_string(),
        format!("{}/resumable_session_10k", server.uri()),
        &valve,
        2 * 1024 * 1024, // 2 MB flush threshold
    )
    .expect("Write handle creation failed");

    // Generate 10,000 random unaligned write chunks
    let mut overall_hasher = Sha256::new();
    let mut total_written: u64 = 0;
    let chunk_sizes = [137, 4096, 65535, 137, 4096, 8192, 137, 65535];

    let mut synthetic_payload = Vec::new();

    for i in 0..10_000 {
        let sz = chunk_sizes[i % chunk_sizes.len()];
        let mut chunk = Vec::with_capacity(sz);
        for j in 0..sz {
            chunk.push(((i + j) % 255) as u8);
        }

        overall_hasher.update(&chunk);
        synthetic_payload.extend_from_slice(&chunk);

        let written = write_handle
            .write(total_written, &chunk)
            .await
            .expect("Write failed");
        assert_eq!(written, sz);
        total_written += sz as u64;

        // Verify accumulator RAM usage never exceeds bounded limit (e.g. 8 MB)
        assert!(
            write_handle.current_ram_usage() <= 8 * 1024 * 1024,
            "RAM usage exceeded 8 MB ceiling during write"
        );
    }

    let expected_sha256 = format!("{:x}", overall_hasher.finalize());

    // Commit write handle
    let commit_res = write_handle.commit().await.expect("Commit failed");

    assert_eq!(commit_res.file_id, "committed_google_fid_10k");
    assert_eq!(commit_res.total_bytes, total_written);
    assert_eq!(commit_res.sha256_hex, expected_sha256);

    // Verify RAM usage dropped to 0!
    assert_eq!(write_handle.current_ram_usage(), 0);
    assert_eq!(valve.current_usage(), 0);

    // Verify intermediate PUT checks
    let intermediate_puts = intermediate_count.load(Ordering::SeqCst);
    let misaligned_puts = misaligned_count.load(Ordering::SeqCst);

    println!("Total intermediate PUTs sent: {}", intermediate_puts);
    assert!(intermediate_puts > 0, "Must have sent intermediate PUTs");
    assert_eq!(
        misaligned_puts, 0,
        "All intermediate PUTs must be exact multiples of 262,144 bytes (256 KB)"
    );

    // Verify reassembled payload matches 100% byte-for-byte SHA-256
    let mut reassembled = Vec::new();
    if let Ok(g) = intermediate_chunks.lock() {
        for c in g.iter() {
            reassembled.extend_from_slice(c);
        }
    }
    if let Ok(g) = final_chunk.lock()
        && let Some(ref c) = *g
    {
        reassembled.extend_from_slice(c);
    }

    assert_eq!(reassembled.len(), synthetic_payload.len());
    let mut reassembled_hasher = Sha256::new();
    reassembled_hasher.update(&reassembled);
    let reassembled_sha256 = format!("{:x}", reassembled_hasher.finalize());
    assert_eq!(reassembled_sha256, expected_sha256);
}

#[tokio::test]
async fn test_out_of_order_write_reordering_and_backward_rejection() {
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(308).insert_header("Range", "bytes=0-0"))
        .mount(&server)
        .await;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .expect("Client build failed"),
    );

    let valve = Arc::new(MemoryBackpressureValve::new());
    let mut handle = EphemeralWriteHandle::with_threshold(
        303,
        client,
        "acc_1".to_string(),
        format!("{}/session_ooo", server.uri()),
        &valve,
        256 * 1024, // 256 KB flush threshold
    )
    .expect("Creation failed");

    // 1. Write out-of-order within staging buffer: write offset 4096, then offset 0
    handle.write(4096, &[0xAA; 4096]).await.expect("Write 4096 failed");
    handle.write(0, &[0xBB; 4096]).await.expect("Write 0 failed");

    // Verify accumulator contents in RAM
    assert_eq!(handle.current_ram_usage(), 8192);

    // 2. Write additional bytes to reach 256 KB, triggering automatic aligned flush
    handle
        .write(8192, &[0xDD; 262_144 - 8192])
        .await
        .expect("Write to flush failed");

    assert_eq!(handle.flushed_bytes(), 262_144);
    assert_eq!(handle.current_ram_usage(), 0);

    // 3. Now attempt backward seek past flushed boundary (offset 0 < flushed_bytes 262,144)
    let res = handle.write(0, &[0xCC; 100]).await;
    match res {
        Err(VfsError::InvalidSeekOrOutOfOrderWrite { expected, actual }) => {
            assert_eq!(expected, 262_144);
            assert_eq!(actual, 0);
        }
        _ => panic!("Expected InvalidSeekOrOutOfOrderWrite on backward seek"),
    }
}

#[tokio::test]
async fn test_memory_backpressure_window_shrinking() {
    // 10 MB ceiling
    let valve = Arc::new(MemoryBackpressureValve::with_ceiling(10 * 1024 * 1024));

    // Handle 1 allocates 8 MB + 256 KB = 8.25 MB
    let res1 = valve
        .acquire_or_shrink(8 * 1024 * 1024 + 256 * 1024, 2 * 1024 * 1024)
        .expect("res1 should allocate full 8.25 MB");
    assert_eq!(res1.allocated_bytes(), 8 * 1024 * 1024 + 256 * 1024);

    // Handle 2 requests 8.25 MB, but only ~1.75 MB is left.
    // However, min requested is 2 MB, which is > 1.75 MB -> Should fail
    assert!(valve
        .acquire_or_shrink(8 * 1024 * 1024, 2 * 1024 * 1024)
        .is_none());

    // Drop handle 1: RAM freed back to 0
    drop(res1);
    assert_eq!(valve.current_usage(), 0);

    // Now handle 2 can allocate full amount
    let res2 = valve
        .acquire_or_shrink(8 * 1024 * 1024, 2 * 1024 * 1024)
        .expect("res2 should allocate");
    assert_eq!(res2.allocated_bytes(), 8 * 1024 * 1024);
}
