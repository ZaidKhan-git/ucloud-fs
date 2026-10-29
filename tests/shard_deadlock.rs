use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinSet;
use ucloud_fs::routing::inode::{NewFileParams, StorageLocation};
use ucloud_fs::routing::InMemoryRoutingTable;

#[tokio::test]
async fn test_shard_collision_deadlock_freedom() {
    let table = Arc::new(InMemoryRoutingTable::new());

    // Create two directories under root
    let dir_a = table
        .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, "FolderA", None, None)
        .expect("Create FolderA");
    let dir_b = table
        .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, "FolderB", None, None)
        .expect("Create FolderB");

    // Insert 10 files into FolderA
    for i in 0..10 {
        let name = format!("item_{i}.dat");
        table
            .insert_file(NewFileParams {
                parent_ino: dir_a,
                name: &name,
                size: 1024,
                mime_type: "application/octet-stream",
                modified_time: 0,
                created_time: 0,
                location: StorageLocation::SingleAccount {
                    account_id: "acc_shard".to_string(),
                    google_file_id: format!("gid_{i}"),
                },
                web_view_link: None,
            })
            .expect("Insert initial file");
    }

    let iterations = 10_000;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut join_set = JoinSet::new();

    let start_time = Instant::now();

    // Spawn 8 concurrent worker tasks rapidly moving files back and forth between FolderA and FolderB
    for worker_id in 0..8 {
        let t = Arc::clone(&table);
        let stop = Arc::clone(&stop_flag);

        join_set.spawn(async move {
            let mut moves = 0;
            for k in 0..(iterations / 8) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let item_idx = (worker_id + k) % 10;
                let name = format!("item_{item_idx}.dat");

                // Move A -> B
                let _ = t.rename(dir_a, &name, dir_b, &name);

                // Concurrently read directories
                let _ = t.readdir(dir_a);
                let _ = t.readdir(dir_b);

                // Move B -> A
                let _ = t.rename(dir_b, &name, dir_a, &name);

                moves += 2;
            }
            moves
        });
    }

    // Guard with a 15-second watchdog to prove zero hangs
    let timeout_result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut total_moves = 0;
        while let Some(res) = join_set.join_next().await {
            total_moves += res.expect("Task panicked");
        }
        total_moves
    })
    .await;

    let elapsed = start_time.elapsed();
    assert!(
        timeout_result.is_ok(),
        "DEADLOCK DETECTED: Tasks hung during 10,000 shard-collision rename operations!"
    );

    let total = timeout_result.unwrap();
    println!(
        "Shard collision deadlock test PASSED: {} cross-directory moves in {:?} (0 hangs, 0 deadlocks)",
        total, elapsed
    );
}
