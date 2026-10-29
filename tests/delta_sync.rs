use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ucloud_fs::error::VfsError;
use ucloud_fs::network::client::{
    BackoffConfig, GoogleChangeItem, GoogleChangesList, GoogleDriveClient, GoogleFileMetadata,
};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::routing::inode::{NewFileParams, StorageLocation};
use ucloud_fs::routing::placement::PlacementEngine;
use ucloud_fs::routing::sync::DeltaSyncEngine;
use ucloud_fs::routing::InMemoryRoutingTable;
use ucloud_fs::security::session::SessionController;
use ucloud_fs::vfs::notify::MockShellNotifier;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper to set up a test environment with a mock server.
async fn setup_test_env(server: &MockServer) -> (
    Arc<GoogleDriveClient>,
    Arc<InMemoryRoutingTable>,
    Arc<PlacementEngine>,
    Arc<SessionController>,
    Arc<MockShellNotifier>,
    Arc<DeltaSyncEngine>,
) {
    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .expect("Failed to build GoogleDriveClient"),
    );
    let routing_table = Arc::new(InMemoryRoutingTable::new());

    let placement = Arc::new(PlacementEngine::new());
    placement.register_account("acc_1", 100 * 1024 * 1024, 10 * 1024 * 1024);
    placement.register_account("acc_2", 100 * 1024 * 1024, 20 * 1024 * 1024);
    placement.register_account("acc_3", 100 * 1024 * 1024, 30 * 1024 * 1024);

    let session = Arc::new(SessionController::new());
    let notifier = Arc::new(MockShellNotifier::new());
    let sync_engine = Arc::new(DeltaSyncEngine::new(
        Arc::clone(&client),
        Arc::clone(&routing_table),
        Arc::clone(&placement),
        Arc::clone(&session),
        Arc::clone(&notifier) as Arc<dyn ucloud_fs::vfs::notify::ShellNotifier>,
    ));

    (client, routing_table, placement, session, notifier, sync_engine)
}

#[tokio::test]
async fn test_web_ui_deletion_sync() {
    let server = MockServer::start().await;

    // Mock about.get for live quota updates
    Mock::given(method("GET"))
        .and(path("/drive/v3/about"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "storageQuota": {
                "limit": "104857600",
                "usage": "15000000"
            }
        })))
        .mount(&server)
        .await;

    let (_client, routing_table, _placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    // 1. Populate 50 files across 3 accounts
    let accounts = ["acc_1", "acc_2", "acc_3"];
    let mut file_ids = Vec::new();
    let mut inodes = Vec::new();

    for i in 1..=50 {
        let acc = accounts[(i - 1) % 3];
        let filename = format!("file_{:02}.dat", i);
        let google_id = format!("cloud_id_{:02}", i);

        let ino = routing_table
            .insert_file(NewFileParams {
                parent_ino: InMemoryRoutingTable::ROOT_INO,
                name: &filename,
                size: 1024 * i as u64,
                mime_type: "application/octet-stream",
                modified_time: 1_700_000_000,
                created_time: 1_700_000_000,
                location: StorageLocation::SingleAccount {
                    account_id: acc.to_string(),
                    google_file_id: google_id.clone(),
                },
                web_view_link: None,
            })
            .expect("Failed to insert file");

        file_ids.push((acc, google_id, filename));
        inodes.push(ino);
    }

    assert_eq!(inodes.len(), 50);

    // 2. Prepare changes.list: 10 removed: true + 5 trashed: true = 15 total deletions
    let mut deleted_file_ids = Vec::new();

    // First 10: removed = true
    for item in file_ids.iter().take(10) {
        deleted_file_ids.push((item.0, item.1.clone(), true, false));
    }
    // Next 5: trashed = true
    for item in file_ids.iter().skip(10).take(5) {
        deleted_file_ids.push((item.0, item.1.clone(), false, true));
    }

    // Apply changes per account
    let start = Instant::now();
    for acc in &accounts {
        let changes_for_acc: Vec<GoogleChangeItem> = deleted_file_ids
            .iter()
            .filter(|(a, _, _, _)| a == acc)
            .map(|(_, fid, is_removed, is_trashed)| GoogleChangeItem {
                file_id: fid.clone(),
                removed: if *is_removed { Some(true) } else { None },
                file: if *is_trashed {
                    Some(GoogleFileMetadata {
                        id: fid.clone(),
                        name: "trashed".to_string(),
                        mime_type: "application/octet-stream".to_string(),
                        size: None,
                        trashed: Some(true),
                        parents: None,
                        modified_time: None,
                        app_properties: None,
                    })
                } else {
                    None
                },
            })
            .collect();

        let changes_list = GoogleChangesList {
            changes: changes_for_acc,
            next_page_token: None,
            new_start_page_token: Some("new_token_123".to_string()),
        };

        let stats = sync_engine
            .apply_changes(acc, &changes_list, "mock_token")
            .await
            .expect("apply_changes failed");

        assert!(stats.deleted > 0);
    }
    let elapsed = start.elapsed();

    // Verify reconciliation speed is fast (under gate threshold)
    println!("Deletion reconciliation of 15 files completed in {:?}", elapsed);
    assert!(elapsed < Duration::from_millis(100));

    // 3. Verify all 15 deleted files are gone from inodes and lookup_by_name
    for i in 0..15 {
        let (_, ref gid, ref name) = file_ids[i];
        let acc = accounts[i % 3];
        assert!(
            routing_table
                .lookup(InMemoryRoutingTable::ROOT_INO, name)
                .is_none(),
            "File {} should have been removed from lookup",
            name
        );
        assert!(
            routing_table.get_inode(inodes[i]).is_none(),
            "Inode for {} should have been removed",
            name
        );
        assert!(
            routing_table.lookup_by_cloud_id(acc, gid).is_none(),
            "Reverse cloud index for {} should have been removed",
            gid
        );
    }

    // 4. Verify remaining 35 files are completely intact
    for i in 15..50 {
        let (_, ref gid, ref name) = file_ids[i];
        let acc = accounts[i % 3];
        assert!(
            routing_table
                .lookup(InMemoryRoutingTable::ROOT_INO, name)
                .is_some(),
            "File {} should remain intact",
            name
        );
        assert!(
            routing_table.get_inode(inodes[i]).is_some(),
            "Inode for {} should remain intact",
            name
        );
        assert!(
            routing_table.lookup_by_cloud_id(acc, gid).is_some(),
            "Reverse cloud index for {} should remain intact",
            gid
        );
    }

    // 5. Verify shell notifications
    let events = notifier.get_events();
    assert_eq!(events.len(), 15);
    for (event_type, path) in &events {
        assert_eq!(event_type, "DELETE");
        assert!(path.starts_with("/file_"));
    }
}

#[tokio::test]
async fn test_delete_while_reading_race_condition() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/about"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "storageQuota": { "usage": "20971520" }
        })))
        .mount(&server)
        .await;

    let (_client, routing_table, _placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    // 1. Create a 20 MB virtual file
    let file_size: u64 = 20 * 1024 * 1024;
    let filename = "active_stream.mp4";
    let cloud_id = "cloud_stream_video_1";

    let ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: filename,
            size: file_size,
            mime_type: "video/mp4",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: "acc_1".to_string(),
                google_file_id: cloud_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("Insert failed");

    // 2. Open handle simulation
    let is_unlinked = sync_engine.register_open_handle(ino);
    assert!(!is_unlinked.load(Ordering::SeqCst));

    // Simulate reading 1 MB chunks up to 8 MB
    let mut current_offset: u64 = 0;
    let chunk_size: u64 = 1024 * 1024;

    while current_offset < 8 * 1024 * 1024 {
        // Reader check
        assert!(
            !is_unlinked.load(Ordering::SeqCst),
            "Handle should not be unlinked before remote deletion"
        );
        let node = routing_table.get_inode(ino);
        assert!(node.is_some(), "Inode must be present during read");
        current_offset += chunk_size;
    }

    assert_eq!(current_offset, 8 * 1024 * 1024);

    // 3. Mid-stream at 8 MB: Fire remote deletion
    let deletion_changes = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: cloud_id.to_string(),
            removed: Some(true),
            file: None,
        }],
        next_page_token: None,
        new_start_page_token: Some("token_after_delete".to_string()),
    };

    let stats = sync_engine
        .apply_changes("acc_1", &deletion_changes, "token")
        .await
        .expect("Deletion apply failed");
    assert_eq!(stats.deleted, 1);

    // 4. Next read attempt at 8 MB
    // Check reader status: is_unlinked must be true and routing table must not have the inode
    let read_result: Result<(), VfsError> = if is_unlinked.load(Ordering::SeqCst)
        || routing_table.get_inode(ino).is_none()
    {
        Err(VfsError::NotFound("File deleted remotely during read".to_string()))
    } else {
        Ok(())
    };

    match read_result {
        Err(VfsError::NotFound(msg)) => {
            assert!(msg.contains("File deleted remotely"));
        }
        _ => panic!("Expected NotFound error on read after unlinking"),
    }

    // Verify lookup is None
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, filename)
        .is_none());

    // 5. Close handle
    sync_engine.unregister_open_handle(ino);

    // Verify shell notification was dispatched
    let events = notifier.get_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], ("DELETE".to_string(), format!("/{}", filename)));
}

#[tokio::test]
async fn test_self_echo_suppression() {
    let server = MockServer::start().await;

    let (_client, routing_table, _placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let filename = "locally_created.pdf";
    let cloud_id = "local_upload_fid_123";

    let ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: filename,
            size: 5000,
            mime_type: "application/pdf",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: "acc_1".to_string(),
                google_file_id: cloud_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("Insert failed");

    // Record local mutation
    sync_engine.record_local_mutation("acc_1", cloud_id);

    // Inject changes.list with the same file ID
    let echo_change = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: cloud_id.to_string(),
            removed: Some(true),
            file: None,
        }],
        next_page_token: None,
        new_start_page_token: Some("token_echo".to_string()),
    };

    let stats = sync_engine
        .apply_changes("acc_1", &echo_change, "token")
        .await
        .expect("Apply failed");

    // Self-echo must be suppressed!
    assert_eq!(stats.deleted, 0);
    assert!(notifier.get_events().is_empty());
    assert!(routing_table.get_inode(ino).is_some());
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, filename)
        .is_some());
}

#[tokio::test]
async fn test_remote_creation_and_update() {
    let server = MockServer::start().await;

    let (_client, routing_table, _placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    // 1. Remote addition of a new file from drive.google.com
    let new_file_change = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: "remote_new_1".to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: "remote_new_1".to_string(),
                name: "notes.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("4096".to_string()),
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_new".to_string()),
    };

    let stats = sync_engine
        .apply_changes("acc_2", &new_file_change, "token")
        .await
        .expect("Create failed");

    assert_eq!(stats.created, 1);
    let ino = routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "notes.txt")
        .expect("Created file must be found in routing table");
    let node = routing_table.get_inode(ino).expect("Inode must exist");
    assert_eq!(node.name, "notes.txt");
    assert_eq!(node.size, 4096);

    let events = notifier.get_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], ("CREATE".to_string(), "notes.txt".to_string()));

    // 2. Remote update / rename from drive.google.com
    notifier.clear();
    let rename_change = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: "remote_new_1".to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: "remote_new_1".to_string(),
                name: "notes_renamed.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("4096".to_string()),
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_renamed".to_string()),
    };

    let stats = sync_engine
        .apply_changes("acc_2", &rename_change, "token")
        .await
        .expect("Rename failed");

    assert_eq!(stats.updated, 1);
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "notes.txt")
        .is_none());
    let renamed_ino = routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "notes_renamed.txt")
        .expect("Renamed file must be found");
    assert_eq!(ino, renamed_ino);

    let update_events = notifier.get_events();
    assert_eq!(update_events.len(), 1);
    assert_eq!(
        update_events[0],
        ("UPDATE".to_string(), "notes_renamed.txt".to_string())
    );
}

#[tokio::test]
async fn test_striped_file_chunk_remote_deletion() {
    let server = MockServer::start().await;

    let (_client, routing_table, _placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let filename = "striped_archive.zip";
    let manifest_fid = "manifest_fid_99";
    let chunk0_fid = "chunk0_fid";
    let chunk1_fid = "chunk1_fid";
    let warning_filename = format!("{}.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt", filename);

    // 1. Insert striped file
    let striped_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: filename,
            size: 32 * 1024 * 1024,
            mime_type: "application/zip",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::StripedManifest {
                manifest_id: "manifest-uuid-1".to_string(),
                manifest_account_id: "acc_1".to_string(),
                manifest_google_file_id: manifest_fid.to_string(),
            },
            web_view_link: None,
        })
        .expect("Striped insert failed");

    // Register chunk IDs to unified striped ino
    routing_table.register_chunk_cloud_id("acc_1", chunk0_fid, striped_ino);
    routing_table.register_chunk_cloud_id("acc_2", chunk1_fid, striped_ino);

    // Insert companion warning note
    let warning_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: &warning_filename,
            size: 256,
            mime_type: "text/plain",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::VirtualWarningNote {
                parent_striped_file_name: filename.to_string(),
                content: "Important notice...".to_string(),
            },
            web_view_link: None,
        })
        .expect("Warning insert failed");

    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, filename)
        .is_some());
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, &warning_filename)
        .is_some());

    // 2. Fire remote deletion for chunk 1 on acc_2
    let chunk_deletion = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: chunk1_fid.to_string(),
            removed: Some(true),
            file: None,
        }],
        next_page_token: None,
        new_start_page_token: Some("tok_chunk_del".to_string()),
    };

    let stats = sync_engine
        .apply_changes("acc_2", &chunk_deletion, "token")
        .await
        .expect("Chunk deletion apply failed");

    assert_eq!(stats.deleted, 1);

    // 3. Verify both striped file and companion warning note are removed!
    assert!(
        routing_table
            .lookup(InMemoryRoutingTable::ROOT_INO, filename)
            .is_none(),
        "Striped file should be removed"
    );
    assert!(
        routing_table
            .lookup(InMemoryRoutingTable::ROOT_INO, &warning_filename)
            .is_none(),
        "Companion warning note should be removed"
    );
    assert!(routing_table.get_inode(striped_ino).is_none());
    assert!(routing_table.get_inode(warning_ino).is_none());

    // Verify delete notifications were sent for both
    let events = notifier.get_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], ("DELETE".to_string(), format!("/{}", filename)));
    assert_eq!(events[1], ("DELETE".to_string(), format!("/{}", warning_filename)));
}
