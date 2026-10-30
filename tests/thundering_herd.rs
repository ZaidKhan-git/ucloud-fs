use std::sync::Arc;
use ucloud_fs::security::oauth::AccountTokenManager;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_thundering_herd_token_refresh_stress() {
    let mock_server = MockServer::start().await;

    // Program the mock server to respond with a refreshed token
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.refreshed_single_flight_access_token",
                "expires_in": 3600,
                "token_type": "Bearer"
            })),
        )
        .expect(1) // Mandatory invariant: exactly 1 HTTP POST call
        .mount(&mock_server)
        .await;

    let client = reqwest::Client::new();
    let creds = ucloud_fs::security::vault::AccountCredentials {
        account_id: "stress_user@gmail.com".to_string(),
        refresh_token: "refresh_token_valid".to_string(),
        access_token: "expired_old_access_token".to_string(),
        expires_at_unix: 0, // Expired at epoch (forces immediate refresh)
        client_id: None,
        client_secret: None,
    };
    let manager = Arc::new(AccountTokenManager::new(
        "stress_user@gmail.com".to_string(),
        "client_id_123".to_string(),
        "client_secret_xyz".to_string(),
        mock_server.uri(),
        creds,
        client,
    ));

    // Spawn 1,000 concurrent tasks requesting an access token simultaneously
    let mut handles = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let mgr = manager.clone();
        handles.push(tokio::spawn(async move {
            mgr.get_valid_access_token().await
        }));
    }

    let mut success_count = 0;
    for h in handles {
        let res = h.await.expect("Task must not panic");
        let token = res.expect("Token retrieval must succeed");
        assert_eq!(token, "ya29.refreshed_single_flight_access_token");
        success_count += 1;
    }

    assert_eq!(success_count, 1_000);

    // Verify mock server received strictly 1 request
    let received = mock_server.received_requests().await.expect("Must fetch received requests");
    assert_eq!(
        received.len(),
        1,
        "Thundering herd failed: expected strictly 1 HTTP refresh call, but got {}",
        received.len()
    );
}
