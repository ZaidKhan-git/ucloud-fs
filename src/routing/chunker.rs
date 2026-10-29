use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::error::VfsError;
use crate::network::client::{GoogleDriveClient, GoogleFileInput};
use crate::routing::inode::{NewFileParams, StorageLocation};
use crate::routing::placement::StripedReservationGuard;
use crate::routing::InMemoryRoutingTable;

/// Completed chunk record included in manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UploadedChunk {
    pub chunk_index: usize,
    pub account_id: String,
    pub google_file_id: String,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub sha256: String,
}

/// JSON manifest payload written to cloud for striped files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileManifest {
    pub manifest_id: String,
    pub filename: String,
    pub total_size: u64,
    pub overall_sha256: String,
    pub chunks: Vec<UploadedChunk>,
}

/// Chunker engine orchestrating multi-account uploads and transactional rollback.
pub struct OversizedFileChunker {
    client: Arc<GoogleDriveClient>,
    routing_table: Arc<InMemoryRoutingTable>,
}

impl OversizedFileChunker {
    /// Creates a new `OversizedFileChunker`.
    #[must_use]
    pub fn new(
        client: Arc<GoogleDriveClient>,
        routing_table: Arc<InMemoryRoutingTable>,
    ) -> Self {
        Self {
            client,
            routing_table,
        }
    }

    /// Generates human-readable text for the companion warning note.
    #[must_use]
    pub fn generate_warning_note_content(filename: &str, chunks: &[UploadedChunk]) -> String {
        let mut account_counts: HashMap<&str, usize> = HashMap::new();
        for c in chunks {
            *account_counts.entry(&c.account_id).or_insert(0) += 1;
        }

        let mut summary = String::new();
        for (acc, count) in account_counts {
            summary.push_str(&format!("- Account '{acc}': holds {count} chunk(s)\n"));
        }

        format!(
            "IMPORTANT NOTICE REGARDING '{filename}'\n\
            ====================================================================\n\
            This file exceeded the storage quota of any single Google account and was\n\
            automatically split into 16 MB chunks across your linked accounts:\n\n\
            {summary}\n\
            IMPORTANT RESTRICTIONS:\n\
            1. You CANNOT open, view, or play this file directly on drive.google.com.\n\
            2. To access this file, download, open, or copy it out directly through this\n\
               local ucloud-fs virtual drive, which stitches all chunks seamlessly on-the-fly.\n\
            ===================================================================="
        )
    }

    /// Uploads an oversized file across accounts according to `plan`, with transactional rollback.
    ///
    /// If any chunk upload fails, all previously uploaded chunks for this manifest are deleted
    /// from Google's servers before returning an error.
    pub async fn upload_striped_file(
        &self,
        parent_ino: u64,
        mut guard: StripedReservationGuard,
        stream_bytes: &[u8],
        account_tokens: &HashMap<String, String>,
    ) -> Result<u64, VfsError> {
        let plan = guard.plan.clone();
        info!(
            filename = %plan.filename,
            total_size = plan.total_size,
            manifest_id = %plan.manifest_id,
            chunks_count = plan.chunks.len(),
            "Starting multi-account striped upload"
        );

        let mut uploaded_chunks: Vec<UploadedChunk> = Vec::with_capacity(plan.chunks.len());
        let mut overall_hasher = Sha256::new();

        // 1. Upload chunks sequentially or concurrently per plan
        for chunk_assign in &plan.chunks {
            let start = chunk_assign.byte_offset as usize;
            let end = start + (chunk_assign.byte_len as usize);

            if end > stream_bytes.len() {
                self.rollback_uploaded_chunks(&uploaded_chunks, account_tokens)
                    .await;
                return Err(VfsError::InternalFault(
                    "Stream ended prematurely before expected chunk boundary".to_string(),
                ));
            }

            let chunk_slice = &stream_bytes[start..end];
            let chunk_bytes = Bytes::copy_from_slice(chunk_slice);

            // Compute chunk SHA-256
            let mut chunk_hasher = Sha256::new();
            chunk_hasher.update(&chunk_bytes);
            let chunk_sha256 = format!("{:x}", chunk_hasher.finalize());

            overall_hasher.update(&chunk_bytes);

            let token = match account_tokens.get(&chunk_assign.account_id) {
                Some(t) => t,
                None => {
                    self.rollback_uploaded_chunks(&uploaded_chunks, account_tokens)
                        .await;
                    return Err(VfsError::PermissionDenied(format!(
                        "Missing OAuth token for account '{}'",
                        chunk_assign.account_id
                    )));
                }
            };

            // Prepare chunk appProperties metadata
            let mut app_props = HashMap::new();
            app_props.insert("ucloud_chunk".to_string(), "true".to_string());
            app_props.insert("parent_manifest_id".to_string(), plan.manifest_id.clone());
            app_props.insert(
                "chunk_index".to_string(),
                chunk_assign.chunk_index.to_string(),
            );

            let chunk_file_name = format!(
                ".ucloud_chunk_{}_{:05}",
                plan.manifest_id, chunk_assign.chunk_index
            );

            let input = GoogleFileInput {
                name: chunk_file_name,
                mime_type: Some("application/octet-stream".to_string()),
                parents: None,
                app_properties: Some(app_props),
            };

            // Upload chunk via Resumable Upload
            let upload_result = self
                .upload_single_chunk(
                    &chunk_assign.account_id,
                    token,
                    &input,
                    chunk_bytes,
                    chunk_assign.byte_len,
                )
                .await;

            match upload_result {
                Ok(google_file_id) => {
                    uploaded_chunks.push(UploadedChunk {
                        chunk_index: chunk_assign.chunk_index,
                        account_id: chunk_assign.account_id.clone(),
                        google_file_id,
                        byte_offset: chunk_assign.byte_offset,
                        byte_len: chunk_assign.byte_len,
                        sha256: chunk_sha256,
                    });
                }
                Err(err) => {
                    error!(
                        error = %err,
                        chunk_index = chunk_assign.chunk_index,
                        manifest_id = %plan.manifest_id,
                        "Chunk upload failed! Initiating transactional rollback"
                    );
                    self.rollback_uploaded_chunks(&uploaded_chunks, account_tokens)
                        .await;
                    return Err(err);
                }
            }
        }

        let overall_sha256 = format!("{:x}", overall_hasher.finalize());

        // 2. Upload manifest JSON to the assigned manifest account
        let manifest = FileManifest {
            manifest_id: plan.manifest_id.clone(),
            filename: plan.filename.clone(),
            total_size: plan.total_size,
            overall_sha256,
            chunks: uploaded_chunks.clone(),
        };

        let manifest_json = serde_json::to_vec(&manifest).map_err(|e| {
            VfsError::InternalFault(format!("Failed to serialize manifest JSON: {e}"))
        })?;

        let manifest_token = match account_tokens.get(&plan.manifest_account_id) {
            Some(t) => t,
            None => {
                self.rollback_uploaded_chunks(&uploaded_chunks, account_tokens)
                    .await;
                return Err(VfsError::PermissionDenied(format!(
                    "Missing token for manifest account '{}'",
                    plan.manifest_account_id
                )));
            }
        };

        let manifest_filename = format!("{}.ucloud_manifest.json", plan.filename);
        let manifest_input = GoogleFileInput {
            name: manifest_filename,
            mime_type: Some("application/json".to_string()),
            parents: None,
            app_properties: None,
        };

        let manifest_file_id = match self
            .upload_single_chunk(
                &plan.manifest_account_id,
                manifest_token,
                &manifest_input,
                Bytes::from(manifest_json),
                manifest_input.name.len() as u64,
            )
            .await
        {
            Ok(fid) => fid,
            Err(e) => {
                error!("Manifest upload failed; executing rollback: {e}");
                self.rollback_uploaded_chunks(&uploaded_chunks, account_tokens)
                    .await;
                return Err(e);
            }
        };

        // 3. Commit quota reservations (transfer in-flight reservation to permanent)
        guard.commit();

        // 4. Insert virtual striped inode into in-memory routing table
        let striped_location = StorageLocation::StripedManifest {
            manifest_id: plan.manifest_id.clone(),
            manifest_account_id: plan.manifest_account_id.clone(),
            manifest_google_file_id: manifest_file_id,
        };

        let unified_ino = self.routing_table.insert_file(NewFileParams {
            parent_ino,
            name: &plan.filename,
            size: plan.total_size,
            mime_type: "application/octet-stream",
            modified_time: 0,
            created_time: 0,
            location: striped_location,
            web_view_link: None,
        })?;

        // Register each chunk cloud ID to the unified inode
        for chunk in &uploaded_chunks {
            self.routing_table.register_chunk_cloud_id(
                &chunk.account_id,
                &chunk.google_file_id,
                unified_ino,
            );
        }

        // 5. Insert companion warning note into the same directory
        let warning_filename = format!("{}.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt", plan.filename);
        let warning_content = Self::generate_warning_note_content(&plan.filename, &uploaded_chunks);
        let warning_bytes_len = warning_content.len() as u64;

        let warning_location = StorageLocation::VirtualWarningNote {
            parent_striped_file_name: plan.filename.clone(),
            content: warning_content,
        };

        let _ = self.routing_table.insert_file(NewFileParams {
            parent_ino,
            name: &warning_filename,
            size: warning_bytes_len,
            mime_type: "text/plain",
            modified_time: 0,
            created_time: 0,
            location: warning_location,
            web_view_link: None,
        })?;

        info!(
            filename = %plan.filename,
            unified_ino,
            "Successfully created virtual striped file and companion warning note"
        );

        Ok(unified_ino)
    }

    /// Helper to upload a single chunk or manifest using Resumable Upload.
    async fn upload_single_chunk(
        &self,
        account_id: &str,
        access_token: &str,
        input: &GoogleFileInput,
        chunk_data: Bytes,
        chunk_len: u64,
    ) -> Result<String, VfsError> {
        let session_url = self
            .client
            .initiate_resumable_upload(account_id, access_token, input)
            .await?;

        let res = self
            .client
            .upload_resumable_chunk(
                account_id,
                &session_url,
                chunk_data,
                0,
                chunk_len.saturating_sub(1),
                chunk_len,
            )
            .await?;

        match res {
            crate::network::client::ChunkUploadResult::Complete { file_id } => Ok(file_id),
            crate::network::client::ChunkUploadResult::Incomplete { .. } => {
                Err(VfsError::InternalFault(
                    "Unexpected 308 on single complete chunk upload".to_string(),
                ))
            }
        }
    }

    /// Transactional rollback: deletes all already-uploaded chunks if an upload was aborted.
    pub async fn rollback_uploaded_chunks(
        &self,
        chunks: &[UploadedChunk],
        tokens: &HashMap<String, String>,
    ) {
        warn!(
            chunks_to_delete = chunks.len(),
            "Executing transactional rollback: deleting uploaded chunks from cloud"
        );

        for chunk in chunks {
            if let Some(token) = tokens.get(&chunk.account_id) {
                let _ = self
                    .client
                    .delete_file(&chunk.account_id, token, &chunk.google_file_id)
                    .await;
            }
        }
    }
}
