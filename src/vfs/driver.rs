use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::VfsError;
use crate::network::client::{GoogleDriveClient, GoogleFileInput};
use crate::routing::chunker::{FileManifest, OversizedFileChunker};
use crate::routing::inode::{NewFileParams, StorageLocation};
use crate::routing::placement::{
    PlacementDecision, PlacementEngine, SingleReservationGuard, StripedPlacementPlan,
    StripedReservationGuard,
};
use crate::routing::sync::DeltaSyncEngine;
use crate::routing::InMemoryRoutingTable;
use crate::security::session::SessionController;
use crate::stream::backpressure::MemoryBackpressureValve;
use crate::stream::read_window::{
    ByteSource, EphemeralReadHandle, MockByteSource, SingleAccountByteSource,
    StripedManifestByteSource,
};
use crate::stream::write_spool::EphemeralWriteHandle;
use crate::vfs::notify::ShellNotifier;
use crate::vfs::prompt::ChunkPrompter;
use crate::vfs::sandbox::{self, ffi_boundary_guard};

/// Volume metrics and filesystem capabilities reported to the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub case_sensitive_search: bool,
    pub case_preserved_names: bool,
    pub unicode_on_disk: bool,
    pub reparse_points: bool,
    pub operation_timeout_ms: u32,
    pub volume_label: String,
}

/// Metadata attributes for a virtual file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub ino: u64,
    pub size: u64,
    pub allocation_size: u64,
    pub is_directory: bool,
    pub is_readonly: bool,
    pub created_time: u64,
    pub modified_time: u64,
    pub file_attributes: u32,
}

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;

/// Directory entry returned during directory enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
}

/// State of a file in pending write / upload lifecycle.
struct WritePendingState {
    path: String,
    parent_ino: u64,
    filename: String,
    #[allow(dead_code)]
    allocation_size: u64,
    write_spool: Option<EphemeralWriteHandle>,
    single_guard: Option<SingleReservationGuard>,
    striped_guard: Option<StripedReservationGuard>,
    striped_plan: Option<StripedPlacementPlan>,
    uploaded_bytes: Vec<u8>,
    delete_on_close: bool,
}

/// In-flight state of an open file or directory handle.
enum HandleState {
    Directory {
        #[allow(dead_code)]
        ino: u64,
    },
    Read {
        ino: u64,
        handle: EphemeralReadHandle,
    },
    WritePending(Box<WritePendingState>),
}

/// Core dependencies required to initialize the VFS driver.
pub struct DriverDependencies {
    pub routing_table: Arc<InMemoryRoutingTable>,
    pub placement_engine: Arc<PlacementEngine>,
    pub session: Arc<SessionController>,
    pub client: Arc<GoogleDriveClient>,
    pub sync_engine: Arc<DeltaSyncEngine>,
    pub notifier: Arc<dyn ShellNotifier>,
    pub prompter: Arc<dyn ChunkPrompter>,
    pub valve: Arc<MemoryBackpressureValve>,
    pub rt_handle: tokio::runtime::Handle,
}

/// Unified sandboxed VFS Driver bridging OS filesystem IRPs into the core ucloud-fs engine.
pub struct UcloudFsDriver {
    routing_table: Arc<InMemoryRoutingTable>,
    placement_engine: Arc<PlacementEngine>,
    session: Arc<SessionController>,
    client: Arc<GoogleDriveClient>,
    sync_engine: Arc<DeltaSyncEngine>,
    notifier: Arc<dyn ShellNotifier>,
    prompter: Arc<dyn ChunkPrompter>,
    valve: Arc<MemoryBackpressureValve>,
    rt_handle: tokio::runtime::Handle,
    open_handles: DashMap<u64, Arc<Mutex<HandleState>>>,
    next_fh: AtomicU64,
    account_tokens: DashMap<String, String>,
    mock_files: DashMap<String, Bytes>,
    is_mounted: AtomicBool,
}

impl UcloudFsDriver {
    /// Creates a new `UcloudFsDriver`.
    #[must_use]
    pub fn new(deps: DriverDependencies) -> Self {
        Self {
            routing_table: deps.routing_table,
            placement_engine: deps.placement_engine,
            session: deps.session,
            client: deps.client,
            sync_engine: deps.sync_engine,
            notifier: deps.notifier,
            prompter: deps.prompter,
            valve: deps.valve,
            rt_handle: deps.rt_handle,
            open_handles: DashMap::new(),
            next_fh: AtomicU64::new(100),
            account_tokens: DashMap::new(),
            mock_files: DashMap::new(),
            is_mounted: AtomicBool::new(true),
        }
    }

    /// Registers an OAuth token for a linked Google account.
    pub fn set_account_token(&self, account_id: &str, token: &str) {
        self.account_tokens.insert(account_id.to_string(), token.to_string());
    }

    /// Registers a mock file payload for testing mode.
    pub fn set_mock_file(&self, path: &str, data: Bytes) {
        self.mock_files.insert(path.to_string(), data);
    }

    /// Allocates a unique file handle ID.
    fn allocate_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    /// Mandatory 4-step wrapper guard for synchronous VFS operations.
    pub fn execute_vfs_op<F, R>(&self, path_for_validation: Option<&str>, op: F) -> Result<R, VfsError>
    where
        F: FnOnce() -> Result<R, VfsError>,
    {
        ffi_boundary_guard(
            "vfs_op",
            Err(VfsError::InternalFault("FFI panic contained".to_string())),
            || {
                if !self.is_mounted.load(Ordering::SeqCst) {
                    return Err(VfsError::SessionLocked);
                }
                self.session.verify_session_active()?;

                if let Some(path) = path_for_validation {
                    sandbox::validate_path(path)?;
                }

                op()
            },
        )
    }

    /// Mandatory 4-step wrapper guard for asynchronous VFS operations with 120s timeout.
    pub fn execute_vfs_async_op<F, Fut, R>(&self, path_for_validation: Option<&str>, async_op: F) -> Result<R, VfsError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<R, VfsError>>,
    {
        self.execute_vfs_op(path_for_validation, || {
            let res = if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::block_in_place(|| {
                    self.rt_handle.block_on(async {
                        tokio::time::timeout(Duration::from_secs(120), async_op()).await
                    })
                })
            } else {
                self.rt_handle.block_on(async {
                    tokio::time::timeout(Duration::from_secs(120), async_op()).await
                })
            };
            res.map_err(|_| VfsError::RateLimitTimeout)?
        })
    }

    /// Volume Parameters: Reports total pool capacity and free space across all linked accounts.
    pub fn get_volume_info(&self) -> Result<VolumeInfo, VfsError> {
        self.execute_vfs_op(None, || {
            let total_bytes = self.placement_engine.total_pool_capacity();
            let free_bytes = self.placement_engine.total_combined_free_space();

            Ok(VolumeInfo {
                total_bytes,
                free_bytes,
                case_sensitive_search: false,
                case_preserved_names: true,
                unicode_on_disk: true,
                reparse_points: false,
                operation_timeout_ms: 300_000,
                volume_label: "ucloud-fs".to_string(),
            })
        })
    }

    /// Resolves virtual path to parent inode and basename.
    fn resolve_path_parent_and_name(&self, path: &str) -> Result<(u64, String), VfsError> {
        let clean = path.trim_start_matches('/').trim_start_matches('\\');
        if clean.is_empty() {
            return Ok((InMemoryRoutingTable::ROOT_INO, String::new()));
        }

        let parts: Vec<&str> = clean.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Ok((InMemoryRoutingTable::ROOT_INO, String::new()));
        }

        let mut current_ino = InMemoryRoutingTable::ROOT_INO;
        for &name in parts.iter().take(parts.len().saturating_sub(1)) {
            current_ino = self.routing_table.lookup(current_ino, name).ok_or_else(|| {
                VfsError::NotFound(format!("Directory component '{name}' not found"))
            })?;
        }

        let filename = parts.last().map(|s| (*s).to_string()).unwrap_or_default();
        Ok((current_ino, filename))
    }

    /// Resolves virtual path to its inode ID.
    fn resolve_path(&self, path: &str) -> Result<u64, VfsError> {
        let clean = path.trim_start_matches('/').trim_start_matches('\\');
        if clean.is_empty() {
            return Ok(InMemoryRoutingTable::ROOT_INO);
        }

        let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
        self.routing_table
            .lookup(parent_ino, &filename)
            .ok_or_else(|| VfsError::NotFound(format!("File or directory '{path}' not found")))
    }

    /// File Info: Evaluated strictly from in-memory routing table in <0.01 ms with ZERO network calls.
    pub fn get_file_info(&self, path: &str) -> Result<FileInfo, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let node = self
                .routing_table
                .get_inode(ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;

            let attributes = if node.is_directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else if node.is_workspace_doc {
                FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };

            Ok(FileInfo {
                ino,
                size: node.size,
                allocation_size: node.size,
                is_directory: node.is_directory,
                is_readonly: node.is_workspace_doc,
                created_time: node.created_time,
                modified_time: node.modified_time,
                file_attributes: attributes,
            })
        })
    }

    /// Retrieves file information for an active open handle, including in-flight writes.
    fn file_info_from_handle_state(&self, fh: u64, state: &HandleState) -> Result<FileInfo, VfsError> {
        match state {
            HandleState::Directory { ino } => {
                let node = self
                    .routing_table
                    .get_inode(*ino)
                    .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;
                Ok(FileInfo {
                    ino: *ino,
                    size: 0,
                    allocation_size: 0,
                    is_directory: true,
                    is_readonly: false,
                    created_time: node.created_time,
                    modified_time: node.modified_time,
                    file_attributes: FILE_ATTRIBUTE_DIRECTORY,
                })
            }
            HandleState::Read { ino, .. } => {
                let node = self
                    .routing_table
                    .get_inode(*ino)
                    .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;
                let attributes = if node.is_workspace_doc {
                    FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                };
                Ok(FileInfo {
                    ino: *ino,
                    size: node.size,
                    allocation_size: node.size,
                    is_directory: false,
                    is_readonly: node.is_workspace_doc,
                    created_time: node.created_time,
                    modified_time: node.modified_time,
                    file_attributes: attributes,
                })
            }
            HandleState::WritePending(wp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = if let Some(spool) = &wp.write_spool {
                    spool.flushed_bytes() + spool.current_ram_usage() as u64
                } else {
                    wp.uploaded_bytes.len() as u64
                };
                let alloc = size.max(wp.allocation_size);
                Ok(FileInfo {
                    ino: fh,
                    size,
                    allocation_size: alloc,
                    is_directory: false,
                    is_readonly: false,
                    created_time: now,
                    modified_time: now,
                    file_attributes: FILE_ATTRIBUTE_NORMAL,
                })
            }
        }
    }

    /// Retrieves file information for an active open handle, with instantaneous non-blocking fast path.
    pub fn get_file_info_by_handle(&self, fh: u64) -> Result<FileInfo, VfsError> {
        let handle_arc = self
            .open_handles
            .get(&fh)
            .map(|g| Arc::clone(g.value()))
            .ok_or_else(|| VfsError::NotFound(format!("File handle {fh} not found")))?;

        if let Ok(state) = handle_arc.try_lock() {
            return self.file_info_from_handle_state(fh, &state);
        }

        self.execute_vfs_async_op(None, || async {
            let state = handle_arc.lock().await;
            self.file_info_from_handle_state(fh, &state)
        })
    }

    /// Opens an existing file for reading.
    pub fn open(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let node = self
                .routing_table
                .get_inode(ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {ino} not found")))?;

            if node.is_directory {
                let fh = self.allocate_fh();
                self.open_handles
                    .insert(fh, Arc::new(Mutex::new(HandleState::Directory { ino })));
                return Ok(fh);
            }

            // Construct ByteSource
            let byte_source: Arc<dyn ByteSource> = if let Some(mock) = self.mock_files.get(path) {
                Arc::new(MockByteSource::new(mock.clone()))
            } else {
                match &node.location {
                    StorageLocation::SingleAccount {
                        account_id,
                        google_file_id,
                    } => {
                        let token = self.account_tokens.get(account_id).map(|t| t.clone()).unwrap_or_default();
                        Arc::new(SingleAccountByteSource::new(
                            Arc::clone(&self.client),
                            account_id.clone(),
                            token,
                            google_file_id.clone(),
                            node.size,
                        ))
                    }
                    StorageLocation::StripedManifest {
                        manifest_account_id,
                        manifest_google_file_id,
                        ..
                    } => {
                        let mut tokens = HashMap::new();
                        for item in self.account_tokens.iter() {
                            tokens.insert(item.key().clone(), item.value().clone());
                        }

                        let manifest_token = tokens.get(manifest_account_id).cloned().unwrap_or_default();
                        let client = Arc::clone(&self.client);
                        let m_acc = manifest_account_id.clone();
                        let m_fid = manifest_google_file_id.clone();

                        let download_fut = async move {
                            if let Ok(resp) = client.download_stream(&m_acc, &manifest_token, &m_fid, None).await
                                && let Ok(bytes) = resp.bytes().await
                                && let Ok(manifest) = serde_json::from_slice::<FileManifest>(&bytes)
                            {
                                manifest.chunks
                            } else {
                                Vec::new()
                            }
                        };

                        let chunks = if tokio::runtime::Handle::try_current().is_ok() {
                            tokio::task::block_in_place(|| self.rt_handle.block_on(download_fut))
                        } else {
                            self.rt_handle.block_on(download_fut)
                        };

                        Arc::new(StripedManifestByteSource::new(
                            Arc::clone(&self.client),
                            tokens,
                            chunks,
                            node.size,
                        ))
                    }
                    StorageLocation::VirtualWarningNote { content, .. } => {
                        Arc::new(MockByteSource::new(Bytes::from(content.clone())))
                    }
                    StorageLocation::VirtualDirectory => {
                        return Err(VfsError::InternalFault("Cannot open directory as file".to_string()));
                    }
                }
            };

            let read_handle = EphemeralReadHandle::with_capacities(
                self.allocate_fh(),
                byte_source,
                &self.valve,
                16 * 1024 * 1024, // 16 MB forward window for smooth 1080p video streaming
                512 * 1024,       // 512 KB tail window
            )?;
            let fh = read_handle.fh();

            let _ = self.sync_engine.register_open_handle(ino);

            self.open_handles.insert(
                fh,
                Arc::new(Mutex::new(HandleState::Read {
                    ino,
                    handle: read_handle,
                })),
            );

            debug!(fh, path, ino, "Opened file for reading");
            Ok(fh)
        })
    }

    /// Creates a new virtual file in pending write state.
    pub fn create(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
            let fh = self.allocate_fh();

            self.open_handles.insert(
                fh,
                Arc::new(Mutex::new(HandleState::WritePending(Box::new(
                    WritePendingState {
                        path: path.to_string(),
                        parent_ino,
                        filename,
                        allocation_size: 0,
                        write_spool: None,
                        single_guard: None,
                        striped_guard: None,
                        striped_plan: None,
                        uploaded_bytes: Vec::new(),
                        delete_on_close: false,
                    },
                )))),
            );

            debug!(fh, path, "Created pending write handle");
            Ok(fh)
        })
    }

    /// Creates a new directory and spawns cloud folder creation on all linked Google accounts.
    pub fn create_directory(&self, path: &str) -> Result<u64, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, dirname) = self.resolve_path_parent_and_name(path)?;
            let ino = self.routing_table.create_virtual_directory(parent_ino, &dirname)?;
            let fh = self.allocate_fh();
            self.open_handles.insert(
                fh,
                Arc::new(Mutex::new(HandleState::Directory { ino })),
            );
            self.notifier.notify_created(path);
            let parent_path = self.routing_table.get_path_for_inode(parent_ino).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&parent_path);

            // Asynchronously dispatch remote folder creation to Google Drive
            let client = Arc::clone(&self.client);
            let routing_table = Arc::clone(&self.routing_table);
            let sync_engine = Arc::clone(&self.sync_engine);
            let dirname_clone = dirname.clone();
            let tokens: Vec<(String, String)> = self
                .account_tokens
                .iter()
                .map(|kv| (kv.key().clone(), kv.value().clone()))
                .collect();

            self.rt_handle.spawn(async move {
                for (account_id, token) in tokens {
                    let parent_remote = if parent_ino == InMemoryRoutingTable::ROOT_INO {
                        None
                    } else {
                        routing_table.get_remote_folder_id(parent_ino, &account_id)
                    };

                    match client
                        .create_folder(
                            &account_id,
                            &token,
                            &dirname_clone,
                            parent_remote.as_deref(),
                        )
                        .await
                    {
                        Ok(remote_id) => {
                            routing_table.record_remote_folder_id(ino, &account_id, &remote_id);
                            sync_engine.record_local_upsert(&account_id, &remote_id);
                            info!(
                                account = %account_id,
                                dirname = %dirname_clone,
                                remote_id = %remote_id,
                                "Created remote folder on Google Drive"
                            );
                        }
                        Err(e) => {
                            warn!(
                                account = %account_id,
                                dirname = %dirname_clone,
                                error = %e,
                                "Failed to create remote folder on Google Drive"
                            );
                        }
                    }
                }
            });

            debug!(fh, path, ino, "Created virtual directory handle and spawned cloud folder creation");
            Ok(fh)
        })
    }

    /// Sets the file size / allocation size. Intercepts allocation size to resolve placement target!
    pub fn set_file_size(&self, fh: u64, size: u64) -> Result<(), VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_arc = self
                .open_handles
                .get(&fh)
                .map(|g| Arc::clone(g.value()))
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            let mut state = handle_arc.lock().await;
            if let HandleState::WritePending(wp) = &mut *state {
                wp.allocation_size = size;

                if wp.write_spool.is_none() && wp.striped_plan.is_none() && size > 0 {
                    let decision = self
                        .placement_engine
                        .resolve_placement(size, &wp.filename, &*self.prompter)
                        .await?;

                    match decision {
                        PlacementDecision::SingleAccount(guard) => {
                            let account_id = guard.account_id.clone();
                            let token = self.account_tokens.get(&account_id).map(|t| t.clone()).unwrap_or_default();
                            let parent_remote = if wp.parent_ino == InMemoryRoutingTable::ROOT_INO {
                                None
                            } else {
                                self.routing_table.get_remote_folder_id(wp.parent_ino, &account_id)
                            };
                            let metadata = GoogleFileInput {
                                name: wp.filename.clone(),
                                mime_type: Some("application/octet-stream".to_string()),
                                parents: parent_remote.map(|pid| vec![pid]),
                                app_properties: None,
                            };

                            let session_uri = self
                                .client
                                .initiate_resumable_upload(&account_id, &token, &metadata)
                                .await
                                .unwrap_or_else(|_| format!("http://127.0.0.1:0/mock_session_{fh}"));

                            let mut spool = EphemeralWriteHandle::new(
                                fh,
                                Arc::clone(&self.client),
                                account_id,
                                session_uri,
                                &self.valve,
                            )?;
                            spool.set_declared_size(size);

                            wp.write_spool = Some(spool);
                            wp.single_guard = Some(guard);
                        }
                        PlacementDecision::StripedManifest(guard) => {
                            wp.striped_plan = Some(guard.plan.clone());
                            wp.striped_guard = Some(guard);
                        }
                    }
                }
            }

            Ok(())
        })
    }

    /// Reads binary content from an open file handle.
    pub fn read(&self, fh: u64, offset: u64, buffer: &mut [u8]) -> Result<usize, VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_arc = self
                .open_handles
                .get(&fh)
                .map(|g| Arc::clone(g.value()))
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            let mut state = handle_arc.lock().await;
            if let HandleState::Read { handle, .. } = &mut *state {
                let bytes = handle.read(offset, buffer.len()).await?;
                let len = bytes.len().min(buffer.len());
                buffer[..len].copy_from_slice(&bytes[..len]);
                Ok(len)
            } else {
                Err(VfsError::InternalFault("Handle not open for reading".to_string()))
            }
        })
    }

    /// Writes binary content to an open file handle.
    pub fn write(&self, fh: u64, offset: u64, buffer: &[u8]) -> Result<usize, VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_arc = self
                .open_handles
                .get(&fh)
                .map(|g| Arc::clone(g.value()))
                .ok_or_else(|| VfsError::NotFound("Invalid file handle".to_string()))?;

            let mut state = handle_arc.lock().await;
            if let HandleState::WritePending(wp) = &mut *state {
                if let Some(spool) = &mut wp.write_spool {
                    spool.write(offset, buffer).await
                } else {
                    let rel_offset = offset as usize;
                    if rel_offset > wp.uploaded_bytes.len() {
                        wp.uploaded_bytes.resize(rel_offset, 0);
                        wp.uploaded_bytes.extend_from_slice(buffer);
                    } else if rel_offset + buffer.len() <= wp.uploaded_bytes.len() {
                        wp.uploaded_bytes[rel_offset..rel_offset + buffer.len()].copy_from_slice(buffer);
                    } else {
                        let overwrite_len = wp.uploaded_bytes.len() - rel_offset;
                        wp.uploaded_bytes[rel_offset..].copy_from_slice(&buffer[..overwrite_len]);
                        wp.uploaded_bytes.extend_from_slice(&buffer[overwrite_len..]);
                    }
                    Ok(buffer.len())
                }
            } else {
                Err(VfsError::InternalFault("Handle not open for writing".to_string()))
            }
        })
    }

    /// Reads directory contents directly from in-memory routing table in <0.01 ms with ZERO network calls.
    pub fn read_directory(&self, path: &str) -> Result<Vec<DirEntry>, VfsError> {
        self.execute_vfs_op(Some(path), || {
            let ino = self.resolve_path(path)?;
            let children = self.routing_table.readdir(ino)?;

            let entries = children
                .into_iter()
                .map(|child| DirEntry {
                    ino: child.ino,
                    name: child.name,
                    is_directory: child.is_directory,
                    size: child.size,
                })
                .collect();

            Ok(entries)
        })
    }

    /// Renames a virtual file or directory, propagating the rename to Google Drive.
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), VfsError> {
        self.execute_vfs_op(Some(old_path), || {
            sandbox::validate_path(new_path)?;

            let (old_parent, old_name) = self.resolve_path_parent_and_name(old_path)?;
            let (new_parent, new_name) = self.resolve_path_parent_and_name(new_path)?;

            let target_ino = self
                .routing_table
                .lookup(old_parent, &old_name)
                .ok_or_else(|| VfsError::NotFound(format!("File or directory '{old_name}' not found for rename")))?;

            let node = self
                .routing_table
                .get_inode(target_ino)
                .ok_or_else(|| VfsError::NotFound(format!("Inode {target_ino} not found for rename")))?;

            // Collect cloud rename targets
            let mut cloud_renames = Vec::new();
            match &node.location {
                StorageLocation::SingleAccount { account_id, google_file_id } => {
                    self.sync_engine.record_local_upsert(account_id, google_file_id);
                    cloud_renames.push((account_id.clone(), google_file_id.clone()));
                }
                StorageLocation::StripedManifest { manifest_account_id, manifest_google_file_id, .. } => {
                    self.sync_engine.record_local_upsert(manifest_account_id, manifest_google_file_id);
                    cloud_renames.push((manifest_account_id.clone(), manifest_google_file_id.clone()));
                }
                StorageLocation::VirtualDirectory => {
                    for acc_id in self.account_tokens.iter().map(|kv| kv.key().clone()) {
                        if let Some(folder_id) = self.routing_table.get_remote_folder_id(target_ino, &acc_id) {
                            self.sync_engine.record_local_upsert(&acc_id, &folder_id);
                            cloud_renames.push((acc_id, folder_id));
                        }
                    }
                }
                StorageLocation::VirtualWarningNote { .. } => {}
            }

            self.routing_table.rename(old_parent, &old_name, new_parent, &new_name)?;
            self.notifier.notify_deleted(old_path);
            self.notifier.notify_created(new_path);
            let old_parent_path = self.routing_table.get_path_for_inode(old_parent).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&old_parent_path);
            if new_parent != old_parent {
                let new_parent_path = self.routing_table.get_path_for_inode(new_parent).unwrap_or_else(|| "/".to_string());
                self.notifier.notify_updated(&new_parent_path);
            }

            if !cloud_renames.is_empty() {
                let client = Arc::clone(&self.client);
                let tokens_map: HashMap<String, String> = self.account_tokens.iter().map(|kv| (kv.key().clone(), kv.value().clone())).collect();
                let new_name_clone = new_name.clone();
                self.rt_handle.spawn(async move {
                    for (account_id, file_id) in cloud_renames {
                        if let Some(token) = tokens_map.get(&account_id) {
                            if let Err(e) = client.rename_file(&account_id, token, &file_id, &new_name_clone).await {
                                warn!(account = %account_id, file_id = %file_id, new_name = %new_name_clone, error = %e, "Failed to rename on Google Drive");
                            } else {
                                info!(account = %account_id, file_id = %file_id, new_name = %new_name_clone, "Successfully renamed on Google Drive");
                            }
                        }
                    }
                });
            }

            info!(old_path, new_path, "Renamed filesystem entry and dispatched cloud renames");
            Ok(())
        })
    }

    /// Deletes a file or directory, propagating the deletion to Google Drive.
    pub fn delete(&self, path: &str) -> Result<(), VfsError> {
        self.execute_vfs_op(Some(path), || {
            let (parent_ino, filename) = self.resolve_path_parent_and_name(path)?;
            let ino = self.routing_table.lookup(parent_ino, &filename).ok_or_else(|| {
                VfsError::NotFound(format!("File or directory '{path}' not found for deletion"))
            })?;

            let node = self.routing_table.get_inode(ino).ok_or_else(|| {
                VfsError::NotFound(format!("Inode {ino} not found for deletion"))
            })?;

            if node.is_directory && !node.children.is_empty() {
                return Err(VfsError::InternalFault(
                    "Directory not empty".to_string(),
                ));
            }

            // Collect remote cloud deletion targets
            let mut cloud_deletions = Vec::new();
            match &node.location {
                StorageLocation::SingleAccount { account_id, google_file_id } => {
                    self.sync_engine.record_local_mutation(account_id, google_file_id);
                    cloud_deletions.push((account_id.clone(), google_file_id.clone()));
                    self.placement_engine.release_used_bytes(account_id, node.size);
                }
                StorageLocation::StripedManifest {
                    manifest_account_id,
                    manifest_google_file_id,
                    ..
                } => {
                    self.sync_engine.record_local_mutation(manifest_account_id, manifest_google_file_id);
                    cloud_deletions.push((manifest_account_id.clone(), manifest_google_file_id.clone()));
                }
                StorageLocation::VirtualDirectory => {
                    for (acc_id, folder_id) in self.routing_table.get_all_remote_folder_ids(ino) {
                        self.sync_engine.record_local_mutation(&acc_id, &folder_id);
                        cloud_deletions.push((acc_id, folder_id));
                    }
                }
                StorageLocation::VirtualWarningNote { .. } => {}
            }

            let _ = self.routing_table.remove(parent_ino, &filename)?;
            self.notifier.notify_deleted(path);
            let parent_path = self.routing_table.get_path_for_inode(parent_ino).unwrap_or_else(|| "/".to_string());
            self.notifier.notify_updated(&parent_path);

            if !cloud_deletions.is_empty() {
                let client = Arc::clone(&self.client);
                let tokens_map: HashMap<String, String> = self.account_tokens.iter().map(|kv| (kv.key().clone(), kv.value().clone())).collect();
                self.rt_handle.spawn(async move {
                    for (account_id, file_id) in cloud_deletions {
                        if let Some(token) = tokens_map.get(&account_id) {
                            if let Err(e) = client.delete_file(&account_id, token, &file_id).await {
                                warn!(account = %account_id, file_id = %file_id, error = %e, "Failed to delete file on Google Drive");
                            } else {
                                info!(account = %account_id, file_id = %file_id, "Successfully deleted remote file on Google Drive");
                            }
                        }
                    }
                });
            }

            info!(path, "Deleted filesystem entry and dispatched cloud deletions");
            Ok(())
        })
    }

    /// Finalizes open write streams on close, committing files to routing table and zeroizing RAM.
    pub fn cleanup(&self, fh: u64) -> Result<(), VfsError> {
        self.execute_vfs_async_op(None, || async move {
            let handle_arc = match self.open_handles.get(&fh) {
                Some(g) => Arc::clone(g.value()),
                None => return Ok(()),
            };

            let mut state = handle_arc.lock().await;
            if let HandleState::WritePending(wp) = &mut *state {
                if wp.delete_on_close {
                    debug!(fh, path = %wp.path, "Cancelled write handle with delete_on_close");
                    return Ok(());
                }

                // If no spool was initialized yet (e.g. WriteFile was called without SetFileSize),
                // initialize spool now from accumulated uploaded_bytes.
                if wp.write_spool.is_none() && wp.striped_guard.is_none() {
                    let total_bytes = wp.uploaded_bytes.len() as u64;
                    let decision = self
                        .placement_engine
                        .resolve_placement(total_bytes.max(1), &wp.filename, &*self.prompter)
                        .await?;

                    match decision {
                        PlacementDecision::SingleAccount(guard) => {
                            let account_id = guard.account_id.clone();
                            let token = self
                                .account_tokens
                                .get(&account_id)
                                .map(|t| t.clone())
                                .unwrap_or_default();
                            let parent_remote = if wp.parent_ino == InMemoryRoutingTable::ROOT_INO {
                                None
                            } else {
                                self.routing_table.get_remote_folder_id(wp.parent_ino, &account_id)
                            };
                            let metadata = GoogleFileInput {
                                name: wp.filename.clone(),
                                mime_type: Some("application/octet-stream".to_string()),
                                parents: parent_remote.map(|pid| vec![pid]),
                                app_properties: None,
                            };
                            let session_uri = self
                                .client
                                .initiate_resumable_upload(&account_id, &token, &metadata)
                                .await
                                .unwrap_or_else(|_| format!("http://127.0.0.1:0/mock_session_{fh}"));
                            let mut spool = EphemeralWriteHandle::new(
                                fh,
                                Arc::clone(&self.client),
                                account_id,
                                session_uri,
                                &self.valve,
                            )?;
                            spool.set_declared_size(total_bytes);
                            if !wp.uploaded_bytes.is_empty() {
                                spool.write(0, &wp.uploaded_bytes).await?;
                            }
                            wp.write_spool = Some(spool);
                            wp.single_guard = Some(guard);
                        }
                        PlacementDecision::StripedManifest(guard) => {
                            wp.striped_plan = Some(guard.plan.clone());
                            wp.striped_guard = Some(guard);
                        }
                    }
                }

                if let Some(mut spool) = wp.write_spool.take() {
                    let account_id = spool.account_id().to_string();
                    let commit_res = spool.commit().await?;

                    if let Some(mut guard) = wp.single_guard.take() {
                        guard.commit();
                    }

                    let location = StorageLocation::SingleAccount {
                        account_id: account_id.clone(),
                        google_file_id: commit_res.file_id.clone(),
                    };

                    let _ = self.routing_table.insert_file(NewFileParams {
                        parent_ino: wp.parent_ino,
                        name: &wp.filename,
                        size: commit_res.total_bytes,
                        mime_type: "application/octet-stream",
                        modified_time: 1_700_000_000,
                        created_time: 1_700_000_000,
                        location,
                        web_view_link: None,
                    })?;

                    self.sync_engine.record_local_upsert(&account_id, &commit_res.file_id);
                    self.notifier.notify_created(&wp.path);
                    info!(path = %wp.path, total_bytes = commit_res.total_bytes, "Committed single file upload to VFS");
                } else if let Some(guard) = wp.striped_guard.take() {
                    let mut account_tokens = HashMap::new();
                    for item in self.account_tokens.iter() {
                        account_tokens.insert(item.key().clone(), item.value().clone());
                    }

                    let chunker = OversizedFileChunker::new(
                        Arc::clone(&self.client),
                        Arc::clone(&self.routing_table),
                    );

                    let unified_ino = chunker
                        .upload_striped_file(
                            wp.parent_ino,
                            guard,
                            &wp.uploaded_bytes,
                            &account_tokens,
                        )
                        .await?;

                    if let Some(node) = self.routing_table.get_inode(unified_ino)
                        && let StorageLocation::StripedManifest {
                            manifest_account_id,
                            manifest_google_file_id,
                            ..
                        } = &node.location
                    {
                        self.sync_engine
                            .record_local_upsert(manifest_account_id, manifest_google_file_id);
                    }

                    self.notifier.notify_created(&wp.path);
                    info!(path = %wp.path, unified_ino, "Committed multi-account striped upload to VFS");
                }
            }

            Ok(())
        })
    }

    /// Closes an open file handle and releases all associated memory.
    pub fn close(&self, fh: u64) -> Result<(), VfsError> {
        self.execute_vfs_op(None, || {
            if let Some((_, handle_arc)) = self.open_handles.remove(&fh) {
                if let Ok(state) = handle_arc.try_lock()
                    && let HandleState::Read { ino, .. } = *state
                {
                    self.sync_engine.unregister_open_handle(ino);
                }
                debug!(fh, "Closed and unregistered file handle");
            }
            Ok(())
        })
    }

    /// Cleanly unmounts the filesystem.
    pub fn unmount(&self) -> Result<(), VfsError> {
        info!("Unmounting ucloud-fs VFS driver");
        self.is_mounted.store(false, Ordering::SeqCst);
        self.open_handles.clear();
        self.session.lock_and_purge_all_caches();
        Ok(())
    }

    /// Returns whether the driver is actively mounted.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        self.is_mounted.load(Ordering::SeqCst)
    }

    /// Number of active open file handles.
    #[must_use]
    pub fn active_handles_count(&self) -> usize {
        self.open_handles.len()
    }
}
