use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::error::VfsError;
use crate::network::client::{GoogleChangesList, GoogleDriveClient};
use crate::routing::inode::{NewFileParams, StorageLocation};
use crate::routing::placement::PlacementEngine;
use crate::routing::InMemoryRoutingTable;
use crate::security::session::SessionController;
use crate::vfs::notify::ShellNotifier;

/// Statistics gathered during a single delta synchronization reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub deleted: usize,
    pub created: usize,
    pub updated: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMutationKind {
    Upsert,
    Delete,
}

/// Real-time delta synchronization engine polling `changes.list` and keeping RAM VFS in sync.
pub struct DeltaSyncEngine {
    client: Arc<GoogleDriveClient>,
    routing_table: Arc<InMemoryRoutingTable>,
    placement_engine: Arc<PlacementEngine>,
    session: Arc<SessionController>,
    notifier: Arc<dyn ShellNotifier>,
    page_tokens: DashMap<String, String>,
    recent_local_mutations: DashMap<(String, String), (LocalMutationKind, Instant)>,
    open_handles: DashMap<u64, Arc<AtomicBool>>,
    poll_interval: Duration,
}

impl DeltaSyncEngine {
    /// Default delta polling frequency: 15 seconds.
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15);
    /// Self-echo suppression TTL: 30 seconds.
    pub const SELF_ECHO_TTL: Duration = Duration::from_secs(30);

    /// Creates a new `DeltaSyncEngine`.
    #[must_use]
    pub fn new(
        client: Arc<GoogleDriveClient>,
        routing_table: Arc<InMemoryRoutingTable>,
        placement_engine: Arc<PlacementEngine>,
        session: Arc<SessionController>,
        notifier: Arc<dyn ShellNotifier>,
    ) -> Self {
        Self::with_poll_interval(
            client,
            routing_table,
            placement_engine,
            session,
            notifier,
            Self::DEFAULT_POLL_INTERVAL,
        )
    }

    /// Creates an engine with a custom poll interval (ideal for testing).
    #[must_use]
    pub fn with_poll_interval(
        client: Arc<GoogleDriveClient>,
        routing_table: Arc<InMemoryRoutingTable>,
        placement_engine: Arc<PlacementEngine>,
        session: Arc<SessionController>,
        notifier: Arc<dyn ShellNotifier>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            client,
            routing_table,
            placement_engine,
            session,
            notifier,
            page_tokens: DashMap::new(),
            recent_local_mutations: DashMap::new(),
            open_handles: DashMap::new(),
            poll_interval,
        }
    }

    /// Sets or updates the starting page token for delta polling on an account.
    pub fn set_start_page_token(&self, account_id: &str, token: String) {
        self.page_tokens.insert(account_id.to_string(), token);
    }

    /// Records a local deletion to prevent "self-echo" flicker in changes.list.
    pub fn record_local_mutation(&self, account_id: &str, file_id: &str) {
        self.record_local_mutation_kind(account_id, file_id, LocalMutationKind::Delete);
    }

    /// Records a local upload or rename to prevent "self-echo" flicker in changes.list.
    pub fn record_local_upsert(&self, account_id: &str, file_id: &str) {
        self.record_local_mutation_kind(account_id, file_id, LocalMutationKind::Upsert);
    }

    /// Records a local mutation with explicit kind.
    pub fn record_local_mutation_kind(
        &self,
        account_id: &str,
        file_id: &str,
        kind: LocalMutationKind,
    ) {
        self.recent_local_mutations.insert(
            (account_id.to_string(), file_id.to_string()),
            (kind, Instant::now()),
        );
    }

    /// Registers an open file handle, returning an `is_unlinked` flag for clean deletion handling.
    #[must_use]
    pub fn register_open_handle(&self, ino: u64) -> Arc<AtomicBool> {
        self.open_handles
            .entry(ino)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Unregisters an open file handle when closed.
    pub fn unregister_open_handle(&self, ino: u64) {
        self.open_handles.remove(&ino);
    }

    /// Checks if a file was recently mutated locally within the 30-second self-echo window.
    fn is_self_echo(&self, account_id: &str, file_id: &str, expected_kind: LocalMutationKind) -> bool {
        let key = (account_id.to_string(), file_id.to_string());
        if let Some(entry) = self.recent_local_mutations.get(&key) {
            let (kind, instant) = *entry.value();
            if kind == expected_kind && instant.elapsed() < Self::SELF_ECHO_TTL {
                return true;
            }
        }
        false
    }

    /// Applies a batch of remote cloud changes into the local in-memory routing table.
    pub async fn apply_changes(
        &self,
        account_id: &str,
        changes_list: &GoogleChangesList,
        access_token: &str,
    ) -> Result<SyncStats, VfsError> {
        let mut stats = SyncStats::default();

        for change in &changes_list.changes {
            let file_id = &change.file_id;
            let is_removed = change.removed.unwrap_or(false)
                || change
                    .file
                    .as_ref()
                    .and_then(|f| f.trashed)
                    .unwrap_or(false);

            if is_removed {
                // 1. Remote Deletion
                if let Some(ino) = self.routing_table.lookup_by_cloud_id(account_id, file_id) {
                    if self.is_self_echo(account_id, file_id, LocalMutationKind::Delete) {
                        debug!(
                            account = %account_id,
                            file_id,
                            "Suppressed self-echo deletion notification"
                        );
                        continue;
                    }

                    // Check if file is currently open; if so, flag is_unlinked
                    if let Some(flag) = self.open_handles.get(&ino) {
                        flag.store(true, Ordering::SeqCst);
                        warn!(
                            ino,
                            file_id,
                            "Remote deletion occurred while file handle is open locally"
                        );
                    }

                    if let Some(node) = self.routing_table.get_inode(ino) {
                        let is_striped = matches!(node.location, StorageLocation::StripedManifest { .. });
                        let _ = self.routing_table.remove(node.parent_ino, &node.name);
                        let del_path = if let Some(parent_path) = self.routing_table.get_path_for_inode(node.parent_ino) {
                            if parent_path == "/" || parent_path.is_empty() {
                                format!("/{}", node.name)
                            } else {
                                format!("{}/{}", parent_path.trim_end_matches('/'), node.name)
                            }
                        } else {
                            format!("/{}", node.name)
                        };
                        self.notifier.notify_deleted(&del_path);
                        stats.deleted += 1;

                        if is_striped {
                            let warning_name = format!("{}.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt", node.name);
                            if self.routing_table.remove(node.parent_ino, &warning_name).is_ok() {
                                let warn_path = if let Some(parent_path) = self.routing_table.get_path_for_inode(node.parent_ino) {
                                    if parent_path == "/" || parent_path.is_empty() {
                                        format!("/{}", warning_name)
                                    } else {
                                        format!("{}/{}", parent_path.trim_end_matches('/'), warning_name)
                                    }
                                } else {
                                    format!("/{}", warning_name)
                                };
                                self.notifier.notify_deleted(&warn_path);
                            }
                        }

                        info!(
                            account = %account_id,
                            name = %node.name,
                            ino,
                            "Reconciled remote cloud deletion into local VFS"
                        );
                    }
                }
            } else if let Some(file) = &change.file {
                // 2. Remote Addition or Update
                if self.is_self_echo(account_id, file_id, LocalMutationKind::Upsert) {
                    debug!(
                        account = %account_id,
                        file_id,
                        "Suppressed self-echo update notification"
                    );
                    continue;
                }

                if let Some(existing_ino) = self.routing_table.lookup_by_cloud_id(account_id, file_id) {
                    // Update existing inode
                    if let Some(mut node) = self.routing_table.get_inode(existing_ino) {
                        if node.name != file.name {
                            let _ = self.routing_table.rename(
                                node.parent_ino,
                                &node.name,
                                node.parent_ino,
                                &file.name,
                            );
                            node.name = file.name.clone();
                        }
                        self.notifier.notify_updated(&node.name);
                        stats.updated += 1;
                        debug!(ino = existing_ino, name = %file.name, "Reconciled remote cloud update");
                    }
                } else {
                    // New remote file or folder created directly on web drive
                    let parent_google_id = file
                        .parents
                        .as_ref()
                        .and_then(|p| p.first())
                        .map(|s| s.as_str());

                    let parent_ino = match parent_google_id {
                        None | Some("root") => InMemoryRoutingTable::ROOT_INO,
                        Some(gid) => self
                            .routing_table
                            .lookup_by_cloud_id(account_id, gid)
                            .unwrap_or(InMemoryRoutingTable::ROOT_INO),
                    };

                    let is_folder = file.mime_type == "application/vnd.google-apps.folder";

                    if is_folder {
                        let _ = self.routing_table.insert_or_merge_directory(
                            parent_ino,
                            &file.name,
                            Some(account_id),
                            Some(file_id),
                        )?;
                        let full_path = if parent_ino == InMemoryRoutingTable::ROOT_INO {
                            file.name.clone()
                        } else if let Some(parent_path) = self.routing_table.get_path_for_inode(parent_ino) {
                            if parent_path == "/" || parent_path.is_empty() {
                                file.name.clone()
                            } else {
                                format!("{}/{}", parent_path.trim_end_matches('/'), file.name)
                            }
                        } else {
                            file.name.clone()
                        };
                        self.notifier.notify_created(&full_path);
                        stats.created += 1;
                        info!(
                            account = %account_id,
                            name = %file.name,
                            file_id,
                            "Reconciled new remote cloud folder into local VFS"
                        );
                    } else {
                        let size = file.size.as_ref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                        let location = StorageLocation::SingleAccount {
                            account_id: account_id.to_string(),
                            google_file_id: file.id.clone(),
                        };

                        let _ = self.routing_table.insert_file(NewFileParams {
                            parent_ino,
                            name: &file.name,
                            size,
                            mime_type: &file.mime_type,
                            modified_time: 0,
                            created_time: 0,
                            location,
                            web_view_link: None,
                        })?;

                        let full_path = if parent_ino == InMemoryRoutingTable::ROOT_INO {
                            file.name.clone()
                        } else if let Some(parent_path) = self.routing_table.get_path_for_inode(parent_ino) {
                            if parent_path == "/" || parent_path.is_empty() {
                                file.name.clone()
                            } else {
                                format!("{}/{}", parent_path.trim_end_matches('/'), file.name)
                            }
                        } else {
                            file.name.clone()
                        };
                        self.notifier.notify_created(&full_path);
                        stats.created += 1;
                        info!(
                            account = %account_id,
                            name = %file.name,
                            "Reconciled new remote cloud file into local VFS"
                        );
                    }
                }
            }
        }

        // 3. Live Quota Sync: Update used bytes if changes were detected
        if !changes_list.changes.is_empty()
            && let Ok(about) = self.client.get_about(account_id, access_token).await
            && let Some(used) = about
                .storage_quota
                .and_then(|q| q.usage)
                .and_then(|s| s.parse::<u64>().ok())
        {
            self.placement_engine.update_used_bytes(account_id, used);
            debug!(account = %account_id, used_bytes = used, "Live quota updated");
        }

        // 4. Advance page token
        if let Some(tok) = &changes_list.new_start_page_token {
            self.set_start_page_token(account_id, tok.clone());
        } else if let Some(tok) = &changes_list.next_page_token {
            self.set_start_page_token(account_id, tok.clone());
        }

        Ok(stats)
    }

    /// Single poll cycle for an account.
    pub async fn poll_account(
        &self,
        account_id: &str,
        access_token: &str,
    ) -> Result<SyncStats, VfsError> {
        let page_token = self
            .page_tokens
            .get(account_id)
            .map(|t| t.clone())
            .ok_or_else(|| {
                VfsError::InternalFault(format!("No start page token found for account '{account_id}'"))
            })?;

        let changes = self
            .client
            .list_changes(account_id, access_token, &page_token)
            .await?;

        self.apply_changes(account_id, &changes, access_token).await
    }

    /// Spawns the background delta polling loop across accounts.
    ///
    /// Automatically pauses/aborts when the session locks.
    pub fn spawn_polling_loop(
        self: Arc<Self>,
        account_tokens: HashMap<String, String>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("Background delta polling loop started");
            let mut interval = tokio::time::interval(self.poll_interval);

            while self.session.is_unlocked() {
                interval.tick().await;

                if !self.session.is_unlocked() {
                    break;
                }

                for (account_id, token) in &account_tokens {
                    if let Err(e) = self.poll_account(account_id, token).await {
                        warn!(account = %account_id, error = %e, "Delta poll cycle encountered error");
                    }
                }
            }

            info!("Background delta polling loop terminated (session locked or logged out)");
        })
    }
}
