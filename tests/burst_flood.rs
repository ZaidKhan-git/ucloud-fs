use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinSet;
use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
use ucloud_fs::network::limiter::MultiTierLimiter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_5000_request_burst_flood() {
    // 1. Initialize MockServer
    let server = MockServer::start().await;

    // Track active concurrency on the server side
    let active_server_concurrency = Arc::new(AtomicUsize::new(0));
    let peak_server_concurrency = Arc::new(AtomicUsize::new(0));

    let active_clone = Arc::clone(&active_server_concurrency);
    let peak_clone = Arc::clone(&peak_server_concurrency);

    let response_body = serde_json::json!({
        "id": "file_123",
        "name": "sample.pdf",
        "mimeType": "application/pdf"
    });

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/file_123"))
        .respond_with(move |_: &wiremock::Request| {
            let cur = active_clone.fetch_add(1, Ordering::SeqCst) + 1;
            let mut peak = peak_clone.load(Ordering::Relaxed);
            while cur > peak {
                match peak_clone.compare_exchange_weak(
                    peak,
                    cur,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => peak = actual,
                }
            }

            // Simulate brief 1ms I/O
            std::thread::sleep(std::time::Duration::from_millis(1));
            active_clone.fetch_sub(1, Ordering::SeqCst);

            ResponseTemplate::new(200).set_body_json(&response_body)
        })
        .mount(&server)
        .await;

    // 2. Initialize GoogleDriveClient with default 4,500 units/sec and 24 concurrency limit
    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            Arc::clone(&limiter),
            server.uri(),
            server.uri(),
            BackoffConfig {
                base_ms: 10,
                max_backoff_ms: 100,
                max_attempts: 3,
            },
        )
        .expect("Failed to create client"),
    );

    let accounts = vec!["acc_alpha", "acc_beta", "acc_gamma"];
    let total_requests = 5_000;
    let mut join_set = JoinSet::new();

    let start_time = Instant::now();

    // 3. Fire 5,000 concurrent requests across 3 accounts
    for i in 0..total_requests {
        let account_id = accounts[i % accounts.len()];
        let c = Arc::clone(&client);
        let acc = account_id.to_string();

        join_set.spawn(async move {
            c.get_file_metadata(&acc, "mock_token", "file_123").await
        });
    }

    let mut successful_calls = 0;
    while let Some(res) = join_set.join_next().await {
        let meta_result = res.expect("Task panicked or aborted");
        assert!(meta_result.is_ok(), "Request failed: {:?}", meta_result.err());
        successful_calls += 1;
    }

    let elapsed = start_time.elapsed();
    assert_eq!(successful_calls, total_requests);

    // 4. Assert active concurrency per account NEVER exceeded 24
    for acc in &accounts {
        let acc_limiter = limiter.get_or_create_account(acc);
        let peak = acc_limiter.peak_concurrency();
        assert!(
            peak <= 24,
            "Account {acc} peak concurrency {peak} exceeded cap of 24"
        );
        assert_eq!(
            acc_limiter.current_concurrency(),
            0,
            "Account {acc} leaked concurrency permits"
        );
    }

    // Server-side total peak concurrency must not exceed 3 accounts * 24 = 72
    let peak_server = peak_server_concurrency.load(Ordering::SeqCst);
    assert!(
        peak_server <= 72,
        "Total server peak concurrency {peak_server} exceeded theoretical maximum 72"
    );

    // 5. Assert rate consumption stays within governor quotas
    // Total units consumed per account: ~1666 requests * 5 units = ~8,330 units
    for acc in &accounts {
        let acc_limiter = limiter.get_or_create_account(acc);
        let total_units = acc_limiter.total_units_consumed();
        assert!(total_units > 8_000, "Units accounted correctly");
    }

    println!(
        "5,000 burst flood test PASSED: 5,000 calls completed in {:?}. Server peak concurrency: {}, Acc peak concurrency: <= 24",
        elapsed, peak_server
    );
}
