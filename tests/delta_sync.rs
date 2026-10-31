use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ucloud_fs::error::VfsError;
use ucloud_fs::network::client::{
    BackoffConfig, GoogleChangeItem, GoogleChangesList, GoogleDriveClient, GoogleFileMetadata,
    HydratedAccountData,
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

#[tokio::test]
async fn test_delta_sync_preserves_disambiguated_folder_and_file_names() {
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

    let (_client, routing_table, placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let acc1 = "mohdzaidk035@gmail.com";
    let acc2 = "ucloudfs@gmail.com";
    placement.register_account(acc1, 100 * 1024 * 1024, 10 * 1024 * 1024);
    placement.register_account(acc2, 100 * 1024 * 1024, 20 * 1024 * 1024);

    // 1. Hydrate two colliding folders named "JaipurTrip2024"
    let folder_id_1 = "gdrive_folder_id_zaid";
    let folder_id_2 = "gdrive_folder_id_ucloud";

    let dir1_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "JaipurTrip2024",
            Some(acc1),
            Some(folder_id_1),
        )
        .expect("Failed to insert folder 1");

    let dir2_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "JaipurTrip2024",
            Some(acc2),
            Some(folder_id_2),
        )
        .expect("Failed to insert colliding folder 2");

    assert_ne!(dir1_ino, dir2_ino);

    let dir1_node = routing_table.get_inode(dir1_ino).expect("dir1 node exists");
    let dir2_node = routing_table.get_inode(dir2_ino).expect("dir2 node exists");

    assert_eq!(dir1_node.name, "JaipurTrip2024 (mohdzaidk035)");
    assert_eq!(dir2_node.name, "JaipurTrip2024 (ucloudfs)");

    // 2. Hydrate two colliding files named "itinerary.pdf"
    let file_id_1 = "gdrive_file_id_itinerary_zaid";
    let file_id_2 = "gdrive_file_id_itinerary_ucloud";

    let file1_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: "itinerary.pdf",
            size: 1024,
            mime_type: "application/pdf",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc1.to_string(),
                google_file_id: file_id_1.to_string(),
            },
            web_view_link: None,
        })
        .expect("Failed to insert file 1");

    let file2_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: "itinerary.pdf",
            size: 2048,
            mime_type: "application/pdf",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc2.to_string(),
                google_file_id: file_id_2.to_string(),
            },
            web_view_link: None,
        })
        .expect("Failed to insert colliding file 2");

    assert_ne!(file1_ino, file2_ino);

    let file1_node = routing_table.get_inode(file1_ino).expect("file1 node exists");
    let file2_node = routing_table.get_inode(file2_ino).expect("file2 node exists");

    assert_eq!(file1_node.name, "itinerary (mohdzaidk035).pdf");
    assert_eq!(file2_node.name, "itinerary (ucloudfs).pdf");

    // 3. Trigger changes.list delta sync event for acc1:
    // - Untagged folder name: "JaipurTrip2024"
    // - Untagged file name: "itinerary.pdf" with parents: None
    // - New file "palace.jpg" inside folder_id_1
    notifier.clear();

    let changes = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: folder_id_1.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: folder_id_1.to_string(),
                    name: "JaipurTrip2024".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: file_id_1.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: file_id_1.to_string(),
                    name: "itinerary.pdf".to_string(),
                    mime_type: "application/pdf".to_string(),
                    size: Some("1024".to_string()),
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: "gdrive_file_palace".to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: "gdrive_file_palace".to_string(),
                    name: "palace.jpg".to_string(),
                    mime_type: "image/jpeg".to_string(),
                    size: Some("8192".to_string()),
                    trashed: Some(false),
                    parents: Some(vec![folder_id_1.to_string()]),
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("token_after_changes".to_string()),
    };

    let stats = sync_engine
        .apply_changes(acc1, &changes, "access_token")
        .await
        .expect("Delta sync apply_changes failed");

    assert_eq!(stats.updated, 2);
    assert_eq!(stats.created, 1);

    // 4. Assert both disambiguated folders remain in readdir(ROOT_INO) without vanishing or stripping tags
    let root_children = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .expect("readdir(ROOT_INO) failed");
    let root_names: Vec<String> = root_children.into_iter().map(|n| n.name).collect();

    assert!(
        root_names.contains(&"JaipurTrip2024 (mohdzaidk035)".to_string()),
        "JaipurTrip2024 (mohdzaidk035) must remain in root"
    );
    assert!(
        root_names.contains(&"JaipurTrip2024 (ucloudfs)".to_string()),
        "JaipurTrip2024 (ucloudfs) must remain in root"
    );
    assert!(
        root_names.contains(&"itinerary (mohdzaidk035).pdf".to_string()),
        "itinerary (mohdzaidk035).pdf must remain in root"
    );
    assert!(
        root_names.contains(&"itinerary (ucloudfs).pdf".to_string()),
        "itinerary (ucloudfs).pdf must remain in root"
    );

    // Untagged raw names must NOT be in readdir(ROOT_INO)
    assert!(
        !root_names.contains(&"JaipurTrip2024".to_string()),
        "Untagged JaipurTrip2024 must not appear in root"
    );
    assert!(
        !root_names.contains(&"itinerary.pdf".to_string()),
        "Untagged itinerary.pdf must not appear in root"
    );

    // 5. Assert lookup returns correct inodes for disambiguated names
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "JaipurTrip2024 (mohdzaidk035)"),
        Some(dir1_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "JaipurTrip2024 (ucloudfs)"),
        Some(dir2_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "itinerary (mohdzaidk035).pdf"),
        Some(file1_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "itinerary (ucloudfs).pdf"),
        Some(file2_ino)
    );

    // Assert lookup for untagged names is None
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "JaipurTrip2024")
        .is_none());
    assert!(routing_table
        .lookup(InMemoryRoutingTable::ROOT_INO, "itinerary.pdf")
        .is_none());

    // 6. Assert new file appears inside JaipurTrip2024 (mohdzaidk035)
    let dir1_children = routing_table.readdir(dir1_ino).expect("readdir(dir1_ino) failed");
    let dir1_child_names: Vec<String> = dir1_children.into_iter().map(|n| n.name).collect();
    assert_eq!(dir1_child_names, vec!["palace.jpg".to_string()]);

    // Assert new file does NOT appear in sibling folder
    let dir2_children = routing_table.readdir(dir2_ino).expect("readdir(dir2_ino) failed");
    assert!(dir2_children.is_empty());

    // 7. Assert shell notifications
    let events = notifier.get_events();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        ("UPDATE".to_string(), "JaipurTrip2024 (mohdzaidk035)".to_string())
    );
    assert_eq!(
        events[1],
        ("UPDATE".to_string(), "itinerary (mohdzaidk035).pdf".to_string())
    );
    assert_eq!(
        events[2],
        (
            "CREATE".to_string(),
            "/JaipurTrip2024 (mohdzaidk035)/palace.jpg".to_string()
        )
    );
}


#[tokio::test]
async fn test_adversarial_nested_parent_preservation_and_shell_notification_paths() {
    let server = MockServer::start().await;

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

    let (_client, routing_table, placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let acc = "adv_user@gmail.com";
    placement.register_account(acc, 100 * 1024 * 1024, 10 * 1024 * 1024);

    // 1. Build hierarchy: ROOT -> ParentFolder -> ChildFolder -> nested_file.txt
    let parent_fid = "gdrive_parent_fid";
    let child_fid = "gdrive_child_fid";
    let file_fid = "gdrive_nested_file_fid";

    let parent_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "ParentFolder",
            Some(acc),
            Some(parent_fid),
        )
        .expect("ParentFolder insert failed");

    let child_ino = routing_table
        .insert_or_merge_directory(
            parent_ino,
            "ChildFolder",
            Some(acc),
            Some(child_fid),
        )
        .expect("ChildFolder insert failed");

    let file_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: child_ino,
            name: "nested_file.txt",
            size: 4096,
            mime_type: "text/plain",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc.to_string(),
                google_file_id: file_fid.to_string(),
            },
            web_view_link: None,
        })
        .expect("nested_file insert failed");

    // Verify initial layout
    assert_eq!(routing_table.get_inode(child_ino).unwrap().parent_ino, parent_ino);
    assert_eq!(routing_table.get_inode(file_ino).unwrap().parent_ino, child_ino);
    assert_eq!(
        routing_table.get_path_for_inode(child_ino),
        Some("/ParentFolder/ChildFolder".to_string())
    );
    assert_eq!(
        routing_table.get_path_for_inode(file_ino),
        Some("/ParentFolder/ChildFolder/nested_file.txt".to_string())
    );

    // 2. Adversarial Test A: Update nested file with parents: None
    notifier.clear();
    let change_file_none = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: file_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: file_fid.to_string(),
                name: "nested_renamed.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("4096".to_string()),
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_1".to_string()),
    };

    let stats_a = sync_engine
        .apply_changes(acc, &change_file_none, "token")
        .await
        .expect("Apply change_file_none failed");
    assert_eq!(stats_a.updated, 1);

    // Assert: Still under ChildFolder, NOT moved to ROOT_INO
    assert_eq!(routing_table.get_inode(file_ino).unwrap().parent_ino, child_ino);
    let child_children: Vec<String> = routing_table
        .readdir(child_ino)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(child_children, vec!["nested_renamed.txt".to_string()]);
    let root_children: Vec<String> = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(!root_children.contains(&"nested_renamed.txt".to_string()));

    // Assert shell notification
    let events_a = notifier.get_events();
    assert_eq!(events_a.len(), 1);
    assert_eq!(
        events_a[0],
        (
            "UPDATE".to_string(),
            "/ParentFolder/ChildFolder/nested_renamed.txt".to_string()
        )
    );

    // 3. Adversarial Test B: Update nested folder with parents: None
    notifier.clear();
    let change_folder_none = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: child_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: child_fid.to_string(),
                name: "ChildFolderRenamed".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_2".to_string()),
    };

    let stats_b = sync_engine
        .apply_changes(acc, &change_folder_none, "token")
        .await
        .expect("Apply change_folder_none failed");
    assert_eq!(stats_b.updated, 1);

    // Assert: Still under ParentFolder, NOT moved to ROOT_INO
    assert_eq!(routing_table.get_inode(child_ino).unwrap().parent_ino, parent_ino);
    let parent_children: Vec<String> = routing_table
        .readdir(parent_ino)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(parent_children, vec!["ChildFolderRenamed".to_string()]);

    let events_b = notifier.get_events();
    assert_eq!(events_b.len(), 1);
    assert_eq!(
        events_b[0],
        ("UPDATE".to_string(), "/ParentFolder/ChildFolderRenamed".to_string())
    );

    // 4. Adversarial Test C: Update nested file with parents: Some(vec![]) (empty list)
    notifier.clear();
    let change_file_empty = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: file_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: file_fid.to_string(),
                name: "nested_renamed_v2.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("4096".to_string()),
                trashed: Some(false),
                parents: Some(vec![]),
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_3".to_string()),
    };

    let stats_c = sync_engine
        .apply_changes(acc, &change_file_empty, "token")
        .await
        .expect("Apply change_file_empty failed");
    assert_eq!(stats_c.updated, 1);

    assert_eq!(routing_table.get_inode(file_ino).unwrap().parent_ino, child_ino);
    let events_c = notifier.get_events();
    assert_eq!(events_c.len(), 1);
    assert_eq!(
        events_c[0],
        (
            "UPDATE".to_string(),
            "/ParentFolder/ChildFolderRenamed/nested_renamed_v2.txt".to_string()
        )
    );

    // 5. Adversarial Test D: Update nested folder with parents: Some(vec![]) (empty list)
    notifier.clear();
    let change_folder_empty = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: child_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: child_fid.to_string(),
                name: "ChildFolderRenamedV2".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                trashed: Some(false),
                parents: Some(vec![]),
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_4".to_string()),
    };

    let stats_d = sync_engine
        .apply_changes(acc, &change_folder_empty, "token")
        .await
        .expect("Apply change_folder_empty failed");
    assert_eq!(stats_d.updated, 1);

    assert_eq!(routing_table.get_inode(child_ino).unwrap().parent_ino, parent_ino);
    let events_d = notifier.get_events();
    assert_eq!(events_d.len(), 1);
    assert_eq!(
        events_d[0],
        ("UPDATE".to_string(), "/ParentFolder/ChildFolderRenamedV2".to_string())
    );

    // 6. Adversarial Test E: Explicit move to ROOT_INO via parents: ["root"]
    notifier.clear();
    let change_move_root = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: file_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: file_fid.to_string(),
                name: "moved_to_root.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("4096".to_string()),
                trashed: Some(false),
                parents: Some(vec!["root".to_string()]),
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_5".to_string()),
    };

    let stats_e = sync_engine
        .apply_changes(acc, &change_move_root, "token")
        .await
        .expect("Apply change_move_root failed");
    assert_eq!(stats_e.updated, 1);

    // Assert: Now moved to ROOT_INO
    assert_eq!(
        routing_table.get_inode(file_ino).unwrap().parent_ino,
        InMemoryRoutingTable::ROOT_INO
    );
    let root_children_e: Vec<String> = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(root_children_e.contains(&"moved_to_root.txt".to_string()));

    // Old parent (child_ino) must no longer have this file
    let child_children_e: Vec<String> = routing_table
        .readdir(child_ino)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(!child_children_e.contains(&"moved_to_root.txt".to_string()));

    // When parent is ROOT_INO, notification path must be the plain name without leading slash
    let events_e = notifier.get_events();
    assert_eq!(events_e.len(), 1);
    assert_eq!(
        events_e[0],
        ("UPDATE".to_string(), "moved_to_root.txt".to_string())
    );
}

#[tokio::test]
async fn test_delta_sync_collision_multi_iteration_stress_matrix() {
    let server = MockServer::start().await;

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

    let (_client, routing_table, placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let acc1 = "mohdzaidk035@gmail.com";
    let acc2 = "ucloudfs@gmail.com";
    let acc3 = "analyst@gmail.com";
    placement.register_account(acc1, 100 * 1024 * 1024, 10 * 1024 * 1024);
    placement.register_account(acc2, 100 * 1024 * 1024, 20 * 1024 * 1024);
    placement.register_account(acc3, 100 * 1024 * 1024, 30 * 1024 * 1024);

    // =========================================================================
    // SECTION 1: Multi-Iteration Alternating Delta Sync on Colliding Folders/Files
    // =========================================================================
    let fld1_id = "gdrive_fld_budget_1";
    let fld2_id = "gdrive_fld_budget_2";
    let fil1_id = "gdrive_fil_summary_1";
    let fil2_id = "gdrive_fil_summary_2";

    let fld1_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Budget2024",
            Some(acc1),
            Some(fld1_id),
        )
        .expect("insert fld1");

    let fld2_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Budget2024",
            Some(acc2),
            Some(fld2_id),
        )
        .expect("insert fld2");

    assert_ne!(fld1_ino, fld2_ino);

    let fil1_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: "summary.xlsx",
            size: 1000,
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc1.to_string(),
                google_file_id: fil1_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("insert fil1");

    let fil2_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: InMemoryRoutingTable::ROOT_INO,
            name: "summary.xlsx",
            size: 2000,
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc2.to_string(),
                google_file_id: fil2_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("insert fil2");

    assert_ne!(fil1_ino, fil2_ino);

    // Initial assertions
    assert_eq!(
        routing_table.get_inode(fld1_ino).unwrap().name,
        "Budget2024 (mohdzaidk035)"
    );
    assert_eq!(
        routing_table.get_inode(fld2_ino).unwrap().name,
        "Budget2024 (ucloudfs)"
    );
    assert_eq!(
        routing_table.get_inode(fil1_ino).unwrap().name,
        "summary (mohdzaidk035).xlsx"
    );
    assert_eq!(
        routing_table.get_inode(fil2_ino).unwrap().name,
        "summary (ucloudfs).xlsx"
    );

    // Iteration 1: Sync acc1 with updated size 1500
    notifier.clear();
    let changes_acc1_r1 = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: fld1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fld1_id.to_string(),
                    name: "Budget2024".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: fil1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fil1_id.to_string(),
                    name: "summary.xlsx".to_string(),
                    mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                    size: Some("1500".to_string()),
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("tok1".to_string()),
    };
    let s1 = sync_engine.apply_changes(acc1, &changes_acc1_r1, "t").await.unwrap();
    assert_eq!(s1.updated, 2);
    assert_eq!(routing_table.get_inode(fil1_ino).unwrap().size, 1500);

    // Iteration 2: Sync acc2 with updated size 2500
    notifier.clear();
    let changes_acc2_r1 = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: fld2_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fld2_id.to_string(),
                    name: "Budget2024".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: fil2_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fil2_id.to_string(),
                    name: "summary.xlsx".to_string(),
                    mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                    size: Some("2500".to_string()),
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("tok2".to_string()),
    };
    let s2 = sync_engine.apply_changes(acc2, &changes_acc2_r1, "t").await.unwrap();
    assert_eq!(s2.updated, 2);
    assert_eq!(routing_table.get_inode(fil2_ino).unwrap().size, 2500);

    // Iteration 3: Sync acc1 AGAIN with updated size 1800
    notifier.clear();
    let changes_acc1_r2 = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: fld1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fld1_id.to_string(),
                    name: "Budget2024".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: fil1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fil1_id.to_string(),
                    name: "summary.xlsx".to_string(),
                    mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                    size: Some("1800".to_string()),
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("tok3".to_string()),
    };
    let s3 = sync_engine.apply_changes(acc1, &changes_acc1_r2, "t").await.unwrap();
    assert_eq!(s3.updated, 2);
    assert_eq!(routing_table.get_inode(fil1_ino).unwrap().size, 1800);

    // Iteration 4: Sync acc2 AGAIN with updated size 2800
    notifier.clear();
    let changes_acc2_r2 = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: fld2_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fld2_id.to_string(),
                    name: "Budget2024".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: fil2_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: fil2_id.to_string(),
                    name: "summary.xlsx".to_string(),
                    mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                    size: Some("2800".to_string()),
                    trashed: Some(false),
                    parents: None,
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("tok4".to_string()),
    };
    let s4 = sync_engine.apply_changes(acc2, &changes_acc2_r2, "t").await.unwrap();
    assert_eq!(s4.updated, 2);
    assert_eq!(routing_table.get_inode(fil2_ino).unwrap().size, 2800);

    // Verify all names and lookups after 4 alternating rounds
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "Budget2024 (mohdzaidk035)"),
        Some(fld1_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "Budget2024 (ucloudfs)"),
        Some(fld2_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "summary (mohdzaidk035).xlsx"),
        Some(fil1_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "summary (ucloudfs).xlsx"),
        Some(fil2_ino)
    );

    // Untagged names MUST NOT resolve
    assert!(routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "Budget2024").is_none());
    assert!(routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "summary.xlsx").is_none());

    // =========================================================================
    // SECTION 2: Nested Subfolders and Parent Preservation Across Updates
    // =========================================================================
    let proj1_id = "gdrive_proj1_zaid";
    let proj1_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Projects",
            Some(acc1),
            Some(proj1_id),
        )
        .expect("insert proj1");

    let specs1_id = "gdrive_specs1_zaid";
    let specs1_ino = routing_table
        .insert_or_merge_directory(
            proj1_ino,
            "Specs",
            Some(acc1),
            Some(specs1_id),
        )
        .expect("insert nested specs1");

    let doc1_id = "gdrive_doc1_zaid";
    let doc1_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: proj1_ino,
            name: "readme.txt",
            size: 50,
            mime_type: "text/plain",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc1.to_string(),
                google_file_id: doc1_id.to_string(),
            },
            web_view_link: None,
        })
        .expect("insert nested doc1");

    // Send delta sync update for specs1 and doc1 with parents: None
    notifier.clear();
    let changes_nested = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: specs1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: specs1_id.to_string(),
                    name: "Specs".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: None, // parents omitted!
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: doc1_id.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: doc1_id.to_string(),
                    name: "readme.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    size: Some("120".to_string()),
                    trashed: Some(false),
                    parents: None, // parents omitted!
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("tok_nested".to_string()),
    };

    let s_nested = sync_engine.apply_changes(acc1, &changes_nested, "t").await.unwrap();
    assert_eq!(s_nested.updated, 2);

    // Verify nested items remained under proj1_ino, NOT ROOT_INO
    assert_eq!(routing_table.get_inode(specs1_ino).unwrap().parent_ino, proj1_ino);
    assert_eq!(routing_table.get_inode(doc1_ino).unwrap().parent_ino, proj1_ino);
    assert_eq!(routing_table.get_inode(doc1_ino).unwrap().size, 120);

    // Verify shell notification paths include parent directory path
    let events = notifier.get_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], ("UPDATE".to_string(), "/Projects/Specs".to_string()));
    assert_eq!(events[1], ("UPDATE".to_string(), "/Projects/readme.txt".to_string()));

    // =========================================================================
    // SECTION 3: Tri-Account Collision on Directory
    // =========================================================================
    let conf1_id = "gdrive_conf1";
    let conf2_id = "gdrive_conf2";
    let conf3_id = "gdrive_conf3";

    let conf1_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Confidential",
            Some(acc1),
            Some(conf1_id),
        )
        .expect("conf1");

    let conf2_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Confidential",
            Some(acc2),
            Some(conf2_id),
        )
        .expect("conf2");

    let conf3_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Confidential",
            Some(acc3),
            Some(conf3_id),
        )
        .expect("conf3");

    assert_eq!(
        routing_table.get_inode(conf1_ino).unwrap().name,
        "Confidential (mohdzaidk035)"
    );
    assert_eq!(
        routing_table.get_inode(conf2_ino).unwrap().name,
        "Confidential (ucloudfs)"
    );
    assert_eq!(
        routing_table.get_inode(conf3_ino).unwrap().name,
        "Confidential (analyst)"
    );

    // Now send delta sync on acc3 with untagged name "Confidential"
    notifier.clear();
    let changes_conf3 = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: conf3_id.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: conf3_id.to_string(),
                name: "Confidential".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("tok_conf3".to_string()),
    };
    sync_engine.apply_changes(acc3, &changes_conf3, "t").await.unwrap();

    assert_eq!(
        routing_table.get_inode(conf3_ino).unwrap().name,
        "Confidential (analyst)"
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "Confidential (analyst)"),
        Some(conf3_ino)
    );

    // =========================================================================
    // SECTION 4: Case-Insensitivity Collision Disambiguation
    // =========================================================================
    let fin1_id = "gdrive_fin1";
    let fin2_id = "gdrive_fin2";

    let fin1_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "Financials",
            Some(acc1),
            Some(fin1_id),
        )
        .expect("fin1");

    let fin2_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "financials", // lowercase!
            Some(acc2),
            Some(fin2_id),
        )
        .expect("fin2");

    assert_ne!(fin1_ino, fin2_ino);
    assert_eq!(
        routing_table.get_inode(fin1_ino).unwrap().name,
        "financials (mohdzaidk035)"
    );
    assert_eq!(
        routing_table.get_inode(fin2_ino).unwrap().name,
        "financials (ucloudfs)"
    );

    // Sync acc2 sending untagged lowercase "financials"
    notifier.clear();
    let changes_fin2 = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: fin2_id.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: fin2_id.to_string(),
                name: "financials".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                trashed: Some(false),
                parents: None,
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("tok_fin2".to_string()),
    };
    sync_engine.apply_changes(acc2, &changes_fin2, "t").await.unwrap();

    assert_eq!(
        routing_table.get_inode(fin2_ino).unwrap().name,
        "financials (ucloudfs)"
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "financials (ucloudfs)"),
        Some(fin2_ino)
    );
    assert_eq!(
        routing_table.lookup(InMemoryRoutingTable::ROOT_INO, "Financials (mohdzaidk035)"),
        Some(fin1_ino)
    );
}

#[tokio::test]
async fn test_adversarial_nested_collision_and_unknown_parent_preservation() {
    let server = MockServer::start().await;

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

    let (_client, routing_table, placement, _session, notifier, sync_engine) =
        setup_test_env(&server).await;

    let acc1 = "alice@gmail.com";
    let acc2 = "bob@gmail.com";
    placement.register_account(acc1, 100 * 1024 * 1024, 10 * 1024 * 1024);
    placement.register_account(acc2, 100 * 1024 * 1024, 20 * 1024 * 1024);

    // Common parent folder
    let shared_folder_fid_alice = "gdrive_shared_alice";
    let shared_folder_fid_bob = "gdrive_shared_bob";

    let shared_dir_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "SharedProjects",
            Some(acc1),
            Some(shared_folder_fid_alice),
        )
        .expect("SharedProjects insert failed");

    // Also register bob's view of shared folder (same merged virtual folder)
    routing_table.record_remote_folder_id(shared_dir_ino, acc2, shared_folder_fid_bob);

    // 1. Nested file collision inside SharedProjects: "budget.xlsx"
    let file_fid_alice = "fid_budget_alice";
    let file_fid_bob = "fid_budget_bob";

    let file1_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: shared_dir_ino,
            name: "budget.xlsx",
            size: 1000,
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc1.to_string(),
                google_file_id: file_fid_alice.to_string(),
            },
            web_view_link: None,
        })
        .expect("Alice budget insert failed");

    let file2_ino = routing_table
        .insert_file(NewFileParams {
            parent_ino: shared_dir_ino,
            name: "budget.xlsx",
            size: 2000,
            mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            modified_time: 1_700_000_000,
            created_time: 1_700_000_000,
            location: StorageLocation::SingleAccount {
                account_id: acc2.to_string(),
                google_file_id: file_fid_bob.to_string(),
            },
            web_view_link: None,
        })
        .expect("Bob budget insert failed");

    assert_ne!(file1_ino, file2_ino);
    assert_eq!(
        routing_table.get_inode(file1_ino).unwrap().name,
        "budget (alice).xlsx"
    );
    assert_eq!(
        routing_table.get_inode(file2_ino).unwrap().name,
        "budget (bob).xlsx"
    );

    // 2. Delta sync from Alice with untagged "budget.xlsx" and an UNKNOWN parent ID
    // Unknown parent ID must fall back to existing_parent_ino and NOT reset to ROOT_INO!
    notifier.clear();
    let change_unknown_parent = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: file_fid_alice.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: file_fid_alice.to_string(),
                name: "budget.xlsx".to_string(),
                mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                size: Some("1500".to_string()),
                trashed: Some(false),
                parents: Some(vec!["unknown_ghost_parent_fid".to_string()]),
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("tok_adv_2".to_string()),
    };

    let stats = sync_engine
        .apply_changes(acc1, &change_unknown_parent, "token")
        .await
        .expect("Apply change_unknown_parent failed");
    assert_eq!(stats.updated, 1);

    // Assert file is still under shared_dir_ino
    let node_alice = routing_table.get_inode(file1_ino).unwrap();
    assert_eq!(node_alice.parent_ino, shared_dir_ino);
    assert_eq!(node_alice.name, "budget (alice).xlsx");
    assert_eq!(node_alice.size, 1500);

    // Assert notification path reflects true nested disambiguated path
    let events = notifier.get_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        (
            "UPDATE".to_string(),
            "/SharedProjects/budget (alice).xlsx".to_string()
        )
    );
}

#[tokio::test]
async fn test_out_of_order_nested_folder_hydration() {
    let routing_table = Arc::new(InMemoryRoutingTable::new());

    // Construct data where ChildFolder is delivered BEFORE ParentFolder (strict reverse order)
    let files = vec![
        GoogleFileMetadata {
            id: "grandchild_789".to_string(),
            name: "GrandchildFolder".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["child_123".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_doc".to_string(),
            name: "Document.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: Some("4096".to_string()),
            modified_time: None,
            parents: Some(vec!["child_123".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "child_123".to_string(),
            name: "ChildFolder".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["parent_456".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "parent_456".to_string(),
            name: "ParentFolder".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "ext_boundary".to_string(),
            name: "ExternalBoundaryFolder".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["unfetched_external_root_999".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
    ];

    let data = HydratedAccountData {
        account_id: "test_acc@gmail.com".to_string(),
        user_email: Some("test_acc@gmail.com".to_string()),
        storage_limit_bytes: 100 * 1024 * 1024,
        storage_used_bytes: 10 * 1024 * 1024,
        start_page_token: "tok_start_1".to_string(),
        files,
    };

    // Hydrate into routing table
    ucloud_fs::routing::table::hydrate_account_into_table(&routing_table, &data)
        .expect("Hydration failed");

    // 1. Verify ROOT_INO contents
    let root_entries = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .expect("readdir ROOT_INO failed");

    // ParentFolder and ExternalBoundaryFolder MUST be in ROOT_INO
    assert!(
        root_entries.iter().any(|e| e.name == "ParentFolder"),
        "ParentFolder must be present in ROOT_INO"
    );
    assert!(
        root_entries.iter().any(|e| e.name == "ExternalBoundaryFolder"),
        "ExternalBoundaryFolder must be present in ROOT_INO"
    );

    // ChildFolder and GrandchildFolder must NOT be in ROOT_INO
    assert!(
        !root_entries.iter().any(|e| e.name == "ChildFolder"),
        "ChildFolder must NOT be present in ROOT_INO (should not be flattened)"
    );
    assert!(
        !root_entries.iter().any(|e| e.name == "GrandchildFolder"),
        "GrandchildFolder must NOT be present in ROOT_INO"
    );
    assert!(
        !root_entries.iter().any(|e| e.name == "Document.pdf"),
        "Document.pdf must NOT be present in ROOT_INO"
    );

    // 2. Locate ParentFolder inode
    let parent_node = root_entries
        .iter()
        .find(|e| e.name == "ParentFolder")
        .expect("ParentFolder node missing");
    let parent_ino = parent_node.ino;
    assert_eq!(parent_node.parent_ino, InMemoryRoutingTable::ROOT_INO);

    // 3. Verify ParentFolder contents
    let parent_entries = routing_table
        .readdir(parent_ino)
        .expect("readdir ParentFolder failed");
    assert_eq!(
        parent_entries.len(),
        1,
        "ParentFolder should contain exactly 1 child (ChildFolder)"
    );
    let child_node = &parent_entries[0];
    assert_eq!(child_node.name, "ChildFolder");
    assert_eq!(child_node.parent_ino, parent_ino);
    let child_ino = child_node.ino;

    // 4. Verify ChildFolder contents
    let child_entries = routing_table
        .readdir(child_ino)
        .expect("readdir ChildFolder failed");
    assert_eq!(
        child_entries.len(),
        2,
        "ChildFolder should contain GrandchildFolder and Document.pdf"
    );

    let grandchild_node = child_entries
        .iter()
        .find(|e| e.name == "GrandchildFolder")
        .expect("GrandchildFolder node missing");
    assert_eq!(grandchild_node.parent_ino, child_ino);

    let doc_node = child_entries
        .iter()
        .find(|e| e.name == "Document.pdf")
        .expect("Document.pdf node missing");
    assert_eq!(doc_node.parent_ino, child_ino);
    assert_eq!(doc_node.size, 4096);

    // 5. Verify path resolution
    assert_eq!(
        routing_table.resolve_path("/ParentFolder/ChildFolder").ok(),
        Some(child_ino)
    );
    assert_eq!(
        routing_table
            .resolve_path("/ParentFolder/ChildFolder/GrandchildFolder")
            .ok(),
        Some(grandchild_node.ino)
    );
    assert_eq!(
        routing_table
            .resolve_path("/ParentFolder/ChildFolder/Document.pdf")
            .ok(),
        Some(doc_node.ino)
    );
}

#[tokio::test]
async fn test_adversarial_hydration_cycles_limits_and_orphans() {
    let routing_table = Arc::new(InMemoryRoutingTable::new());

    // 1. Cyclic folders:
    // A -> B, B -> A (2-cycle)
    // S -> S (self-cycle)
    // X -> Y -> Z -> X (3-cycle)
    // Plus healthy tree: H_Root -> H_Child -> H_Grandchild (in reverse order)
    // Plus orphan branch: O_Root (parent: external_missing_cloud_id) -> O_Child -> O_Grandchild
    // Files inside cyclic, orphan, and healthy folders:
    // - file_in_a inside A
    // - file_in_b inside B
    // - file_in_s inside S
    // - file_in_h_grandchild inside H_Grandchild
    // - file_in_o_grandchild inside O_Grandchild
    // - file_with_unknown_parent inside ghost_missing_id

    let files = vec![
        // 2-cycle
        GoogleFileMetadata {
            id: "cycle_a".to_string(),
            name: "CycleA".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["cycle_b".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "cycle_b".to_string(),
            name: "CycleB".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["cycle_a".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        // Self-cycle
        GoogleFileMetadata {
            id: "self_s".to_string(),
            name: "SelfCycleS".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["self_s".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        // 3-cycle
        GoogleFileMetadata {
            id: "cycle_x".to_string(),
            name: "CycleX".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["cycle_y".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "cycle_y".to_string(),
            name: "CycleY".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["cycle_z".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "cycle_z".to_string(),
            name: "CycleZ".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["cycle_x".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        // Healthy hierarchy (delivered in reverse: Grandchild -> Child -> Root)
        GoogleFileMetadata {
            id: "h_grandchild".to_string(),
            name: "HealthyGrandchild".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["h_child".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "h_child".to_string(),
            name: "HealthyChild".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["h_root".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "h_root".to_string(),
            name: "HealthyRoot".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        // Orphan tree (parent not in fetched set): reverse order Grandchild -> Child -> Root
        GoogleFileMetadata {
            id: "o_grandchild".to_string(),
            name: "OrphanGrandchild".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["o_child".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "o_child".to_string(),
            name: "OrphanChild".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["o_root".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "o_root".to_string(),
            name: "OrphanRoot".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(vec!["ext_missing_parent_9999".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        // Files
        GoogleFileMetadata {
            id: "file_in_a".to_string(),
            name: "file_a.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("100".to_string()),
            modified_time: None,
            parents: Some(vec!["cycle_a".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_in_b".to_string(),
            name: "file_b.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("200".to_string()),
            modified_time: None,
            parents: Some(vec!["cycle_b".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_in_s".to_string(),
            name: "file_s.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("300".to_string()),
            modified_time: None,
            parents: Some(vec!["self_s".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_in_h_grandchild".to_string(),
            name: "file_h.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("400".to_string()),
            modified_time: None,
            parents: Some(vec!["h_grandchild".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_in_o_grandchild".to_string(),
            name: "file_o.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("500".to_string()),
            modified_time: None,
            parents: Some(vec!["o_grandchild".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
        GoogleFileMetadata {
            id: "file_ghost_parent".to_string(),
            name: "file_ghost.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("600".to_string()),
            modified_time: None,
            parents: Some(vec!["completely_unknown_folder_id".to_string()]),
            trashed: Some(false),
            app_properties: None,
        },
    ];

    let data = HydratedAccountData {
        account_id: "adv_test@gmail.com".to_string(),
        user_email: Some("adv_test@gmail.com".to_string()),
        storage_limit_bytes: 100 * 1024 * 1024,
        storage_used_bytes: 10 * 1024 * 1024,
        start_page_token: "tok_adv_1".to_string(),
        files,
    };

    // Hydration must complete cleanly and terminate (no infinite loop)
    ucloud_fs::routing::table::hydrate_account_into_table(&routing_table, &data)
        .expect("Hydration with cycles and orphans must succeed");

    // 1. Verify ROOT_INO contents
    let root_entries = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .expect("readdir ROOT_INO failed");

    let root_names: Vec<String> = root_entries.iter().map(|e| e.name.clone()).collect();
    assert!(root_names.contains(&"CycleA".to_string()), "CycleA must be in ROOT_INO");
    assert!(root_names.contains(&"CycleB".to_string()), "CycleB must be in ROOT_INO");
    assert!(root_names.contains(&"SelfCycleS".to_string()), "SelfCycleS must be in ROOT_INO");
    assert!(root_names.contains(&"CycleX".to_string()), "CycleX must be in ROOT_INO");
    assert!(root_names.contains(&"CycleY".to_string()), "CycleY must be in ROOT_INO");
    assert!(root_names.contains(&"CycleZ".to_string()), "CycleZ must be in ROOT_INO");
    assert!(root_names.contains(&"HealthyRoot".to_string()), "HealthyRoot must be in ROOT_INO");
    assert!(root_names.contains(&"OrphanRoot".to_string()), "OrphanRoot must be in ROOT_INO");
    assert!(root_names.contains(&"file_ghost.txt".to_string()), "file_ghost.txt must be in ROOT_INO");

    // HealthyChild and HealthyGrandchild must NOT be in ROOT_INO
    assert!(!root_names.contains(&"HealthyChild".to_string()), "HealthyChild must NOT be in ROOT_INO");
    assert!(!root_names.contains(&"HealthyGrandchild".to_string()), "HealthyGrandchild must NOT be in ROOT_INO");
    // OrphanChild and OrphanGrandchild must NOT be in ROOT_INO
    assert!(!root_names.contains(&"OrphanChild".to_string()), "OrphanChild must NOT be in ROOT_INO");
    assert!(!root_names.contains(&"OrphanGrandchild".to_string()), "OrphanGrandchild must NOT be in ROOT_INO");

    // 2. Verify file placement inside cycle folders
    let cycle_a_ino = routing_table.resolve_path("/CycleA").expect("resolve /CycleA");
    let cycle_a_entries = routing_table.readdir(cycle_a_ino).expect("readdir CycleA");
    assert!(cycle_a_entries.iter().any(|e| e.name == "file_a.txt"), "file_a.txt must be in CycleA");

    let cycle_b_ino = routing_table.resolve_path("/CycleB").expect("resolve /CycleB");
    let cycle_b_entries = routing_table.readdir(cycle_b_ino).expect("readdir CycleB");
    assert!(cycle_b_entries.iter().any(|e| e.name == "file_b.txt"), "file_b.txt must be in CycleB");

    let self_s_ino = routing_table.resolve_path("/SelfCycleS").expect("resolve /SelfCycleS");
    let self_s_entries = routing_table.readdir(self_s_ino).expect("readdir SelfCycleS");
    assert!(self_s_entries.iter().any(|e| e.name == "file_s.txt"), "file_s.txt must be in SelfCycleS");

    // 3. Verify healthy hierarchy and file placement
    let h_grandchild_ino = routing_table
        .resolve_path("/HealthyRoot/HealthyChild/HealthyGrandchild")
        .expect("resolve /HealthyRoot/HealthyChild/HealthyGrandchild");
    let h_entries = routing_table.readdir(h_grandchild_ino).expect("readdir HealthyGrandchild");
    assert!(h_entries.iter().any(|e| e.name == "file_h.txt"), "file_h.txt must be in HealthyGrandchild");

    // 4. Verify orphan tree internal hierarchy preservation
    let o_grandchild_ino = routing_table
        .resolve_path("/OrphanRoot/OrphanChild/OrphanGrandchild")
        .expect("resolve /OrphanRoot/OrphanChild/OrphanGrandchild");
    let o_entries = routing_table.readdir(o_grandchild_ino).expect("readdir OrphanGrandchild");
    assert!(o_entries.iter().any(|e| e.name == "file_o.txt"), "file_o.txt must be in OrphanGrandchild");
}

#[tokio::test]
async fn test_adversarial_16_pass_limit_and_deep_reverse_chain() {
    let routing_table = Arc::new(InMemoryRoutingTable::new());

    // Construct a 20-level linear chain:
    // L0 (parent: root)
    // L1 (parent: L0)
    // ...
    // L19 (parent: L18)
    // Delivered in REVERSE order: [L19, L18, ..., L1, L0]
    // Because the topological loop runs at most 16 passes (0..16):
    // In pass 0: L0 placed (parent root).
    // In pass 1: L1 placed (parent L0).
    // ...
    // In pass 15: L15 placed (parent L14).
    // After 16 passes (passes 0 through 15), L0..=L15 are topologically placed!
    // The remaining pending folders (L16, L17, L18, L19) were not resolved yet.
    // The safety fallback MUST terminate and place L16..=L19 under ROOT_INO.

    let mut files = Vec::new();
    for i in (0..20).rev() {
        let parent = if i == 0 {
            vec!["root".to_string()]
        } else {
            vec![format!("chain_level_{}", i - 1)]
        };
        files.push(GoogleFileMetadata {
            id: format!("chain_level_{}", i),
            name: format!("Level_{:02}", i),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            parents: Some(parent),
            trashed: Some(false),
            app_properties: None,
        });
    }

    // Add a file in L15 (topologically placed) and L19 (fallback placed)
    files.push(GoogleFileMetadata {
        id: "file_in_l15".to_string(),
        name: "data_l15.bin".to_string(),
        mime_type: "application/octet-stream".to_string(),
        size: Some("1024".to_string()),
        modified_time: None,
        parents: Some(vec!["chain_level_15".to_string()]),
        trashed: Some(false),
        app_properties: None,
    });
    files.push(GoogleFileMetadata {
        id: "file_in_l19".to_string(),
        name: "data_l19.bin".to_string(),
        mime_type: "application/octet-stream".to_string(),
        size: Some("2048".to_string()),
        modified_time: None,
        parents: Some(vec!["chain_level_19".to_string()]),
        trashed: Some(false),
        app_properties: None,
    });

    let data = HydratedAccountData {
        account_id: "deep_chain@gmail.com".to_string(),
        user_email: Some("deep_chain@gmail.com".to_string()),
        storage_limit_bytes: 100 * 1024 * 1024,
        storage_used_bytes: 10 * 1024 * 1024,
        start_page_token: "tok_deep_1".to_string(),
        files,
    };

    // Hydration must terminate within bounded time (no hang, exactly 16 passes)
    ucloud_fs::routing::table::hydrate_account_into_table(&routing_table, &data)
        .expect("Deep chain hydration must succeed");

    // L0..=L15 should form a strict nested path:
    let mut current_path = "/Level_00".to_string();
    for i in 1..=15 {
        current_path.push_str(&format!("/Level_{:02}", i));
        assert!(
            routing_table.resolve_path(&current_path).is_ok(),
            "Path {} must resolve",
            current_path
        );
    }

    // File in L15 should be inside L15
    let l15_ino = routing_table.resolve_path(&current_path).expect("resolve L15");
    let l15_entries = routing_table.readdir(l15_ino).expect("readdir L15");
    assert!(
        l15_entries.iter().any(|e| e.name == "data_l15.bin"),
        "data_l15.bin must be inside Level_15"
    );

    // L16..=L19 exceeded 16 passes and must be placed under ROOT_INO via safety fallback
    let root_entries = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .expect("readdir ROOT_INO");
    let root_names: Vec<String> = root_entries.iter().map(|e| e.name.clone()).collect();
    for i in 16..=19 {
        assert!(
            root_names.contains(&format!("Level_{:02}", i)),
            "Level_{:02} must be placed under ROOT_INO after 16 passes",
            i
        );
    }

    // File in L19 should be inside L19 (which is at /Level_19)
    let l19_ino = routing_table.resolve_path("/Level_19").expect("resolve /Level_19");
    let l19_entries = routing_table.readdir(l19_ino).expect("readdir Level_19");
    assert!(
        l19_entries.iter().any(|e| e.name == "data_l19.bin"),
        "data_l19.bin must be inside Level_19"
    );
}


#[tokio::test]
async fn test_adversarial_out_of_order_hydration_with_cross_account_collisions() {
    let routing_table = Arc::new(InMemoryRoutingTable::new());

    let acc1 = "alice@gmail.com";
    let acc2 = "bob@gmail.com";

    // 1. Hydrate Account 1 with /SharedProjects/alice_notes.txt
    let acc1_data = HydratedAccountData {
        account_id: acc1.to_string(),
        user_email: Some(acc1.to_string()),
        storage_limit_bytes: 100 * 1024 * 1024,
        storage_used_bytes: 5 * 1024 * 1024,
        start_page_token: "tok_a1".to_string(),
        files: vec![
            GoogleFileMetadata {
                id: "f_proj_alice".to_string(),
                name: "SharedProjects".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["root".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_notes_alice".to_string(),
                name: "alice_notes.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size: Some("512".to_string()),
                modified_time: None,
                parents: Some(vec!["f_proj_alice".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
        ],
    };

    ucloud_fs::routing::table::hydrate_account_into_table(&routing_table, &acc1_data)
        .expect("Acc1 hydration failed");

    // Before collision, /SharedProjects should exist directly
    assert!(routing_table.resolve_path("/SharedProjects").is_ok());

    // 2. Hydrate Account 2 with colliding SharedProjects, delivered in REVERSE order:
    // - File bob_specs.pdf (parent: f_deep_bob)
    // - Folder DeepSpecs (parent: f_proj_bob)
    // - Folder SharedProjects (parent: root) -> Collides with acc1
    // - Folder ExtChild (parent: f_ext1)
    // - Folder ExtBoundary1 (parent: unfetched_ext_drive_root)
    // - Folder ExtBoundary2 (parent: unfetched_ext_drive_root)
    let acc2_data = HydratedAccountData {
        account_id: acc2.to_string(),
        user_email: Some(acc2.to_string()),
        storage_limit_bytes: 100 * 1024 * 1024,
        storage_used_bytes: 10 * 1024 * 1024,
        start_page_token: "tok_b1".to_string(),
        files: vec![
            GoogleFileMetadata {
                id: "f_specs_bob".to_string(),
                name: "bob_specs.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: Some("4096".to_string()),
                modified_time: None,
                parents: Some(vec!["f_deep_bob".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_ext_child".to_string(),
                name: "ExtChild".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["f_ext1".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_deep_bob".to_string(),
                name: "DeepSpecs".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["f_proj_bob".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_ext2".to_string(),
                name: "ExtBoundary2".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["unfetched_ext_drive_root".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_ext1".to_string(),
                name: "ExtBoundary1".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["unfetched_ext_drive_root".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
            GoogleFileMetadata {
                id: "f_proj_bob".to_string(),
                name: "SharedProjects".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                modified_time: None,
                parents: Some(vec!["root".to_string()]),
                trashed: Some(false),
                app_properties: None,
            },
        ],
    };

    ucloud_fs::routing::table::hydrate_account_into_table(&routing_table, &acc2_data)
        .expect("Acc2 hydration failed");

    // 3. Verify ROOT_INO contents
    let root_entries = routing_table
        .readdir(InMemoryRoutingTable::ROOT_INO)
        .expect("readdir ROOT_INO failed");
    let root_names: Vec<String> = root_entries.iter().map(|e| e.name.clone()).collect();

    // Disambiguated names must appear in ROOT_INO
    assert!(
        root_names.contains(&"SharedProjects (alice)".to_string()),
        "SharedProjects (alice) must be in ROOT_INO"
    );
    assert!(
        root_names.contains(&"SharedProjects (bob)".to_string()),
        "SharedProjects (bob) must be in ROOT_INO"
    );
    // Untagged SharedProjects must NOT appear in ROOT_INO
    assert!(
        !root_names.contains(&"SharedProjects".to_string()),
        "Untagged SharedProjects must NOT be in ROOT_INO"
    );

    // External roots must appear in ROOT_INO
    assert!(
        root_names.contains(&"ExtBoundary1".to_string()),
        "ExtBoundary1 must be in ROOT_INO"
    );
    assert!(
        root_names.contains(&"ExtBoundary2".to_string()),
        "ExtBoundary2 must be in ROOT_INO"
    );

    // Nested folders/files must NOT be in ROOT_INO
    assert!(
        !root_names.contains(&"DeepSpecs".to_string()),
        "DeepSpecs must NOT be in ROOT_INO"
    );
    assert!(
        !root_names.contains(&"ExtChild".to_string()),
        "ExtChild must NOT be in ROOT_INO"
    );
    assert!(
        !root_names.contains(&"bob_specs.pdf".to_string()),
        "bob_specs.pdf must NOT be in ROOT_INO"
    );
    assert!(
        !root_names.contains(&"alice_notes.txt".to_string()),
        "alice_notes.txt must NOT be in ROOT_INO"
    );

    // 4. Verify path resolution in both trees
    assert!(
        routing_table
            .resolve_path("/SharedProjects (alice)/alice_notes.txt")
            .is_ok(),
        "/SharedProjects (alice)/alice_notes.txt must resolve"
    );
    assert!(
        routing_table
            .resolve_path("/SharedProjects (bob)/DeepSpecs/bob_specs.pdf")
            .is_ok(),
        "/SharedProjects (bob)/DeepSpecs/bob_specs.pdf must resolve"
    );
    assert!(
        routing_table.resolve_path("/ExtBoundary1/ExtChild").is_ok(),
        "/ExtBoundary1/ExtChild must resolve"
    );
}

#[tokio::test]
async fn test_web_ui_upload_in_subfolder_does_not_multiply_root_folders_and_deletion_persists() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/about"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "storageQuota": {
                "limit": "104857600",
                "usage": "20000000"
            }
        })))
        .mount(&server)
        .await;

    let (_client, routing_table, _placement, _session, _notifier, sync_engine) =
        setup_test_env(&server).await;

    let acc = "ucloudfs@gmail.com";
    sync_engine.set_start_page_token(acc, "token_initial".to_string());

    // 1. Initially create "WorkDocs" folder
    let work_fid = "google_folder_workdocs";
    let work_ino = routing_table
        .insert_or_merge_directory(
            InMemoryRoutingTable::ROOT_INO,
            "WorkDocs",
            Some(acc),
            Some(work_fid),
        )
        .expect("Failed to insert WorkDocs");

    assert_eq!(routing_table.count(), 2); // ROOT + WorkDocs

    // 2. Simulate Web UI upload: Google returns changes containing both WorkDocs touch and a new file inside WorkDocs
    let changes_web_upload = GoogleChangesList {
        changes: vec![
            GoogleChangeItem {
                file_id: work_fid.to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: work_fid.to_string(),
                    name: "WorkDocs".to_string(),
                    mime_type: "application/vnd.google-apps.folder".to_string(),
                    size: None,
                    trashed: Some(false),
                    parents: Some(vec!["root".to_string()]),
                    modified_time: None,
                    app_properties: None,
                }),
            },
            GoogleChangeItem {
                file_id: "google_file_budget_xlsx".to_string(),
                removed: Some(false),
                file: Some(GoogleFileMetadata {
                    id: "google_file_budget_xlsx".to_string(),
                    name: "budget.xlsx".to_string(),
                    mime_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
                    size: Some("45000".to_string()),
                    trashed: Some(false),
                    parents: Some(vec![work_fid.to_string()]), // Inside WorkDocs!
                    modified_time: None,
                    app_properties: None,
                }),
            },
        ],
        next_page_token: None,
        new_start_page_token: Some("token_after_upload".to_string()),
    };

    let stats = sync_engine
        .apply_changes(acc, &changes_web_upload, "mock_token")
        .await
        .expect("Apply web upload failed");

    assert_eq!(stats.updated, 1); // WorkDocs updated
    assert_eq!(stats.created, 1); // budget.xlsx created

    // Assert: WorkDocs did NOT multiply in ROOT_INO!
    let root_children = routing_table.readdir(InMemoryRoutingTable::ROOT_INO).unwrap();
    assert_eq!(
        root_children.len(),
        1,
        "Root directory must contain EXACTLY ONE folder, not duplicated"
    );
    assert_eq!(root_children[0].name, "WorkDocs");
    assert_eq!(root_children[0].ino, work_ino);

    // Assert: budget.xlsx is INSIDE WorkDocs, NOT in ROOT_INO
    let work_children = routing_table.readdir(work_ino).unwrap();
    assert_eq!(work_children.len(), 1);
    assert_eq!(work_children[0].name, "budget.xlsx");
    assert_eq!(
        routing_table.resolve_path("/WorkDocs/budget.xlsx").unwrap(),
        work_children[0].ino
    );

    // 3. Repeat delta sync with another event: ensure no duplicates spawn
    let changes_repeat = GoogleChangesList {
        changes: vec![GoogleChangeItem {
            file_id: work_fid.to_string(),
            removed: Some(false),
            file: Some(GoogleFileMetadata {
                id: work_fid.to_string(),
                name: "WorkDocs".to_string(),
                mime_type: "application/vnd.google-apps.folder".to_string(),
                size: None,
                trashed: Some(false),
                parents: Some(vec!["root".to_string()]),
                modified_time: None,
                app_properties: None,
            }),
        }],
        next_page_token: None,
        new_start_page_token: Some("token_after_repeat".to_string()),
    };

    let stats_repeat = sync_engine
        .apply_changes(acc, &changes_repeat, "mock_token")
        .await
        .expect("Repeat sync failed");
    assert_eq!(stats_repeat.updated, 1);

    let root_after_repeat = routing_table.readdir(InMemoryRoutingTable::ROOT_INO).unwrap();
    assert_eq!(
        root_after_repeat.len(),
        1,
        "Root directory must still have EXACTLY ONE folder after repeated sync"
    );
    assert_eq!(root_after_repeat[0].name, "WorkDocs");

    // 4. Delete the folder locally and verify it does NOT bounce back
    let _ = routing_table.remove(work_ino, "budget.xlsx");
    let _ = routing_table.remove(InMemoryRoutingTable::ROOT_INO, "WorkDocs");
    assert_eq!(routing_table.readdir(InMemoryRoutingTable::ROOT_INO).unwrap().len(), 0);

    // Now simulate subsequent poll with empty changes since token was advanced:
    let changes_empty = GoogleChangesList {
        changes: vec![],
        next_page_token: None,
        new_start_page_token: Some("token_final".to_string()),
    };

    let stats_empty = sync_engine
        .apply_changes(acc, &changes_empty, "mock_token")
        .await
        .expect("Empty sync failed");
    assert_eq!(stats_empty.created, 0);
    assert_eq!(stats_empty.updated, 0);

    // Assert: ROOT remains completely empty, deleted folder does NOT resurrect
    assert_eq!(
        routing_table.readdir(InMemoryRoutingTable::ROOT_INO).unwrap().len(),
        0,
        "Deleted folder must NOT resurrect on subsequent delta poll"
    );
}
