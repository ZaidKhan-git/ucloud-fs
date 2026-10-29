use crate::error::VfsError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

pub const DEFAULT_GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const DEFAULT_GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// PKCE code verifier and S256 challenge.
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Generates a cryptographic PKCE code verifier and S256 challenge.
#[must_use]
pub fn generate_pkce() -> PkcePair {
    let mut random_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    PkcePair {
        verifier,
        challenge,
    }
}

/// Generates a random 32-character CSRF state token.
#[must_use]
pub fn generate_csrf_state() -> String {
    let mut random_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    URL_SAFE_NO_PAD.encode(random_bytes)
}

/// Google OAuth token response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub token_type: Option<String>,
}

/// Starts an ephemeral loopback HTTP listener on `127.0.0.1:0` to capture OAuth redirect code.
pub async fn capture_loopback_code(
    listener: TcpListener,
    expected_state: &str,
    timeout_duration: Duration,
) -> Result<String, VfsError> {
    let accept_task = async {
        let (mut socket, _) = listener.accept().await.map_err(|e| {
            VfsError::InternalFault(format!("Failed to accept loopback connection: {}", e))
        })?;

        let mut buffer = [0u8; 2048];
        let bytes_read = socket.read(&mut buffer).await.map_err(|e| {
            VfsError::InternalFault(format!("Failed to read HTTP request: {}", e))
        })?;

        let request_str = std::str::from_utf8(&buffer[..bytes_read]).map_err(|e| {
            VfsError::InternalFault(format!("Malformed UTF-8 in HTTP request: {}", e))
        })?;

        // Extract query string from GET request line: "GET /?code=...&state=... HTTP/1.1"
        let first_line = request_str.lines().next().unwrap_or("");
        let query_path = first_line.split_whitespace().nth(1).unwrap_or("");

        let query = query_path.split('?').nth(1).unwrap_or("");
        let mut code: Option<String> = None;
        let mut state: Option<String> = None;

        for param in query.split('&') {
            let mut parts = param.split('=');
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                if k == "code" {
                    code = Some(v.to_string());
                } else if k == "state" {
                    state = Some(v.to_string());
                }
            }
        }

        if state.as_deref() != Some(expected_state) {
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h3>CSRF State Mismatch</h3>";
            let _ = socket.write_all(resp.as_bytes()).await;
            return Err(VfsError::PermissionDenied(
                "CSRF state validation failed".to_string(),
            ));
        }

        let auth_code = code.ok_or_else(|| {
            VfsError::PermissionDenied("Missing authorization code in OAuth redirect".to_string())
        })?;

        let success_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
            <html><body style='font-family:sans-serif;text-align:center;padding:50px;'>\
            <h2>Authentication Successful!</h2>\
            <p>You may now close this window and return to the application.</p>\
            </body></html>";

        let _ = socket.write_all(success_resp.as_bytes()).await;
        let _ = socket.flush().await;

        Ok(auth_code)
    };

    match tokio::time::timeout(timeout_duration, accept_task).await {
        Ok(res) => res,
        Err(_) => Err(VfsError::RateLimitTimeout),
    }
}

/// Exchanges an authorization code for access and refresh tokens.
pub async fn exchange_code_for_tokens(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokenResponse, VfsError> {
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ];

    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|_| VfsError::NetworkOffline)?;

    if !response.status().is_success() {
        return Err(VfsError::PermissionDenied(format!(
            "OAuth token exchange failed with status {}",
            response.status()
        )));
    }

    response
        .json::<OAuthTokenResponse>()
        .await
        .map_err(|e| VfsError::InternalFault(format!("Failed to parse token response: {}", e)))
}

/// Refreshes an expired access token using the stored refresh token.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<OAuthTokenResponse, VfsError> {
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];

    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|_| VfsError::NetworkOffline)?;

    if !response.status().is_success() {
        return Err(VfsError::PermissionDenied(format!(
            "OAuth token refresh failed with status {}",
            response.status()
        )));
    }

    response
        .json::<OAuthTokenResponse>()
        .await
        .map_err(|e| VfsError::InternalFault(format!("Failed to parse refresh response: {}", e)))
}

use crate::security::vault::AccountCredentials;

/// Single-flight token coordinator preventing thundering herd refresh storms.
pub struct AccountTokenManager {
    pub account_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub token_url: String,
    pub client: reqwest::Client,
    tokens: RwLock<TokenState>,
}

struct TokenState {
    refresh_token: String,
    access_token: String,
    expires_at_unix: u64,
}

impl AccountTokenManager {
    #[must_use]
    pub fn new(
        account_id: String,
        client_id: String,
        client_secret: String,
        token_url: String,
        creds: AccountCredentials,
        client: reqwest::Client,
    ) -> Self {
        Self {
            account_id,
            client_id,
            client_secret,
            token_url,
            client,
            tokens: RwLock::new(TokenState {
                refresh_token: creds.refresh_token.clone(),
                access_token: creds.access_token.clone(),
                expires_at_unix: creds.expires_at_unix,
            }),
        }
    }

    /// Fetches a valid access token. If token is within 300 seconds of expiry or expired,
    /// performs a single-flight refresh while holding back other concurrent callers.
    pub async fn get_valid_access_token(&self) -> Result<String, VfsError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| VfsError::InternalFault("System clock before Unix Epoch".to_string()))?
            .as_secs();

        // 1. Fast path: check under read lock
        {
            let read_guard = self.tokens.read().await;
            if read_guard.expires_at_unix > (now + 300) {
                return Ok(read_guard.access_token.clone());
            }
        }

        // 2. Slow path: acquire exclusive write lock to prevent thundering herd
        let mut write_guard = self.tokens.write().await;

        // Double-check: another task might have completed the refresh while we waited
        if write_guard.expires_at_unix > (now + 300) {
            return Ok(write_guard.access_token.clone());
        }

        let resp = refresh_access_token(
            &self.client,
            &self.token_url,
            &self.client_id,
            &self.client_secret,
            &write_guard.refresh_token,
        )
        .await?;

        write_guard.access_token = resp.access_token.clone();
        write_guard.expires_at_unix = now + resp.expires_in;

        if let Some(new_refresh) = resp.refresh_token {
            write_guard.refresh_token = new_refresh;
        }

        Ok(resp.access_token)
    }
}
