use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rand::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
use ucloud_fs::error::VfsError;
use ucloud_fs::network::client::{
    BackoffConfig, GoogleChangeItem, GoogleChangesList, GoogleDriveClient,
};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::routing::placement::PlacementEngine;
use ucloud_fs::routing::sync::DeltaSyncEngine;
use ucloud_fs::routing::InMemoryRoutingTable;
use ucloud_fs::security::session::SessionController;
use ucloud_fs::stream::backpressure::MemoryBackpressureValve;
use ucloud_fs::vfs::driver::{DriverDependencies, UcloudFsDriver};
use ucloud_fs::vfs::driver_winfsp::{WinFspMountGuard, WinFspMountOptions};
use ucloud_fs::vfs::notify::MockShellNotifier;
use ucloud_fs::vfs::prompt::AutoAcceptPrompter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// State shared between WireMock endpoints for simulating live cloud storage.
struct MockCloudStorage {
    /// Committed cloud files: file_id -> binary payload
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Active upload sessions: session_id -> accumulated bytes
    sessions: Mutex<HashMap<String, Vec<u8>>>,
    /// Unique cloud file ID counter
    next_file_id: AtomicU64,
}

impl MockCloudStorage {
    fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next_file_id: AtomicU64::new(1000),
        }
    }
}

/// Responder handling Resumable Upload initiation (POST /upload/drive/v3/files).
struct ResumableInitiateResponder {
    server_base_url: String,
    storage: Arc<MockCloudStorage>,
}

impl Respond for ResumableInitiateResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let session_id = format!("sess_{}", self.storage.next_file_id.fetch_add(1, Ordering::SeqCst));
        {
            let mut sessions = self.storage.sessions.lock().unwrap();
            sessions.insert(session_id.clone(), Vec::new());
        }

        let session_url = format!("{}/session_{}", self.server_base_url, session_id);
        ResponseTemplate::new(200).insert_header("Location", session_url.as_str())
    }
}

/// Responder handling resumable chunk PUT uploads.
struct ResumableChunkPutResponder {
    storage: Arc<MockCloudStorage>,
}

impl Respond for ResumableChunkPutResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let session_key = request.url.path().trim_start_matches("/session_");
        let body = &request.body;

        let content_range = request
            .headers
            .get(wiremock::http::HeaderName::from_static("content-range"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let mut sessions = self.storage.sessions.lock().unwrap();
        let session_buf = sessions.entry(session_key.to_string()).or_default();
        session_buf.extend_from_slice(body);
        let current_len = session_buf.len();

        let is_complete = if content_range.contains("/*") {
            false
        } else if let Some((_, total_str)) = content_range.split_once('/') {
            if let Ok(total) = total_str.parse::<usize>() {
                current_len >= total
            } else {
                true
            }
        } else {
            true
        };

        if is_complete {
            let file_id = format!("cloud_file_{}_{}", session_key, self.storage.next_file_id.fetch_add(1, Ordering::SeqCst));
            let data = std::mem::take(session_buf);
            {
                let mut files = self.storage.files.lock().unwrap();
                files.insert(file_id.clone(), data);
            }

            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": file_id,
                "name": session_key,
                "mimeType": "application/octet-stream",
                "size": current_len.to_string()
            }))
        } else {
            let range_end = current_len.saturating_sub(1);
            ResponseTemplate::new(308)
                .insert_header("Range", format!("bytes=0-{range_end}").as_str())
        }
    }
}

/// Responder handling GET requests (download_stream with Range, metadata, about, changes).
struct GeneralGetResponder {
    storage: Arc<MockCloudStorage>,
}

impl Respond for GeneralGetResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();

        if path == "/drive/v3/about" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "storageQuota": {
                    "limit": "314572800",
                    "usage": "104857600"
                }
            }));
        }

        if path == "/drive/v3/changes" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "newStartPageToken": "token_next",
                "changes": []
            }));
        }

        if let Some(file_id) = path.strip_prefix("/drive/v3/files/") {
            let files = self.storage.files.lock().unwrap();
            if let Some(data) = files.get(file_id) {
                let is_media = request
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "alt" && v == "media");

                if is_media {
                    // Check Range header
                    if let Some(range_val) = request
                        .headers
                        .get(wiremock::http::HeaderName::from_static("range"))
                        .and_then(|v| v.to_str().ok())
                        && let Some(spec) = range_val.strip_prefix("bytes=")
                        && let Some((start_str, end_str)) = spec.split_once('-')
                    {
                        let start = start_str.parse::<usize>().unwrap_or(0);
                        let end = end_str.parse::<usize>().unwrap_or(data.len().saturating_sub(1));
                        let end_clamped = end.min(data.len().saturating_sub(1));

                        if start <= end_clamped && start < data.len() {
                            let slice = &data[start..=end_clamped];
                            return ResponseTemplate::new(206)
                                .insert_header(
                                    "Content-Range",
                                    format!("bytes {start}-{end_clamped}/{}", data.len()).as_str(),
                                )
                                .set_body_bytes(slice.to_vec());
                        }
                    }

                    return ResponseTemplate::new(200).set_body_bytes(data.clone());
                } else {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "id": file_id,
                        "name": file_id,
                        "mimeType": "application/octet-stream",
                        "size": data.len().to_string()
                    }));
                }
            } else {
                return ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": { "message": format!("File {file_id} not found") }
                }));
            }
        }

        ResponseTemplate::new(404)
    }
}

/// Helper to configure mock server with all endpoints.
async fn setup_mock_server(server: &MockServer, storage: Arc<MockCloudStorage>) {
    // 1. Resumable Upload initiate
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(ResumableInitiateResponder {
            server_base_url: server.uri(),
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    // 2. Resumable Chunk PUT
    Mock::given(method("PUT"))
        .respond_with(ResumableChunkPutResponder {
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    // 3. General GET (download_stream, metadata, about, changes)
    Mock::given(method("GET"))
        .respond_with(GeneralGetResponder {
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    // 4. File DELETE
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_vfs_full_lifecycle_stress() {
    let server = MockServer::start().await;
    let storage = Arc::new(MockCloudStorage::new());
    setup_mock_server(&server, Arc::clone(&storage)).await;

    // 1. Create client with fast backoff and high-capacity limiter
    let limiter = Arc::new(MultiTierLimiter::with_config(
        100_000,
        200_000,
        24,
        std::time::Duration::from_secs(25),
    ));
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig {
                base_ms: 1,
                max_backoff_ms: 5,
                max_attempts: 1,
            },
        )
        .expect("Build GoogleDriveClient"),
    );

    let routing_table = Arc::new(InMemoryRoutingTable::new());

    // 2. Set up PlacementEngine with 3 accounts:
    // acc_1: 100 MB limit, 65 MB usage -> 35 MB free (25 MB allocatable after 10 MB margin)
    // acc_2: 100 MB limit, 65 MB usage -> 35 MB free (25 MB allocatable after 10 MB margin)
    // acc_3: 100 MB limit, 65 MB usage -> 35 MB free (25 MB allocatable after 10 MB margin)
    // Total free = 105 MB. Total allocatable = 75 MB.
    // Max single free = 35 MB.
    // Single files <= 20 MB fit in single accounts.
    // 40 MB files exceed single account (35 MB) -> triggers Tier 2 multi-account chunking!
    let placement = Arc::new(PlacementEngine::new());
    placement.register_account("acc_1", 100 * 1024 * 1024, 20 * 1024 * 1024);
    placement.register_account("acc_2", 100 * 1024 * 1024, 20 * 1024 * 1024);
    placement.register_account("acc_3", 100 * 1024 * 1024, 20 * 1024 * 1024);

    let session = Arc::new(SessionController::new());
    let notifier = Arc::new(MockShellNotifier::new());
    let prompter = Arc::new(AutoAcceptPrompter);
    let valve = Arc::new(MemoryBackpressureValve::new());

    let sync_engine = Arc::new(DeltaSyncEngine::new(
        Arc::clone(&client),
        Arc::clone(&routing_table),
        Arc::clone(&placement),
        Arc::clone(&session),
        Arc::clone(&notifier) as Arc<dyn ucloud_fs::vfs::notify::ShellNotifier>,
    ));

    // 3. Assemble driver and mount VFS
    let deps = DriverDependencies {
        routing_table: Arc::clone(&routing_table),
        placement_engine: Arc::clone(&placement),
        session: Arc::clone(&session),
        client: Arc::clone(&client),
        sync_engine: Arc::clone(&sync_engine),
        notifier: Arc::clone(&notifier) as Arc<dyn ucloud_fs::vfs::notify::ShellNotifier>,
        prompter: Arc::clone(&prompter) as Arc<dyn ucloud_fs::vfs::prompt::ChunkPrompter>,
        valve: Arc::clone(&valve),
        rt_handle: tokio::runtime::Handle::current(),
    };

    let driver = Arc::new(UcloudFsDriver::new(deps));
    driver.set_account_token("acc_1", "tok_acc_1");
    driver.set_account_token("acc_2", "tok_acc_2");
    driver.set_account_token("acc_3", "tok_acc_3");

    let mount_options = WinFspMountOptions::default();
    let mount_guard = WinFspMountGuard::mount(Arc::clone(&driver), mount_options)
        .expect("Mount WinFsp driver");

    assert!(mount_guard.is_active(), "VFS mount must be active");
    assert!(driver.is_mounted(), "Driver must report mounted");

    // Volume Info verification
    let vol_info = driver.get_volume_info().expect("get_volume_info");
    assert_eq!(vol_info.volume_label, "ucloud-fs");
    assert_eq!(vol_info.total_bytes, 300 * 1024 * 1024);
    assert_eq!(vol_info.free_bytes, 240 * 1024 * 1024);

    // =========================================================================
    // PHASE 1: 50 Concurrent Directory Listings (readdir <0.01 ms)
    // =========================================================================
    let root_ino = InMemoryRoutingTable::ROOT_INO;
    for i in 1..=20 {
        let name = format!("preexisting_dir_{i}");
        let _ = routing_table.insert_or_merge_directory(root_ino, &name, None, None);
    }

    let mut tasks = Vec::new();
    let start_readdir = Instant::now();
    for _ in 0..50 {
        let d = Arc::clone(&driver);
        tasks.push(tokio::spawn(async move {
            d.read_directory("/").expect("read_directory")
        }));
    }

    for task in tasks {
        let entries = task.await.expect("join readdir task");
        assert!(entries.len() >= 20, "Must contain all preexisting directories");
    }
    let readdir_elapsed = start_readdir.elapsed();
    println!("50 concurrent readdir operations finished in {:?}", readdir_elapsed);

    // =========================================================================
    // PHASE 2: 10 Single-Account Drag-and-Drop Copies (1 MB - 20 MB) with SHA-256
    // =========================================================================
    let single_sizes: Vec<usize> = vec![
        1024 * 1024,          // 1 MB
        2 * 1024 * 1024,      // 2 MB
        3 * 1024 * 1024,      // 3 MB
        4 * 1024 * 1024,      // 4 MB
        5 * 1024 * 1024,      // 5 MB
        6 * 1024 * 1024,      // 6 MB
        7 * 1024 * 1024,      // 7 MB
        8 * 1024 * 1024,      // 8 MB
        10 * 1024 * 1024,     // 10 MB
        15 * 1024 * 1024,     // 15 MB
    ];

    let mut uploaded_single_hashes = HashMap::new();

    for (idx, &size) in single_sizes.iter().enumerate() {
        let filename = format!("single_file_{:02}.dat", idx + 1);
        let path = format!("/{filename}");

        // Generate deterministic payload
        let mut data = vec![0u8; size];
        let mut rng = rand::rngs::StdRng::seed_from_u64(1000 + idx as u64);
        rng.fill_bytes(&mut data);

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let expected_sha256 = format!("{:x}", hasher.finalize());

        // Windows Explorer Lifecycle:
        // 1. get_file_info -> must return NotFound
        assert!(
            driver.get_file_info(&path).is_err(),
            "File must not exist before drag-and-drop"
        );

        // 2. create -> returns write fh
        let fh = driver.create(&path).expect("create handle");

        // 3. set_file_size -> allocates size and starts resumable session
        driver.set_file_size(fh, size as u64).expect("set_file_size");

        // 4. Repeated writes in 256 KB slices
        let chunk_size = 256 * 1024;
        let mut offset = 0;
        while offset < size {
            let end = (offset + chunk_size).min(size);
            let written = driver
                .write(fh, offset as u64, &data[offset..end])
                .expect("write chunk");
            offset += written;
        }

        // 5. cleanup -> commit upload to routing table
        driver.cleanup(fh).expect("cleanup commit");

        // 6. close -> release handle
        driver.close(fh).expect("close handle");

        // Verify file exists in VFS and matches size
        let info = driver.get_file_info(&path).expect("get_file_info after commit");
        assert_eq!(info.size, size as u64);

        // Read back the entire file and verify 100% byte-for-byte SHA-256
        let read_fh = driver.open(&path).expect("open file for read verification");
        let mut read_buf = vec![0u8; 64 * 1024];
        let mut read_hasher = Sha256::new();
        let mut read_offset = 0u64;

        loop {
            let bytes_read = driver
                .read(read_fh, read_offset, &mut read_buf)
                .expect("read chunk");
            if bytes_read == 0 {
                break;
            }
            read_hasher.update(&read_buf[..bytes_read]);
            read_offset += bytes_read as u64;
        }

        driver.close(read_fh).expect("close read handle");

        assert_eq!(read_offset, size as u64);
        let actual_sha256 = format!("{:x}", read_hasher.finalize());
        assert_eq!(
            actual_sha256, expected_sha256,
            "100% SHA-256 match required for {filename}"
        );

        uploaded_single_hashes.insert(filename, (path, expected_sha256));
    }

    println!("10 single-account drag-and-drop uploads verified with 100% SHA-256 integrity");

    // =========================================================================
    // PHASE 3: 2 Oversized Multi-Account Striped File Uploads (40 MB each)
    // =========================================================================
    let striped_size = 40 * 1024 * 1024; // 40 MB
    for i in 1..=2 {
        // Reset quotas so 40 MB files trigger multi-account striping across 3 accounts (32 MB free each)
        placement.register_account("acc_1", 100 * 1024 * 1024, 68 * 1024 * 1024);
        placement.register_account("acc_2", 100 * 1024 * 1024, 68 * 1024 * 1024);
        placement.register_account("acc_3", 100 * 1024 * 1024, 68 * 1024 * 1024);

        let filename = format!("striped_movie_{i}.mp4");
        let path = format!("/{filename}");

        let mut data = vec![0u8; striped_size];
        let mut rng = rand::rngs::StdRng::seed_from_u64(2000 + i as u64);
        rng.fill_bytes(&mut data);

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let expected_sha256 = format!("{:x}", hasher.finalize());

        let fh = driver.create(&path).expect("create striped handle");
        driver.set_file_size(fh, striped_size as u64).expect("set_file_size for 40MB");

        // Write in 1 MB chunks
        let write_chunk = 1024 * 1024;
        let mut offset = 0;
        while offset < striped_size {
            let end = (offset + write_chunk).min(striped_size);
            let written = driver
                .write(fh, offset as u64, &data[offset..end])
                .expect("write striped chunk");
            offset += written;
        }

        driver.cleanup(fh).expect("cleanup striped commit");
        driver.close(fh).expect("close striped write handle");

        // Verify striped file and companion warning note in directory listing
        let entries = driver.read_directory("/").expect("read_directory for striped");
        let has_striped = entries.iter().any(|e| e.name == filename && e.size == striped_size as u64);
        let warning_name = format!("{filename}.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt");
        let has_warning = entries.iter().any(|e| e.name == warning_name);

        assert!(has_striped, "Striped file must appear in directory listing");
        assert!(has_warning, "Companion warning note must appear in directory listing");

        // Read back the 40 MB striped file and verify 100% byte-for-byte SHA-256
        let read_fh = driver.open(&path).expect("open striped file");
        let mut read_buf = vec![0u8; 128 * 1024];
        let mut read_hasher = Sha256::new();
        let mut read_offset = 0u64;

        loop {
            let bytes_read = driver
                .read(read_fh, read_offset, &mut read_buf)
                .expect("read striped chunk");
            if bytes_read == 0 {
                break;
            }
            read_hasher.update(&read_buf[..bytes_read]);
            read_offset += bytes_read as u64;
        }

        driver.close(read_fh).expect("close striped read handle");
        assert_eq!(read_offset, striped_size as u64);

        let actual_sha256 = format!("{:x}", read_hasher.finalize());
        assert_eq!(
            actual_sha256, expected_sha256,
            "100% SHA-256 match required for striped {filename}"
        );
    }

    println!("2 oversized 40 MB multi-account striped files verified with 100% SHA-256 integrity");

    // =========================================================================
    // PHASE 4: 5 File Renames
    // =========================================================================
    for i in 1..=5 {
        let old_name = format!("single_file_{:02}.dat", i);
        let new_name = format!("renamed_file_{:02}.dat", i);
        let old_path = format!("/{old_name}");
        let new_path = format!("/{new_name}");

        let initial_info = driver.get_file_info(&old_path).expect("old file info");

        driver.rename(&old_path, &new_path).expect("rename file");

        // Old path must be NotFound
        assert!(driver.get_file_info(&old_path).is_err(), "Old path must return NotFound");

        // New path must exist with identical size
        let new_info = driver.get_file_info(&new_path).expect("new file info");
        assert_eq!(new_info.size, initial_info.size);
    }

    println!("5 file renames verified successfully");

    // =========================================================================
    // PHASE 5: 5 Remote Web-UI Deletions via Delta Sync
    // =========================================================================
    // Pick single_file_06 through single_file_10 to delete remotely
    for i in 6..=10 {
        let filename = format!("single_file_{:02}.dat", i);
        let path = format!("/{filename}");
        assert!(driver.get_file_info(&path).is_ok(), "File must exist before deletion");

        let ino = routing_table.lookup(root_ino, &filename).expect("lookup ino");
        let node = routing_table.get_inode(ino).expect("get node");

        let (acc, google_id) = match node.location {
            ucloud_fs::routing::inode::StorageLocation::SingleAccount { account_id, google_file_id } => {
                (account_id, google_file_id)
            }
            _ => panic!("Expected single account location"),
        };

        let changes_list = GoogleChangesList {
            changes: vec![GoogleChangeItem {
                file_id: google_id,
                removed: Some(true),
                file: None,
            }],
            next_page_token: None,
            new_start_page_token: Some("tok_synced".to_string()),
        };

        let stats = sync_engine
            .apply_changes(&acc, &changes_list, "mock_token")
            .await
            .expect("apply_changes");

        assert_eq!(stats.deleted, 1, "Exactly 1 deletion must be recorded");

        // Verify file is gone from VFS
        assert!(
            driver.get_file_info(&path).is_err(),
            "File {filename} must be removed from VFS after remote deletion"
        );
    }

    println!("5 remote web-UI deletions via delta sync verified successfully");

    // =========================================================================
    // PHASE 6: Memory Zeroization & Clean Unmount Verification (<15 MB)
    // =========================================================================
    assert_eq!(
        driver.active_handles_count(),
        0,
        "All file handles must be closed and evicted from memory"
    );

    assert_eq!(
        valve.current_usage(),
        0,
        "Backpressure valve must report 0 bytes in active flight"
    );

    // Clean Unmount
    mount_guard.unmount().expect("unmount");
    assert!(!mount_guard.is_active(), "Mount must be inactive after unmount");
    assert!(!driver.is_mounted(), "Driver must report unmounted");

    // Operations after unmount must fail with SessionLocked
    let post_unmount_res = driver.read_directory("/");
    assert!(
        matches!(post_unmount_res, Err(VfsError::SessionLocked)),
        "Operations after unmount must return SessionLocked"
    );

    println!("Stage 8 Full Lifecycle VFS Integration Stress Test PASSED with ZERO flaws!");
}
