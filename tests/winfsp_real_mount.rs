use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
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

/// In-memory cloud storage mock for WireMock test cluster.
struct MockCloudStorage {
    files: Mutex<HashMap<String, Vec<u8>>>,
    sessions: Mutex<HashMap<String, Vec<u8>>>,
    next_file_id: AtomicU64,
}

impl MockCloudStorage {
    fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next_file_id: AtomicU64::new(5000),
        }
    }
}

struct ResumableInitiateResponder {
    server_base_url: String,
    storage: Arc<MockCloudStorage>,
}

impl Respond for ResumableInitiateResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let session_id = format!(
            "sess_{}",
            self.storage.next_file_id.fetch_add(1, Ordering::SeqCst)
        );
        {
            let mut sessions = self.storage.sessions.lock().unwrap();
            sessions.insert(session_id.clone(), Vec::new());
        }
        let session_url = format!("{}/session_{}", self.server_base_url, session_id);
        ResponseTemplate::new(200).insert_header("Location", session_url.as_str())
    }
}

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
            let file_id = format!(
                "cloud_file_{}_{}",
                session_key,
                self.storage.next_file_id.fetch_add(1, Ordering::SeqCst)
            );
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

struct GeneralGetResponder {
    storage: Arc<MockCloudStorage>,
}

impl Respond for GeneralGetResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();

        if path == "/drive/v3/about" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "storageQuota": {
                    "limit": "107374182400",
                    "usage": "10737418240"
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

async fn setup_mock_server(server: &MockServer, storage: Arc<MockCloudStorage>) {
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(ResumableInitiateResponder {
            server_base_url: server.uri(),
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    Mock::given(method("PUT"))
        .respond_with(ResumableChunkPutResponder {
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .respond_with(GeneralGetResponder {
            storage: Arc::clone(&storage),
        })
        .mount(server)
        .await;

    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// Dynamically locates an unused drive letter scanning downwards from Y: to E:
/// (Strictly avoiding C: and Z: repository workspace).
fn find_available_drive_letter() -> String {
    for ch in ('E'..='Y').rev() {
        if ch == 'Z' || ch == 'C' {
            continue;
        }
        let root = format!("{ch}:\\");
        if !std::path::Path::new(&root).exists() {
            return format!("{ch}:");
        }
    }
    panic!("No free drive letter available between E: and Y:");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_winfsp_real_kernel_mount() {
    let server = MockServer::start().await;
    let storage = Arc::new(MockCloudStorage::new());
    setup_mock_server(&server, Arc::clone(&storage)).await;

    let limiter = Arc::new(MultiTierLimiter::with_config(
        100_000,
        200_000,
        24,
        Duration::from_secs(25),
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
    let placement = Arc::new(PlacementEngine::new());
    placement.register_account("acc_primary", 100 * 1024 * 1024 * 1024, 10 * 1024 * 1024 * 1024);

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
    driver.set_account_token("acc_primary", "tok_valid_live");

    // Scan for free drive letter (e.g. Y:, X:, W:)
    let mount_letter = find_available_drive_letter();
    println!("Selected target drive letter: {mount_letter}");

    let mount_options = WinFspMountOptions {
        mount_point: mount_letter.clone(),
        volume_label: "ucloud-fs".to_string(),
        ..WinFspMountOptions::default()
    };

    // 1. Mount WinFsp Kernel Driver
    let mount_guard = WinFspMountGuard::mount(Arc::clone(&driver), mount_options)
        .expect("Mount native WinFsp driver");

    assert!(mount_guard.is_active(), "Mount must be active");
    assert!(driver.is_mounted(), "Driver must report mounted");

    let drive_root = format!("{mount_letter}\\");
    println!("Mounted virtual drive at {drive_root}");

    // Give Windows Shell 100ms to register drive arrival
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Genuine Windows Kernel std::fs::read_dir verification
    let root_entries: Vec<_> = std::fs::read_dir(&drive_root)
        .expect("std::fs::read_dir on drive root")
        .filter_map(|e| e.ok())
        .collect();
    println!("Root directory read via kernel driver; entries: {}", root_entries.len());

    // 3. Genuine Windows Kernel std::fs::write verification
    let hello_path = format!("{drive_root}hello.txt");
    let test_data = b"real Windows 11 WinFsp kernel driver test data payload";
    std::fs::write(&hello_path, test_data)
        .expect("std::fs::write through WinFsp kernel driver");
    println!("Wrote hello.txt through kernel driver");

    // 4. Genuine Windows Kernel std::fs::read verification
    let read_back = std::fs::read(&hello_path)
        .expect("std::fs::read through WinFsp kernel driver");
    assert_eq!(read_back, test_data, "Read payload must match written data byte-for-byte");
    println!("Read hello.txt back with 100% byte integrity");

    // 5. Genuine Windows Kernel std::fs::create_dir verification
    let test_dir_path = format!("{drive_root}testdir");
    std::fs::create_dir(&test_dir_path)
        .expect("std::fs::create_dir through WinFsp kernel driver");
    println!("Created directory testdir through kernel driver");

    // 6. Nested file write and read inside created directory
    let nested_file_path = format!("{test_dir_path}\\nested.txt");
    let nested_data = b"nested file content inside newly created folder";
    std::fs::write(&nested_file_path, nested_data)
        .expect("std::fs::write into nested folder through WinFsp kernel driver");
    let read_nested = std::fs::read(&nested_file_path)
        .expect("std::fs::read from nested folder through WinFsp kernel driver");
    assert_eq!(read_nested, nested_data, "Nested file data must match byte-for-byte");
    println!("Verified nested directory file read/write");

    // 7. Cleanup operations via Windows Kernel std::fs calls
    std::fs::remove_file(&nested_file_path)
        .expect("std::fs::remove_file nested.txt through WinFsp kernel driver");
    std::fs::remove_dir(&test_dir_path)
        .expect("std::fs::remove_dir testdir through WinFsp kernel driver");
    std::fs::remove_file(&hello_path)
        .expect("std::fs::remove_file hello.txt through WinFsp kernel driver");
    println!("Cleaned up files and directories via kernel filesystem calls");

    // 8. Clean Unmount verification
    mount_guard.unmount().expect("Unmount WinFsp driver");
    assert!(!mount_guard.is_active(), "Mount guard must report inactive");
    assert!(!driver.is_mounted(), "Driver must report unmounted");

    // Allow Windows Shell 150ms to deregister drive
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Verify drive no longer exists
    assert!(
        !std::path::Path::new(&drive_root).exists(),
        "Drive {drive_root} must no longer exist after unmount"
    );

    println!("Stage 8B Real-Mount Kernel Integration Test PASSED with 100% SUCCESS!");
}
