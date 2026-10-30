use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rand::Rng;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, CONTENT_TYPE, RANGE, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::error::VfsError;
use crate::network::limiter::{MultiTierLimiter, QuotaCost};

/// Configuration for exponential backoff and jitter.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    /// Base delay in milliseconds for exponential backoff (e.g. 500ms).
    pub base_ms: u64,
    /// Maximum backoff ceiling in milliseconds (e.g. 10,000ms).
    pub max_backoff_ms: u64,
    /// Maximum retry attempts before giving up.
    pub max_attempts: u32,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            base_ms: 500,
            max_backoff_ms: 10_000,
            max_attempts: 5,
        }
    }
}

/// Google Drive v3 `about.get` response model.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleAbout {
    #[serde(default)]
    pub storage_quota: Option<GoogleStorageQuota>,
    #[serde(default)]
    pub user: Option<GoogleUser>,
}

/// Google Drive storage quota metrics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleStorageQuota {
    #[serde(default)]
    pub limit: Option<String>,
    #[serde(default)]
    pub usage: Option<String>,
    #[serde(default)]
    pub usage_in_drive: Option<String>,
    #[serde(default)]
    pub usage_in_drive_trash: Option<String>,
}

/// Google user profile info.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleUser {
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Google Drive `changes.getStartPageToken` response model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleStartPageToken {
    pub start_page_token: String,
}

/// Google Drive `files.list` response model.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFileList {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub files: Vec<GoogleFileMetadata>,
}

/// Partial-field file metadata model returned from Google Drive v3 API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFileMetadata {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub parents: Option<Vec<String>>,
    #[serde(default)]
    pub trashed: Option<bool>,
    #[serde(default)]
    pub app_properties: Option<HashMap<String, String>>,
}

/// Google Drive `changes.list` response model for delta sync.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChangesList {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub new_start_page_token: Option<String>,
    #[serde(default)]
    pub changes: Vec<GoogleChangeItem>,
}

/// Individual change entry in `changes.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChangeItem {
    pub file_id: String,
    #[serde(default)]
    pub removed: Option<bool>,
    #[serde(default)]
    pub file: Option<GoogleFileMetadata>,
}

/// Input metadata payload for initiating resumable uploads or creating files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFileInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_properties: Option<HashMap<String, String>>,
}

/// Result of uploading a resumable file chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkUploadResult {
    /// Chunk accepted, upload incomplete (HTTP 308 Range header parsed).
    Incomplete { range_end: u64 },
    /// Final chunk accepted, file created on remote (HTTP 200/201).
    Complete { file_id: String },
}

/// Consolidated account state hydrated during login.
#[derive(Debug, Clone)]
pub struct HydratedAccountData {
    pub account_id: String,
    pub user_email: Option<String>,
    pub storage_limit_bytes: u64,
    pub storage_used_bytes: u64,
    pub start_page_token: String,
    pub files: Vec<GoogleFileMetadata>,
}

/// High-performance, resilient Google Drive v3 HTTP/2 client.
pub struct GoogleDriveClient {
    client: Arc<Client>,
    base_url: String,
    upload_base_url: String,
    limiter: Arc<MultiTierLimiter>,
    backoff_config: BackoffConfig,
}

impl GoogleDriveClient {
    /// Default Google Drive v3 API endpoints.
    pub const DEFAULT_BASE_URL: &'static str = "https://www.googleapis.com";
    pub const DEFAULT_UPLOAD_BASE_URL: &'static str = "https://www.googleapis.com";

    /// Creates a new `GoogleDriveClient` sharing a single connection pool and adaptive HTTP/2 window.
    pub fn new(limiter: Arc<MultiTierLimiter>) -> Result<Self, VfsError> {
        Self::with_endpoints(
            limiter,
            Self::DEFAULT_BASE_URL.to_string(),
            Self::DEFAULT_UPLOAD_BASE_URL.to_string(),
            BackoffConfig::default(),
        )
    }

    /// Creates a client with custom endpoints and backoff config (ideal for WireMock tests).
    pub fn with_endpoints(
        limiter: Arc<MultiTierLimiter>,
        base_url: String,
        upload_base_url: String,
        backoff_config: BackoffConfig,
    ) -> Result<Self, VfsError> {
        let client = Client::builder()
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Some(Duration::from_secs(20)))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build()
            .map_err(|e| VfsError::InternalFault(format!("Failed to build reqwest client: {e}")))?;

        Ok(Self {
            client: Arc::new(client),
            base_url,
            upload_base_url,
            limiter,
            backoff_config,
        })
    }

    /// Returns a reference to the underlying rate limiter.
    #[must_use]
    pub fn limiter(&self) -> &Arc<MultiTierLimiter> {
        &self.limiter
    }

    /// Computes sleep duration with full jitter or parses `Retry-After`.
    fn calculate_backoff(&self, response: Option<&Response>, attempt: u32) -> Duration {
        if let Some(secs) = response.and_then(|resp| {
            resp.headers()
                .get(RETRY_AFTER)
                .and_then(|val| val.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        }) {
            return Duration::from_secs(secs);
        }

        // Full jitter backoff: sleep = rand(0, min(max_backoff, base * 2^attempt))
        let shift = attempt.min(10);
        let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let exp = self.backoff_config.base_ms.saturating_mul(factor);
        let ceiling = exp.min(self.backoff_config.max_backoff_ms);

        let sleep_ms = if ceiling == 0 {
            0
        } else {
            rand::thread_rng().gen_range(0..=ceiling)
        };

        Duration::from_millis(sleep_ms)
    }

    /// Dispatches an HTTP request with rate limit admission, adaptive jitter backoff, and 429/5xx retry.
    pub async fn execute_request<F, Fut>(
        &self,
        account_id: &str,
        cost: QuotaCost,
        build_and_send: F,
    ) -> Result<Response, VfsError>
    where
        F: Fn(Arc<Client>) -> Fut,
        Fut: std::future::Future<Output = Result<Response, reqwest::Error>>,
    {
        let mut attempt = 0;
        loop {
            // 1. Acquire admission budget (holds concurrency permit until dropped)
            let permit_guard = self.limiter.acquire_budget(account_id, cost).await?;

            // 2. Dispatch the network call
            let result = build_and_send(Arc::clone(&self.client)).await;

            match result {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() || status.as_u16() == 308 {
                        // Success or Resumable Upload 308 Resume Incomplete:
                        // permit guard drops when returned or consumed
                        return Ok(response);
                    }

                    let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error();

                    if is_retryable && attempt < self.backoff_config.max_attempts {
                        let backoff = self.calculate_backoff(Some(&response), attempt);
                        warn!(
                            account = %account_id,
                            status = %status,
                            attempt = attempt + 1,
                            backoff_ms = backoff.as_millis(),
                            "Google API throttled or error; tripping cooldown and backing off"
                        );

                        // Trip account cooldown so concurrent tasks pause
                        self.limiter.trip_account_cooldown(account_id, backoff).await;

                        // Explicitly drop permit guard before sleeping to free concurrency slot
                        drop(permit_guard);

                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    // Explicitly drop guard on terminal failure
                    drop(permit_guard);

                    let err_body = response.text().await.unwrap_or_default();
                    return match status {
                        StatusCode::NOT_FOUND => Err(VfsError::NotFound(format!(
                            "Google Drive resource not found ({status}): {err_body}"
                        ))),
                        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                            Err(VfsError::PermissionDenied(format!(
                                "Google Drive auth/quota error ({status}): {err_body}"
                            )))
                        }
                        _ => Err(VfsError::InternalFault(format!(
                            "Google Drive API error ({status}): {err_body}"
                        ))),
                    };
                }
                Err(err) => {
                    if attempt < self.backoff_config.max_attempts {
                        let backoff = self.calculate_backoff(None, attempt);
                        warn!(
                            account = %account_id,
                            error = %err,
                            attempt = attempt + 1,
                            backoff_ms = backoff.as_millis(),
                            "Google API network transport fault; backing off"
                        );

                        self.limiter.trip_account_cooldown(account_id, backoff).await;
                        drop(permit_guard);

                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    drop(permit_guard);
                    return Err(VfsError::NetworkOffline);
                }
            }
        }
    }

    /// Fetches `about.get?fields=storageQuota,user` for an account.
    pub async fn get_about(&self, account_id: &str, access_token: &str) -> Result<GoogleAbout, VfsError> {
        let url = format!("{}/drive/v3/about?fields=storageQuota,user", self.base_url);
        let token_header = format!("Bearer {access_token}");

        let resp = self
            .execute_request(account_id, QuotaCost::AboutGet, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .get(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        resp.json::<GoogleAbout>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse about.get JSON: {e}")))
    }

    /// Fetches `changes.getStartPageToken?fields=startPageToken` for an account.
    pub async fn get_start_page_token(
        &self,
        account_id: &str,
        access_token: &str,
    ) -> Result<String, VfsError> {
        let url = format!(
            "{}/drive/v3/changes/startPageToken?fields=startPageToken",
            self.base_url
        );
        let token_header = format!("Bearer {access_token}");

        let resp = self
            .execute_request(account_id, QuotaCost::AboutGet, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .get(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        let body = resp
            .json::<GoogleStartPageToken>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse startPageToken: {e}")))?;

        Ok(body.start_page_token)
    }

    /// Queries `files.list` with minimal fields mask and pagination token.
    pub async fn list_files_page(
        &self,
        account_id: &str,
        access_token: &str,
        page_token: Option<&str>,
        page_size: u32,
    ) -> Result<GoogleFileList, VfsError> {
        let fields = "nextPageToken,files(id,name,mimeType,size,modifiedTime,parents,trashed,appProperties)";
        let mut url = format!(
            "{}/drive/v3/files?pageSize={page_size}&fields={fields}&q=trashed=false",
            self.base_url
        );
        if let Some(token) = page_token {
            url.push_str("&pageToken=");
            url.push_str(token);
        }

        let token_header = format!("Bearer {access_token}");

        let resp = self
            .execute_request(account_id, QuotaCost::FilesList, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .get(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        resp.json::<GoogleFileList>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse files.list JSON: {e}")))
    }

    /// Queries `changes.list` for real-time delta polling.
    pub async fn list_changes(
        &self,
        account_id: &str,
        access_token: &str,
        page_token: &str,
    ) -> Result<GoogleChangesList, VfsError> {
        let fields = "newStartPageToken,nextPageToken,changes(fileId,removed,file(id,name,mimeType,size,trashed,parents,modifiedTime,appProperties))";
        let url = format!(
            "{}/drive/v3/changes?pageToken={page_token}&includeItemsFromAllDrives=false&fields={fields}",
            self.base_url
        );
        let token_header = format!("Bearer {access_token}");

        let resp = self
            .execute_request(account_id, QuotaCost::ChangesList, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .get(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        resp.json::<GoogleChangesList>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse changes.list: {e}")))
    }

    /// Fetches file metadata by ID with hardcoded minimal fields mask.
    pub async fn get_file_metadata(
        &self,
        account_id: &str,
        access_token: &str,
        file_id: &str,
    ) -> Result<GoogleFileMetadata, VfsError> {
        let fields = "id,name,mimeType,size,modifiedTime,parents,trashed,appProperties";
        let url = format!(
            "{}/drive/v3/files/{}?fields={fields}",
            self.base_url, file_id
        );
        let token_header = format!("Bearer {access_token}");

        let resp = self
            .execute_request(account_id, QuotaCost::FilesGet, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .get(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        resp.json::<GoogleFileMetadata>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse file metadata: {e}")))
    }

    /// Initiates a streaming download for a file's binary content, supporting partial range requests.
    pub async fn download_stream(
        &self,
        account_id: &str,
        access_token: &str,
        file_id: &str,
        byte_range: Option<(u64, u64)>,
    ) -> Result<Response, VfsError> {
        let url = format!("{}/drive/v3/files/{}?alt=media", self.base_url, file_id);
        let token_header = format!("Bearer {access_token}");
        let range_header = byte_range.map(|(start, end)| format!("bytes={start}-{end}"));

        self.execute_request(account_id, QuotaCost::MediaStream, |client| {
            let u = url.clone();
            let auth = token_header.clone();
            let range = range_header.clone();
            async move {
                let mut req = client.get(&u).header(AUTHORIZATION, auth);
                if let Some(r) = range {
                    req = req.header(RANGE, r);
                }
                req.send().await
            }
        })
        .await
    }

    /// Deletes a file on Google Drive (used for rollback of orphaned chunks).
    pub async fn delete_file(
        &self,
        account_id: &str,
        access_token: &str,
        file_id: &str,
    ) -> Result<(), VfsError> {
        let url = format!("{}/drive/v3/files/{}", self.base_url, file_id);
        let token_header = format!("Bearer {access_token}");

        let _ = self
            .execute_request(account_id, QuotaCost::FilesUpdate, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                async move {
                    client
                        .delete(&u)
                        .header(AUTHORIZATION, auth)
                        .send()
                        .await
                }
            })
            .await?;

        Ok(())
    }

    /// Renames a file or folder on Google Drive via PATCH /drive/v3/files/{file_id}.
    pub async fn rename_file(
        &self,
        account_id: &str,
        access_token: &str,
        file_id: &str,
        new_name: &str,
    ) -> Result<(), VfsError> {
        let url = format!("{}/drive/v3/files/{}", self.base_url, file_id);
        let token_header = format!("Bearer {access_token}");
        let body = serde_json::json!({ "name": new_name });

        let _ = self
            .execute_request(account_id, QuotaCost::FilesUpdate, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                let b = body.clone();
                async move {
                    client
                        .patch(&u)
                        .header(AUTHORIZATION, auth)
                        .header(CONTENT_TYPE, "application/json")
                        .json(&b)
                        .send()
                        .await
                }
            })
            .await?;

        Ok(())
    }

    /// Creates a new directory/folder on Google Drive.
    pub async fn create_folder(
        &self,
        account_id: &str,
        access_token: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<String, VfsError> {
        let url = format!("{}/drive/v3/files", self.base_url);
        let token_header = format!("Bearer {access_token}");

        let metadata = GoogleFileInput {
            name: name.to_string(),
            mime_type: Some("application/vnd.google-apps.folder".to_string()),
            parents: parent_id.map(|pid| vec![pid.to_string()]),
            app_properties: None,
        };

        let body = serde_json::to_vec(&metadata).map_err(|e| {
            VfsError::InternalFault(format!("Failed to serialize folder metadata: {e}"))
        })?;

        let resp = self
            .execute_request(account_id, QuotaCost::FilesCreate, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                let b = body.clone();
                async move {
                    client
                        .post(&u)
                        .header(AUTHORIZATION, auth)
                        .header("Content-Type", "application/json; charset=UTF-8")
                        .body(b)
                        .send()
                        .await
                }
            })
            .await?;

        #[derive(serde::Deserialize)]
        struct FolderResponse {
            id: String,
        }

        let folder = resp
            .json::<FolderResponse>()
            .await
            .map_err(|e| VfsError::InternalFault(format!("Failed to parse folder creation response: {e}")))?;

        Ok(folder.id)
    }

    /// Initiates a Resumable Upload session for a file or chunk.
    pub async fn initiate_resumable_upload(
        &self,
        account_id: &str,
        access_token: &str,
        metadata: &GoogleFileInput,
    ) -> Result<String, VfsError> {
        let url = format!(
            "{}/upload/drive/v3/files?uploadType=resumable",
            self.upload_base_url
        );
        let token_header = format!("Bearer {access_token}");
        let body = serde_json::to_vec(metadata).map_err(|e| {
            VfsError::InternalFault(format!("Failed to serialize upload metadata: {e}"))
        })?;

        let resp = self
            .execute_request(account_id, QuotaCost::FilesCreate, |client| {
                let u = url.clone();
                let auth = token_header.clone();
                let b = body.clone();
                async move {
                    client
                        .post(&u)
                        .header(AUTHORIZATION, auth)
                        .header("Content-Type", "application/json; charset=UTF-8")
                        .body(b)
                        .send()
                        .await
                }
            })
            .await?;

        let location = resp
            .headers()
            .get("Location")
            .ok_or_else(|| {
                VfsError::InternalFault("Missing Location header in resumable upload init".to_string())
            })?
            .to_str()
            .map_err(|_| {
                VfsError::InternalFault("Invalid Location header string".to_string())
            })?;

        Ok(location.to_string())
    }

    /// Uploads a binary chunk with an optional total size (using `*` for in-flight streams).
    pub async fn upload_resumable_chunk_opt(
        &self,
        account_id: &str,
        session_uri: &str,
        chunk: Bytes,
        start_byte: u64,
        end_byte: u64,
        total_size: Option<u64>,
    ) -> Result<ChunkUploadResult, VfsError> {
        let total_str = match total_size {
            Some(tot) => tot.to_string(),
            None => "*".to_string(),
        };
        let content_range = format!("bytes {start_byte}-{end_byte}/{total_str}");
        let session_url = session_uri.to_string();

        let resp = self
            .execute_request(account_id, QuotaCost::MediaStream, |client| {
                let u = session_url.clone();
                let range = content_range.clone();
                let body = chunk.clone();
                async move {
                    client
                        .put(&u)
                        .header(CONTENT_RANGE, range)
                        .body(body)
                        .send()
                        .await
                }
            })
            .await?;

        let status = resp.status();
        if status.as_u16() == 308 {
            // Resume Incomplete - parse Range header if present
            let range_end = if let Some(val) = resp.headers().get(RANGE) {
                if let Ok(s) = val.to_str() {
                    s.split('-')
                        .nth(1)
                        .and_then(|e| e.parse::<u64>().ok())
                        .unwrap_or(end_byte)
                } else {
                    end_byte
                }
            } else {
                end_byte
            };

            Ok(ChunkUploadResult::Incomplete { range_end })
        } else if status.is_success() {
            // Final chunk complete - parse returned file metadata ID
            let meta = resp.json::<GoogleFileMetadata>().await.map_err(|e| {
                VfsError::InternalFault(format!("Failed to parse completed upload metadata: {e}"))
            })?;

            Ok(ChunkUploadResult::Complete { file_id: meta.id })
        } else {
            Err(VfsError::InternalFault(format!(
                "Unexpected chunk upload status: {status}"
            )))
        }
    }

    /// Uploads a single binary chunk to an active resumable upload session URI.
    pub async fn upload_resumable_chunk(
        &self,
        account_id: &str,
        session_uri: &str,
        chunk: Bytes,
        start_byte: u64,
        end_byte: u64,
        total_size: u64,
    ) -> Result<ChunkUploadResult, VfsError> {
        self.upload_resumable_chunk_opt(
            account_id,
            session_uri,
            chunk,
            start_byte,
            end_byte,
            Some(total_size),
        )
        .await
    }

    /// Fully hydrates a single account on login: fetches quota, startPageToken, and paginates all files.
    pub async fn hydrate_account(
        &self,
        account_id: &str,
        access_token: &str,
    ) -> Result<HydratedAccountData, VfsError> {
        info!(account = %account_id, "Beginning parallel account hydration");

        // 1. Fetch quota and startPageToken concurrently
        let about_fut = self.get_about(account_id, access_token);
        let token_fut = self.get_start_page_token(account_id, access_token);
        let (about, start_page_token) = tokio::try_join!(about_fut, token_fut)?;

        let (limit, used) = if let Some(q) = about.storage_quota {
            let limit_num = q
                .limit
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(15 * 1024 * 1024 * 1024); // 15 GB default
            let used_num = q
                .usage
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            (limit_num, used_num)
        } else {
            (15 * 1024 * 1024 * 1024, 0)
        };

        let user_email = about.user.and_then(|u| u.email_address);

        // 2. Paginate files.list until all files are retrieved
        let mut all_files = Vec::new();
        let mut next_page: Option<String> = None;

        loop {
            let page = self
                .list_files_page(account_id, access_token, next_page.as_deref(), 1000)
                .await?;

            all_files.extend(page.files);

            match page.next_page_token {
                Some(tok) if !tok.is_empty() => {
                    next_page = Some(tok);
                }
                _ => break,
            }
        }

        info!(
            account = %account_id,
            files_count = all_files.len(),
            used_mb = used / (1024 * 1024),
            limit_mb = limit / (1024 * 1024),
            "Account hydration complete"
        );

        Ok(HydratedAccountData {
            account_id: account_id.to_string(),
            user_email,
            storage_limit_bytes: limit,
            storage_used_bytes: used,
            start_page_token,
            files: all_files,
        })
    }

    /// Hydrates multiple accounts in parallel using Tokio tasks.
    pub async fn hydrate_all_accounts(
        self: &Arc<Self>,
        accounts: &[(String, String)],
    ) -> Result<Vec<HydratedAccountData>, VfsError> {
        let mut set = JoinSet::new();

        for (acc_id, token) in accounts {
            let client = Arc::clone(self);
            let id = acc_id.clone();
            let tok = token.clone();
            set.spawn(async move { client.hydrate_account(&id, &tok).await });
        }

        let mut results = Vec::with_capacity(accounts.len());
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(data)) => results.push(data),
                Ok(Err(vfs_err)) => return Err(vfs_err),
                Err(join_err) => {
                    return Err(VfsError::InternalFault(format!(
                        "Hydration task panic or cancellation: {join_err}"
                    )))
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_client_about_and_start_page_token() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/drive/v3/about"))
            .and(query_param("fields", "storageQuota,user"))
            .and(header("authorization", "Bearer test_tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "storageQuota": {
                    "limit": "16106127360",
                    "usage": "5368709120"
                },
                "user": {
                    "emailAddress": "user@example.com",
                    "displayName": "Test User"
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/drive/v3/changes/startPageToken"))
            .and(query_param("fields", "startPageToken"))
            .and(header("authorization", "Bearer test_tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "startPageToken": "token_12345"
            })))
            .mount(&server)
            .await;

        let limiter = Arc::new(MultiTierLimiter::new());
        let client = GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .unwrap();

        let about = client.get_about("acc1", "test_tok").await.unwrap();
        assert_eq!(
            about.user.and_then(|u| u.email_address),
            Some("user@example.com".to_string())
        );

        let token = client.get_start_page_token("acc1", "test_tok").await.unwrap();
        assert_eq!(token, "token_12345");
    }

    #[tokio::test]
    async fn test_client_files_list_pagination() {
        let server = MockServer::start().await;

        // Page 1
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .and(query_param("pageSize", "1000"))
            .and(query_param("q", "trashed=false"))
            .and(wiremock::matchers::query_param_is_missing("pageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nextPageToken": "page_2_marker",
                "files": [
                    { "id": "f1", "name": "file1.txt", "mimeType": "text/plain" }
                ]
            })))
            .mount(&server)
            .await;

        // Page 2
        Mock::given(method("GET"))
            .and(path("/drive/v3/files"))
            .and(query_param("pageSize", "1000"))
            .and(query_param("pageToken", "page_2_marker"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nextPageToken": null,
                "files": [
                    { "id": "f2", "name": "file2.txt", "mimeType": "text/plain" }
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/drive/v3/about"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "storageQuota": { "limit": "1000000", "usage": "500000" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/drive/v3/changes/startPageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "startPageToken": "start_tok"
            })))
            .mount(&server)
            .await;

        let limiter = Arc::new(MultiTierLimiter::new());
        let client = GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .unwrap();

        let hydrated = client.hydrate_account("acc_page", "tok").await.unwrap();
        assert_eq!(hydrated.files.len(), 2);
        assert_eq!(hydrated.files[0].name, "file1.txt");
        assert_eq!(hydrated.files[1].name, "file2.txt");
        assert_eq!(hydrated.start_page_token, "start_tok");
        assert_eq!(hydrated.storage_limit_bytes, 1000000);
        assert_eq!(hydrated.storage_used_bytes, 500000);
    }

    #[tokio::test]
    async fn test_client_resumable_upload_workflow() {
        let server = MockServer::start().await;

        let upload_session_path = "/upload_session_abc";
        let session_uri = format!("{}{}", server.uri(), upload_session_path);

        // 1. Resumable upload init
        Mock::given(method("POST"))
            .and(path("/upload/drive/v3/files"))
            .and(query_param("uploadType", "resumable"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("Location", session_uri.as_str()),
            )
            .mount(&server)
            .await;

        // 2. First chunk -> 308 Resume Incomplete
        Mock::given(method("PUT"))
            .and(path(upload_session_path))
            .and(header("content-range", "bytes 0-1023/2048"))
            .respond_with(ResponseTemplate::new(308).insert_header("Range", "bytes=0-1023"))
            .mount(&server)
            .await;

        // 3. Second chunk -> 200 OK Complete
        Mock::given(method("PUT"))
            .and(path(upload_session_path))
            .and(header("content-range", "bytes 1024-2047/2048"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "new_file_id_xyz",
                "name": "large_file.bin",
                "mimeType": "application/octet-stream"
            })))
            .mount(&server)
            .await;

        let limiter = Arc::new(MultiTierLimiter::new());
        let client = GoogleDriveClient::with_endpoints(
            limiter,
            server.uri(),
            server.uri(),
            BackoffConfig::default(),
        )
        .unwrap();

        let input = GoogleFileInput {
            name: "large_file.bin".to_string(),
            mime_type: Some("application/octet-stream".to_string()),
            parents: None,
            app_properties: None,
        };

        let location = client
            .initiate_resumable_upload("acc_upload", "tok", &input)
            .await
            .unwrap();
        assert_eq!(location, session_uri);

        let chunk1 = Bytes::from(vec![0u8; 1024]);
        let res1 = client
            .upload_resumable_chunk("acc_upload", &location, chunk1, 0, 1023, 2048)
            .await
            .unwrap();
        assert_eq!(res1, ChunkUploadResult::Incomplete { range_end: 1023 });

        let chunk2 = Bytes::from(vec![1u8; 1024]);
        let res2 = client
            .upload_resumable_chunk("acc_upload", &location, chunk2, 1024, 2047, 2048)
            .await
            .unwrap();
        assert_eq!(
            res2,
            ChunkUploadResult::Complete {
                file_id: "new_file_id_xyz".to_string()
            }
        );
    }
}
