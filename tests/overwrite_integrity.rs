//! In-place overwrite integrity: the original file must survive a failed overwrite, and its cloud
//! copy must be deleted only after the replacement has been committed and routed.
use std::sync::Arc;
use std::time::{Duration, Instant};

use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::routing::inode::{NewFileParams, StorageLocation};
use ucloud_fs::routing::placement::PlacementEngine;
use ucloud_fs::routing::sync::DeltaSyncEngine;
use ucloud_fs::routing::InMemoryRoutingTable;
use ucloud_fs::security::session::SessionController;
use ucloud_fs::stream::backpressure::MemoryBackpressureValve;
use ucloud_fs::vfs::driver::{DriverDependencies, UcloudFsDriver};
use ucloud_fs::vfs::notify::{MockShellNotifier, ShellNotifier};
use ucloud_fs::vfs::prompt::{AutoAcceptPrompter, ChunkPrompter};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MB: u64 = 1024 * 1024;

struct Harness {
    server: MockServer,
    routing_table: Arc<InMemoryRoutingTable>,
    driver: Arc<UcloudFsDriver>,
}

/// Builds a driver against a WireMock Drive. `chunk_put_status` is the status every resumable
/// upload PUT answers with (200 = success, 500 = simulated upload failure).
async fn harness(accounts: &[(&str, u64, u64)], chunk_put_status: u16) -> Harness {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Location", format!("{}/session_1", server.uri()).as_str()),
        )
        .mount(&server)
        .await;
    let put = if chunk_put_status == 200 {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "new_file_id",
            "name": "new_file_id",
            "mimeType": "application/octet-stream",
            "size": "2000"
        }))
    } else {
        ResponseTemplate::new(chunk_put_status)
    };
    Mock::given(method("PUT")).respond_with(put).mount(&server).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
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
        .expect("build client"),
    );
    let routing_table = Arc::new(InMemoryRoutingTable::new());
    let placement = Arc::new(PlacementEngine::new());
    for (id, limit, used) in accounts {
        placement.register_account(id, *limit, *used);
    }
    let session = Arc::new(SessionController::new());
    let notifier = Arc::new(MockShellNotifier::new());
    let sync_engine = Arc::new(DeltaSyncEngine::new(
        Arc::clone(&client),
        Arc::clone(&routing_table),
        Arc::clone(&placement),
        Arc::clone(&session),
        Arc::clone(&notifier) as Arc<dyn ShellNotifier>,
    ));
    let driver = Arc::new(UcloudFsDriver::new(DriverDependencies {
        routing_table: Arc::clone(&routing_table),
        placement_engine: Arc::clone(&placement),
        session,
        client,
        sync_engine,
        notifier: notifier as Arc<dyn ShellNotifier>,
        prompter: Arc::new(AutoAcceptPrompter) as Arc<dyn ChunkPrompter>,
        valve: Arc::new(MemoryBackpressureValve::new()),
        rt_handle: tokio::runtime::Handle::current(),
        volume_label: Some("ucloud".to_string()),
    }));
    for (id, _, _) in accounts {
        driver.set_account_token(id, &format!("tok_{id}"));
    }
    Harness {
        server,
        routing_table,
        driver,
    }
}

fn seed_old_file(h: &Harness, name: &str, account: &str, file_id: &str, size: u64) {
    h.routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name,
            size,
            mime_type: "application/octet-stream",
            modified_time: 1,
            created_time: 1,
            location: StorageLocation::SingleAccount {
                account_id: account.to_string(),
                google_file_id: file_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("seed old file");
}

async fn delete_requests_for(h: &Harness, file_id: &str) -> usize {
    h.server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "DELETE" && r.url.path().ends_with(file_id))
        .count()
}

fn write_all(driver: &UcloudFsDriver, fh: u64, data: &[u8]) {
    let mut offset = 0usize;
    while offset < data.len() {
        let end = (offset + 256 * 1024).min(data.len());
        let n = driver
            .write(fh, offset as u64, &data[offset..end])
            .expect("write");
        offset += n;
    }
}

/// A striped overwrite whose upload fails must leave the original file routed and undeleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_striped_overwrite_keeps_original_file() {
    // 40 MB accounts with a 16 MB margin: 24 MB allocatable each, so a 30 MB file must stripe.
    let h = harness(&[("acc_1", 40 * MB, 0), ("acc_2", 40 * MB, 0)], 500).await;
    seed_old_file(&h, "movie.bin", "acc_1", "old_file_id", 1000);
    let size = 30 * MB;
    let fh = h.driver.create("/movie.bin").expect("create");
    h.driver.set_file_size(fh, size).expect("set_file_size");
    write_all(&h.driver, fh, &vec![7u8; size as usize]);
    let result = h.driver.cleanup(fh);
    assert!(result.is_err(), "striped upload must fail when chunk PUTs return 500");
    let ino = h
        .routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "movie.bin")
        .expect("original file must still be routed after a failed overwrite");
    let node = h.routing_table.get_inode(ino).expect("inode");
    match node.location {
        StorageLocation::SingleAccount { account_id, google_file_id } => {
            assert_eq!(account_id, "acc_1");
            assert_eq!(google_file_id, "old_file_id");
        }
        other => panic!("original location must be restored, got {other:?}"),
    }
    assert_eq!(node.size, 1000, "original size must be preserved");
    // Give any (incorrectly) spawned cloud delete time to fire, then prove none did.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        delete_requests_for(&h, "old_file_id").await,
        0,
        "original must not be deleted from Drive when the overwrite failed"
    );
}

/// A successful single-account overwrite replaces the entry (no false `(abbr)` rename) and
/// deletes the old cloud copy exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_overwrite_replaces_entry_and_deletes_old_cloud_copy() {
    let h = harness(&[("acc_1", 100 * MB, 20 * MB)], 200).await;
    seed_old_file(&h, "report.txt", "acc_1", "old_file_id", 500);
    let data = vec![42u8; 2000];
    let fh = h.driver.create("/report.txt").expect("create");
    h.driver.set_file_size(fh, 2000).expect("set_file_size");
    write_all(&h.driver, fh, &data);
    h.driver.cleanup(fh).expect("cleanup commit");
    let ino = h
        .routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "report.txt")
        .expect("overwritten file must keep its name");
    let node = h.routing_table.get_inode(ino).expect("inode");
    match node.location {
        StorageLocation::SingleAccount { google_file_id, .. } => {
            assert_eq!(google_file_id, "new_file_id");
        }
        other => panic!("unexpected location {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while delete_requests_for(&h, "old_file_id").await == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        delete_requests_for(&h, "old_file_id").await,
        1,
        "old cloud copy must be deleted exactly once after a successful overwrite"
    );
}
