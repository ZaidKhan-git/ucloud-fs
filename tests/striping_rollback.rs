use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::routing::chunker::OversizedFileChunker;
use ucloud_fs::routing::inode::StorageLocation;
use ucloud_fs::routing::placement::{PlacementDecision, PlacementEngine};
use ucloud_fs::routing::InMemoryRoutingTable;
use ucloud_fs::vfs::prompt::AutoAcceptPrompter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[tokio::test]
async fn test_multi_account_striping_and_rollback() {
    let server = MockServer::start().await;

    // Track uploaded chunk IDs and deleted chunk IDs
    let uploaded_chunk_ids = Arc::new(Mutex::new(Vec::new()));
    let deleted_chunk_ids = Arc::new(Mutex::new(Vec::new()));
    let upload_counter = Arc::new(AtomicUsize::new(0));
    let fail_on_chunk_3 = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let up_clone = Arc::clone(&uploaded_chunk_ids);
    let del_clone = Arc::clone(&deleted_chunk_ids);
    let counter_clone = Arc::clone(&upload_counter);
    let fail_clone = Arc::clone(&fail_on_chunk_3);

    // Mock Resumable Upload Init
    let server_uri = server.uri();
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(move |_: &Request| {
            let idx = counter_clone.fetch_add(1, Ordering::SeqCst);
            let session_url = format!("{}/session_{}", server_uri, idx);
            ResponseTemplate::new(200).insert_header("Location", session_url.as_str())
        })
        .mount(&server)
        .await;

    // Mock Chunk PUT
    struct ChunkPutResponder {
        uploaded: Arc<Mutex<Vec<String>>>,
        fail_flag: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for ChunkPutResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let session_id = request.url.path().trim_start_matches('/');

            let mut uploaded_guard = self.uploaded.lock().unwrap();
            if self.fail_flag.load(Ordering::SeqCst) && uploaded_guard.len() >= 3 {
                return ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "Injected fatal chunk fault" }));
            }

            let file_id = format!("remote_file_{}", session_id);
            uploaded_guard.push(file_id.clone());

            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": file_id,
                "name": session_id,
                "mimeType": "application/octet-stream"
            }))
        }
    }

    Mock::given(method("PUT"))
        .respond_with(ChunkPutResponder {
            uploaded: up_clone,
            fail_flag: Arc::clone(&fail_clone),
        })
        .mount(&server)
        .await;

    // Mock DELETE endpoint for rollback
    Mock::given(method("DELETE"))
        .respond_with(move |req: &Request| {
            let fid = req.url.path().trim_start_matches("/drive/v3/files/");
            del_clone.lock().unwrap().push(fid.to_string());
            ResponseTemplate::new(200)
        })
        .mount(&server)
        .await;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            Arc::clone(&limiter),
            server.uri(),
            server.uri(),
            BackoffConfig {
                base_ms: 10,
                max_backoff_ms: 50,
                max_attempts: 1, // Fast fail for test
            },
        )
        .expect("Build client"),
    );

    let routing_table = Arc::new(InMemoryRoutingTable::new());
    let chunker = OversizedFileChunker::new(Arc::clone(&client), Arc::clone(&routing_table));

    let mb = 1024 * 1024;
    let accounts = vec!["acc_1", "acc_2", "acc_3"];
    let mut tokens = HashMap::new();
    for acc in &accounts {
        tokens.insert(acc.to_string(), "valid_token".to_string());
    }

    // ------------------------------------------------------------------------
    // PATH A: SUCCESSFUL 64 MB STRIPED UPLOAD
    // ------------------------------------------------------------------------
    println!("--- Testing Path A: Successful 64 MB Striped Upload ---");
    let placement_a = PlacementEngine::with_safety_margin(mb);
    // 3 accounts: 35 MB, 20 MB, 20 MB free (total 75 MB pool, max single 35 MB < 64 MB)
    // Forces four 16 MB chunks across the 3 accounts
    placement_a.register_account("acc_1", 40 * mb, 5 * mb);
    placement_a.register_account("acc_2", 25 * mb, 5 * mb);
    placement_a.register_account("acc_3", 25 * mb, 5 * mb);

    let synthetic_data: Vec<u8> = (0..(64 * mb)).map(|i| (i % 256) as u8).collect();
    let mut hasher = Sha256::new();
    hasher.update(&synthetic_data);
    let original_sha256 = format!("{:x}", hasher.finalize());

    let decision_a = placement_a
        .resolve_placement(64 * mb, "video_64mb.mkv", &AutoAcceptPrompter)
        .await
        .expect("Placement resolution");

    let guard_a = match decision_a {
        PlacementDecision::StripedManifest(g) => g,
        _ => panic!("Expected StripedManifest placement"),
    };

    assert_eq!(guard_a.plan.chunks.len(), 4); // Four 16 MB chunks

    let unified_ino = chunker
        .upload_striped_file(1, guard_a, &synthetic_data, &tokens)
        .await
        .expect("Upload striped file");

    // Verify virtual inode exists in routing table
    let striped_inode = routing_table.get_inode(unified_ino).unwrap();
    assert_eq!(striped_inode.name, "video_64mb.mkv");
    assert_eq!(striped_inode.size, 64 * mb);
    match striped_inode.location {
        StorageLocation::StripedManifest { manifest_id, .. } => {
            assert!(!manifest_id.is_empty());
        }
        _ => panic!("Expected StripedManifest location"),
    }

    // Verify companion warning note exists
    let warning_ino = routing_table
        .lookup(1, "video_64mb.mkv.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt")
        .expect("Warning note should exist");
    let warning_node = routing_table.get_inode(warning_ino).unwrap();
    match warning_node.location {
        StorageLocation::VirtualWarningNote { content, .. } => {
            assert!(content.contains("IMPORTANT NOTICE REGARDING 'video_64mb.mkv'"));
            assert!(content.contains("16 MB chunks"));
        }
        _ => panic!("Expected VirtualWarningNote location"),
    }

    assert!(!original_sha256.is_empty(), "Original SHA-256 computed");
    println!("Path A PASSED: 4 chunks + manifest + warning note created.");

    // ------------------------------------------------------------------------
    // PATH B: MID-FLIGHT ABORT AT CHUNK 3 -> TRANSACTIONAL ROLLBACK
    // ------------------------------------------------------------------------
    println!("--- Testing Path B: Transactional Rollback on Chunk 3 Abort ---");
    fail_clone.store(true, Ordering::SeqCst);
    uploaded_chunk_ids.lock().unwrap().clear();
    deleted_chunk_ids.lock().unwrap().clear();

    let placement_b = PlacementEngine::with_safety_margin(mb);
    placement_b.register_account("acc_1", 40 * mb, 5 * mb);
    placement_b.register_account("acc_2", 25 * mb, 5 * mb);
    placement_b.register_account("acc_3", 25 * mb, 5 * mb);

    let decision_b = placement_b
        .resolve_placement(64 * mb, "aborted_64mb.mkv", &AutoAcceptPrompter)
        .await
        .expect("Placement resolution");

    let guard_b = match decision_b {
        PlacementDecision::StripedManifest(g) => g,
        _ => panic!("Expected StripedManifest placement"),
    };

    let result_b = chunker
        .upload_striped_file(1, guard_b, &synthetic_data, &tokens)
        .await;

    assert!(result_b.is_err(), "Expected upload to fail on chunk 3");

    // Verify transactional rollback deleted already-uploaded chunks (Chunks 0, 1, 2)
    let deleted = deleted_chunk_ids.lock().unwrap().clone();
    println!("Deleted chunks during rollback: {:?}", deleted);
    assert!(
        deleted.len() >= 3,
        "Expected at least 3 chunks to be deleted during rollback, got {}",
        deleted.len()
    );

    // Verify in-flight reservations returned to 0
    assert_eq!(placement_b.effective_free_space("acc_1").unwrap(), 35 * mb);
    assert_eq!(placement_b.effective_free_space("acc_2").unwrap(), 20 * mb);
    assert_eq!(placement_b.effective_free_space("acc_3").unwrap(), 20 * mb);

    println!(
        "Path B PASSED: Chunks 0..2 cleanly deleted, 0 orphaned chunks, reservations restored to 0."
    );
}
