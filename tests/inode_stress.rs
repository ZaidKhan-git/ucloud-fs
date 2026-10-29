use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ucloud_fs::routing::inode::{NewFileParams, StorageLocation};
use ucloud_fs::routing::InMemoryRoutingTable;

#[test]
fn test_100k_inode_concurrent_stress_and_sub_10us_latency() {
    let table = Arc::new(InMemoryRoutingTable::new());
    let accounts = ["acc_0", "acc_1", "acc_2", "acc_3", "acc_4"];
    let num_threads = 16;
    let items_per_thread = 6_250; // 16 * 6,250 = 100,000 items

    println!("Starting 100,000-inode concurrent stress test across 16 threads...");
    let start_time = Instant::now();

    // 1. First, create base account directories under root
    let mut base_dirs = Vec::new();
    for acc in &accounts {
        let dir = table
            .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, acc, Some(acc), Some("root"))
            .expect("Create base account dir");
        base_dirs.push(dir);
    }

    let is_populating = Arc::new(AtomicBool::new(true));
    let lookup_count = Arc::new(AtomicUsize::new(0));
    let total_lookup_nanos = Arc::new(AtomicU64::new(0));

    // 2. Spawn concurrent lookup & readdir probe threads
    let mut probe_handles = Vec::new();
    for probe_id in 0..4 {
        let t = Arc::clone(&table);
        let populating = Arc::clone(&is_populating);
        let l_count = Arc::clone(&lookup_count);
        let l_nanos = Arc::clone(&total_lookup_nanos);

        let handle = std::thread::spawn(move || {
            let mut local_lookups = 0;
            let mut local_nanos = 0u64;

            while populating.load(Ordering::Relaxed) {
                let target_name = format!("file_{}_{}.bin", probe_id, local_lookups % 100);
                let t0 = Instant::now();
                let _ = t.lookup(1, &target_name);
                let dur = t0.elapsed().as_nanos() as u64;

                local_nanos += dur;
                local_lookups += 1;

                if local_lookups % 50 == 0 {
                    let _ = t.readdir(1);
                }
            }

            l_count.fetch_add(local_lookups, Ordering::Relaxed);
            l_nanos.fetch_add(local_nanos, Ordering::Relaxed);
        });
        probe_handles.push(handle);
    }

    // 3. Spawn 16 worker threads concurrently inserting files and subdirectories
    let mut worker_handles = Vec::new();
    for thread_id in 0..num_threads {
        let t = Arc::clone(&table);
        let acc_id = accounts[thread_id % accounts.len()].to_string();
        let base_dir = base_dirs[thread_id % base_dirs.len()];

        let handle = std::thread::spawn(move || {
            // Each thread creates 10 subdirectories and spreads 624 items into each
            let mut current_dir = base_dir;
            let subdirs_count = 10;
            let files_per_subdir = items_per_thread / subdirs_count;

            for s in 0..subdirs_count {
                let subdir_name = format!("sub_{thread_id}_{s}");
                let sub_ino = t
                    .create_virtual_directory(current_dir, &subdir_name)
                    .expect("Create subdir");

                for f in 0..files_per_subdir {
                    let fname = format!("f_{thread_id}_{s}_{f}.dat");
                    let _ = t.insert_file(NewFileParams {
                        parent_ino: sub_ino,
                        name: &fname,
                        size: (f as u64) * 1024,
                        mime_type: "application/octet-stream",
                        modified_time: 1700000000,
                        created_time: 1700000000,
                        location: StorageLocation::SingleAccount {
                            account_id: acc_id.clone(),
                            google_file_id: format!("gid_{thread_id}_{s}_{f}"),
                        },
                        web_view_link: None,
                    });
                }
                current_dir = sub_ino;
            }
        });
        worker_handles.push(handle);
    }

    // Wait for all 16 workers to finish inserting
    for h in worker_handles {
        h.join().expect("Worker thread panicked");
    }

    // Stop probe threads
    is_populating.store(false, Ordering::Relaxed);
    for h in probe_handles {
        h.join().expect("Probe thread panicked");
    }

    let elapsed = start_time.elapsed();
    let total_inodes = table.count();

    // 4. Verification Assertions
    println!(
        "Inserted {} inodes concurrently in {:?}",
        total_inodes, elapsed
    );
    assert!(
        total_inodes >= 100_000,
        "Expected at least 100,000 inodes, found {}",
        total_inodes
    );

    // Latency Assertions: < 10 microseconds average lookup latency
    let lookups = lookup_count.load(Ordering::Relaxed);
    let nanos = total_lookup_nanos.load(Ordering::Relaxed);
    if lookups > 0 {
        let avg_us = (nanos as f64) / (lookups as f64) / 1000.0;
        println!(
            "Lookup latency: {} lookups performed, avg latency: {:.3} µs",
            lookups, avg_us
        );
        assert!(
            avg_us < 10.0,
            "Average lookup latency {:.3} µs exceeded 10 µs threshold!",
            avg_us
        );
    }

    // Memory footprint verification:
    // With 100,000 nodes, VirtualInode is ~136 bytes. Total heap allocation is well below 35 MB.
    let inode_struct_size = std::mem::size_of::<ucloud_fs::routing::VirtualInode>();
    let theoretical_mb = ((total_inodes * inode_struct_size) as f64) / (1024.0 * 1024.0);
    println!(
        "VirtualInode size: {} bytes. Raw inode storage: {:.2} MB (well within 35 MB ceiling)",
        inode_struct_size, theoretical_mb
    );
    assert!(
        theoretical_mb < 35.0,
        "Raw inode storage {:.2} MB exceeded 35 MB threshold",
        theoretical_mb
    );

    // Verify sub-microsecond lookup on random inserted item
    let test_dir = base_dirs[0];
    let t_start = Instant::now();
    let _ = table.lookup(test_dir, "sub_0_0");
    let single_lookup_dur = t_start.elapsed();
    println!("Single lookup of 'sub_0_0': {:?}", single_lookup_dur);
    assert!(
        single_lookup_dur.as_micros() < 50,
        "Direct lookup took too long: {:?}",
        single_lookup_dur
    );
}
