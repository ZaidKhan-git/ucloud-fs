use std::sync::Arc;

use tokio::task::JoinSet;
use ucloud_fs::routing::placement::{PlacementDecision, PlacementEngine};
use ucloud_fs::vfs::prompt::AutoRejectPrompter;

#[tokio::test]
async fn test_concurrent_quota_reservation_race() {
    let engine = Arc::new(PlacementEngine::with_safety_margin(16 * 1024 * 1024));
    let gb = 1024 * 1024 * 1024;

    // Configure 2 accounts with exactly 10 GB free each (15 GB limit, 5 GB used)
    engine.register_account("Account_A", 15 * gb, 5 * gb);
    engine.register_account("Account_B", 15 * gb, 5 * gb);

    let mut join_set = JoinSet::new();

    // Simultaneously fire two 8 GB uploads at the exact same microsecond
    for i in 0..2 {
        let eng = Arc::clone(&engine);
        let fname = format!("large_file_{i}.mkv");

        join_set.spawn(async move {
            let prompter = AutoRejectPrompter;
            eng.resolve_placement(8 * gb, &fname, &prompter).await
        });
    }

    let mut assigned_accounts = Vec::new();
    let mut guards = Vec::new();

    while let Some(res) = join_set.join_next().await {
        let decision_res = res.expect("Task panicked");
        let decision = decision_res.expect("Placement failed");

        match decision {
            PlacementDecision::SingleAccount(guard) => {
                assigned_accounts.push(guard.account_id.clone());
                guards.push(guard);
            }
            PlacementDecision::StripedManifest(_) => {
                panic!("Expected single account placement for 8 GB file");
            }
        }
    }

    // Assert that the two simultaneous 8 GB uploads claimed DIFFERENT accounts!
    assert_eq!(assigned_accounts.len(), 2);
    assert_ne!(
        assigned_accounts[0], assigned_accounts[1],
        "RACE CONDITION DETECTED: Both 8 GB uploads collided on the same account {:?}!",
        assigned_accounts
    );

    assert!(
        assigned_accounts.contains(&"Account_A".to_string()),
        "Account_A was not claimed"
    );
    assert!(
        assigned_accounts.contains(&"Account_B".to_string()),
        "Account_B was not claimed"
    );

    // Each account must now have only 2 GB effective free space
    let free_a = engine.effective_free_space("Account_A").unwrap();
    let free_b = engine.effective_free_space("Account_B").unwrap();

    assert_eq!(free_a, 2 * gb);
    assert_eq!(free_b, 2 * gb);

    println!(
        "Concurrent quota reservation race test PASSED: Upload 1 claimed '{}', Upload 2 routed to '{}' with 0 collisions.",
        assigned_accounts[0], assigned_accounts[1]
    );
}
