use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::task::JoinSet;
use ucloud_fs::network::client::{BackoffConfig, GoogleDriveClient};
use ucloud_fs::network::limiter::MultiTierLimiter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Chaos responder injecting 429 and 503 on 40% of requests with reproducible seed.
#[derive(Clone)]
struct ChaosResponder {
    rng: Arc<Mutex<StdRng>>,
    total_429: Arc<AtomicUsize>,
    total_503: Arc<AtomicUsize>,
    total_200: Arc<AtomicUsize>,
    request_attempts: Arc<DashMap<String, usize>>,
}

impl ChaosResponder {
    fn new(seed: u64) -> Self {
        println!("Chaos network test initialized with SEED: 0x{:016X}", seed);
        Self {
            rng: Arc::new(Mutex::new(StdRng::seed_from_u64(seed))),
            total_429: Arc::new(AtomicUsize::new(0)),
            total_503: Arc::new(AtomicUsize::new(0)),
            total_200: Arc::new(AtomicUsize::new(0)),
            request_attempts: Arc::new(DashMap::new()),
        }
    }
}

impl Respond for ChaosResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // Track attempts per request URI + Auth header to guarantee eventual success
        let key = format!("{:?}:{:?}", request.url, request.headers.get("authorization"));
        let attempts = {
            let mut entry = self.request_attempts.entry(key).or_insert(0);
            *entry += 1;
            *entry
        };

        let roll: f64 = {
            let mut rng_guard = self.rng.lock().unwrap();
            rng_guard.gen_range(0.0..1.0)
        };

        // If attempts < 3 and roll hits fault window (40%), inject failure
        if attempts < 3 && roll < 0.20 {
            self.total_429.fetch_add(1, Ordering::Relaxed);
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(serde_json::json!({
                    "error": { "code": 429, "message": "Too Many Requests" }
                }))
        } else if attempts < 3 && roll < 0.40 {
            self.total_503.fetch_add(1, Ordering::Relaxed);
            ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": { "code": 503, "message": "Backend Temporarily Unavailable" }
            }))
        } else {
            self.total_200.fetch_add(1, Ordering::Relaxed);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chaos_file_1",
                "name": "resilient.dat",
                "mimeType": "application/octet-stream"
            }))
        }
    }
}

#[tokio::test]
async fn test_chaos_network_and_429_injection() {
    let server = MockServer::start().await;

    let seed = std::env::var("CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xCAFE_BABE_DEAD_BEEF);

    let responder = ChaosResponder::new(seed);

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/chaos_file_1"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(
        GoogleDriveClient::with_endpoints(
            Arc::clone(&limiter),
            server.uri(),
            server.uri(),
            BackoffConfig {
                base_ms: 5,
                max_backoff_ms: 50,
                max_attempts: 5,
            },
        )
        .expect("Failed to build client"),
    );

    let accounts = vec!["chaos_acc_1", "chaos_acc_2", "chaos_acc_3"];
    let num_requests = 200;
    let mut join_set = JoinSet::new();

    let start = Instant::now();

    for i in 0..num_requests {
        let acc = accounts[i % accounts.len()].to_string();
        let c = Arc::clone(&client);
        let token = format!("token_for_req_{i}");

        join_set.spawn(async move {
            c.get_file_metadata(&acc, &token, "chaos_file_1").await
        });
    }

    let mut successful_calls = 0;
    while let Some(res) = join_set.join_next().await {
        let meta_result = res.expect("Task panicked during chaos test");
        assert!(
            meta_result.is_ok(),
            "Expected 100% recovery under chaos, got error: {:?}",
            meta_result.err()
        );
        successful_calls += 1;
    }

    let elapsed = start.elapsed();
    assert_eq!(successful_calls, num_requests);

    let n_429 = responder.total_429.load(Ordering::Relaxed);
    let n_503 = responder.total_503.load(Ordering::Relaxed);
    let n_200 = responder.total_200.load(Ordering::Relaxed);

    println!(
        "Chaos results: 429 injected: {}, 503 injected: {}, 200 successes: {} (Total attempts: {})",
        n_429,
        n_503,
        n_200,
        n_429 + n_503 + n_200
    );

    // Verify faults were actually injected
    assert!(n_429 > 0, "Expected at least one 429 error to be injected");
    assert!(n_503 > 0, "Expected at least one 503 error to be injected");

    // Verify 100% clean recovery with ZERO leaked permits across all accounts
    for acc in &accounts {
        let acc_limiter = limiter.get_or_create_account(acc);
        assert_eq!(
            acc_limiter.current_concurrency(),
            0,
            "Permit leak detected on account {acc} after chaos"
        );
    }

    println!(
        "Chaos test PASSED: 100% of {} calls succeeded in {:?} with 0 permit leaks.",
        num_requests, elapsed
    );
}
