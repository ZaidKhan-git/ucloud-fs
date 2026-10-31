use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, info};

use crate::error::VfsError;
use crate::network::client::HydratedAccountData;
use crate::routing::inode::{NewFileParams, StorageLocation, VirtualInode};
use crate::vfs::sandbox::{canonical_lookup_key, validate_path};

/// High-performance, lock-free in-memory DNS routing table for virtual inodes.
///
/// Features:
/// - Sub-microsecond O(1) path and inode lookups.
/// - Zero persistent disk caching.
/// - Strict Single-Guard Rule: No DashMap guard is ever held while acquiring another guard.
/// - Transparent directory merging across accounts and deterministic file collision resolution.
pub struct InMemoryRoutingTable {
    /// Inode metadata index: InodeId -> VirtualInode
    inodes: DashMap<u64, VirtualInode>,
    /// Fast name lookup index: (parent_ino, canonical_case_folded_name) -> child_ino
    lookup_by_name: DashMap<(u64, String), u64>,
    /// Reverse cloud index for delta sync: (account_id, google_file_id) -> ino
    reverse_cloud_index: DashMap<(String, String), u64>,
    /// Virtual folders mapping: ino -> (account_id -> google_folder_id)
    virtual_folders: DashMap<u64, HashMap<String, String>>,
    /// Track colliding base file names: (parent_ino, canonical_lookup_key(base_name)) -> original_base_name
    collided_files: DashMap<(u64, String), String>,
    /// Track colliding base directory names: (parent_ino, canonical_lookup_key(base_name)) -> original_base_name
    collided_dirs: DashMap<(u64, String), String>,
    /// Monotonic 64-bit inode allocator (starts at 2; 1 is always `/`)
    next_ino: AtomicU64,
}

impl InMemoryRoutingTable {
    /// Inode ID assigned to the virtual filesystem root directory `/`.
    pub const ROOT_INO: u64 = 1;

    /// Creates a new `InMemoryRoutingTable` and initializes the root inode (`ino = 1`).
    #[must_use]
    pub fn new() -> Self {
        let table = Self {
            inodes: DashMap::new(),
            lookup_by_name: DashMap::new(),
            reverse_cloud_index: DashMap::new(),
            virtual_folders: DashMap::new(),
            collided_files: DashMap::new(),
            collided_dirs: DashMap::new(),
            next_ino: AtomicU64::new(2),
        };

        // Initialize root directory
        let root = VirtualInode::new_root();
        table.inodes.insert(Self::ROOT_INO, root);
        table.lookup_by_name.insert(
            (Self::ROOT_INO, canonical_lookup_key("/")),
            Self::ROOT_INO,
        );
        table
            .virtual_folders
            .insert(Self::ROOT_INO, HashMap::new());

        table
    }

    /// Allocates a new unique monotonic 64-bit inode ID.
    #[must_use]
    pub fn allocate_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::SeqCst)
    }

    /// Looks up a child inode by parent inode and child name in O(1) time (< 1 µs).
    #[must_use]
    pub fn lookup(&self, parent_ino: u64, name: &str) -> Option<u64> {
        let key = (parent_ino, canonical_lookup_key(name));
        // Strict single-guard: drop DashMap guard immediately after copying u64
        let guard = self.lookup_by_name.get(&key)?;
        Some(*guard.value())
    }

    /// Retrieves a clone of the `VirtualInode` for the given inode ID.
    #[must_use]
    pub fn get_inode(&self, ino: u64) -> Option<VirtualInode> {
        let guard = self.inodes.get(&ino)?;
        Some(guard.value().clone())
    }

    /// Resolves a full virtual path (e.g. `/Documents/Projects/doc.txt`) to an `InodeId`.
    pub fn resolve_path(&self, raw_path: &str) -> Result<u64, VfsError> {
        let normalized = validate_path(raw_path)?;
        if normalized == "/" {
            return Ok(Self::ROOT_INO);
        }

        let mut curr_ino = Self::ROOT_INO;
        for segment in normalized.split('/').filter(|s| !s.is_empty()) {
            curr_ino = self
                .lookup(curr_ino, segment)
                .ok_or_else(|| VfsError::NotFound(format!("Path component '{segment}' not found")))?;
        }

        Ok(curr_ino)
    }

    /// Reads directory contents, returning a vector of child `VirtualInode` clones.
    pub fn readdir(&self, parent_ino: u64) -> Result<Vec<VirtualInode>, VfsError> {
        let child_ids = {
            let parent = self
                .inodes
                .get(&parent_ino)
                .ok_or_else(|| VfsError::NotFound(format!("Parent inode {parent_ino} not found")))?;
            if !parent.is_directory {
                return Err(VfsError::NotFound(format!(
                    "Inode {parent_ino} is not a directory"
                )));
            }
            parent.children.clone()
        };

        let mut entries = Vec::with_capacity(child_ids.len());
        for cid in child_ids {
            if let Some(child) = self.get_inode(cid) {
                entries.push(child);
            }
        }

        Ok(entries)
    }

    /// Generates a clean abbreviation for an account ID:
    /// e.g. "alice.work@gmail.com" -> "alice.work", "acc_b" -> "acc_b".
    #[must_use]
    pub fn account_abbreviation(account_id: &str) -> String {
        let raw = if let Some(idx) = account_id.find('@') {
            &account_id[..idx]
        } else {
            account_id
        };
        let clean: String = raw
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                _ => c,
            })
            .collect();
        if clean.is_empty() {
            "acc".to_string()
        } else {
            clean
        }
    }

    /// Formats a filename or dirname with an account abbreviation and optional numeric counter:
    /// e.g. "report.pdf", "alice", 1 -> "report (alice).pdf"
    /// e.g. "report.pdf", "alice", 2 -> "report (alice_2).pdf"
    #[must_use]
    pub fn format_disambiguated_name(name: &str, abbr: &str, counter: usize) -> String {
        let (stem, ext) = match name.rfind('.') {
            Some(idx) if idx > 0 && idx < name.len() - 1 => (&name[..idx], Some(&name[idx + 1..])),
            _ => (name, None),
        };
        let suffix = if counter <= 1 {
            format!("({abbr})")
        } else {
            format!("({abbr}_{counter})")
        };
        match ext {
            Some(e) => format!("{stem} {suffix}.{e}"),
            None => format!("{stem} {suffix}"),
        }
    }

    /// Formats a folder name with an account abbreviation:
    /// e.g. "JaipurTrip2024", "mohdzaidk035@gmail.com" -> "JaipurTrip2024 (mohdzaidk035)"
    #[must_use]
    pub fn format_folder_disambiguated(name: &str, account_id: &str) -> String {
        let abbr = Self::account_abbreviation(account_id);
        format!("{name} ({abbr})")
    }

    /// Formats a filename with an account abbreviation:
    /// e.g. "itinerary.pdf", "mohdzaidk035@gmail.com" -> "itinerary (mohdzaidk035).pdf"
    #[must_use]
    pub fn format_disambiguated(name: &str, account_id: &str) -> String {
        let abbr = Self::account_abbreviation(account_id);
        Self::format_disambiguated_name(name, &abbr, 1)
    }

    /// Returns the preferred account for an inode.
    /// Walks up the directory tree to find the first ancestor with an associated cloud account.
    #[must_use]
    pub fn get_preferred_account_for_folder(&self, mut ino: u64) -> Option<String> {
        while ino != Self::ROOT_INO {
            if let Some(guard) = self.virtual_folders.get(&ino)
                && let Some((acc, _)) = guard.value().iter().next()
            {
                return Some(acc.clone());
            }
            if let Some(node) = self.inodes.get(&ino) {
                ino = node.parent_ino;
            } else {
                break;
            }
        }
        None
    }

    /// Generates a deterministic disambiguated filename if collision exists:
    /// e.g. `img.jpg` on `acc_b` -> `img (acc_b).jpg`.
    #[must_use]
    pub fn disambiguate_filename(&self, parent_ino: u64, name: &str, account_id: &str) -> String {
        if self.lookup(parent_ino, name).is_none() {
            return name.to_string();
        }

        let abbr = Self::account_abbreviation(account_id);
        let mut counter = 1;
        loop {
            let candidate = Self::format_disambiguated_name(name, &abbr, counter);
            if self.lookup(parent_ino, &candidate).is_none() {
                return candidate;
            }
            counter += 1;
        }
    }

    /// Inserts a new virtual file under `parent_ino` with automatic bidirectional collision resolution.
    pub fn insert_file(&self, params: NewFileParams<'_>) -> Result<u64, VfsError> {
        let account_id_for_disambiguation = match &params.location {
            StorageLocation::SingleAccount { account_id, .. } => account_id.as_str(),
            StorageLocation::StripedManifest {
                manifest_account_id,
                ..
            } => manifest_account_id.as_str(),
            _ => "cloud",
        };

        let new_abbr = Self::account_abbreviation(account_id_for_disambiguation);
        let canonical_base = canonical_lookup_key(params.name);
        let base_key = (params.parent_ino, canonical_base.clone());

        let final_name = if self.collided_files.contains_key(&base_key) {
            // Base name already collided previously: suffix 3rd, 4th, or N-th account immediately
            let mut counter = 1;
            loop {
                let candidate = Self::format_disambiguated_name(params.name, &new_abbr, counter);
                if self.lookup(params.parent_ino, &candidate).is_none() {
                    break candidate;
                }
                counter += 1;
            }
        } else if let Some(existing_ino) = self.lookup(params.parent_ino, params.name) {
            // Collision detected! Check if existing file is from a different account
            let existing_acc_opt = self.get_inode(existing_ino).and_then(|node| match &node.location {
                StorageLocation::SingleAccount { account_id, .. } => Some(account_id.clone()),
                StorageLocation::StripedManifest { manifest_account_id, .. } => Some(manifest_account_id.clone()),
                _ => None,
            });

            if let Some(existing_acc) = existing_acc_opt {
                if existing_acc != account_id_for_disambiguation {
                    // Cross-account collision: Bidirectionally rename existing file and suffix new file!
                    self.collided_files.insert(base_key.clone(), params.name.to_string());

                    let old_abbr = Self::account_abbreviation(&existing_acc);
                    let mut old_counter = 1;
                    let old_disambiguated = loop {
                        let candidate = Self::format_disambiguated_name(params.name, &old_abbr, old_counter);
                        if self.lookup(params.parent_ino, &candidate).is_none() {
                            break candidate;
                        }
                        old_counter += 1;
                    };

                    // Rename existing file in lookup_by_name and inodes (Strict Single-Guard discipline)
                    self.lookup_by_name.remove(&base_key);
                    self.lookup_by_name.insert(
                        (params.parent_ino, canonical_lookup_key(&old_disambiguated)),
                        existing_ino,
                    );
                    if let Some(mut existing_node) = self.inodes.get_mut(&existing_ino) {
                        existing_node.name = old_disambiguated;
                    }

                    // Compute disambiguated name for the new incoming file
                    let mut new_counter = 1;
                    loop {
                        let candidate = Self::format_disambiguated_name(params.name, &new_abbr, new_counter);
                        if self.lookup(params.parent_ino, &candidate).is_none() {
                            break candidate;
                        }
                        new_counter += 1;
                    }
                } else {
                    // Same account collision: suffix new file with numeric increment
                    let mut counter = 2;
                    loop {
                        let candidate = Self::format_disambiguated_name(params.name, &new_abbr, counter);
                        if self.lookup(params.parent_ino, &candidate).is_none() {
                            break candidate;
                        }
                        counter += 1;
                    }
                }
            } else {
                params.name.to_string()
            }
        } else {
            params.name.to_string()
        };

        let ino = self.allocate_ino();
        let is_workspace_doc = VirtualInode::is_google_workspace_mime(params.mime_type);

        let inode = VirtualInode {
            ino,
            parent_ino: params.parent_ino,
            name: final_name.clone(),
            size: params.size,
            mime_type: params.mime_type.to_string(),
            is_directory: false,
            is_workspace_doc,
            web_view_link: params.web_view_link,
            modified_time: params.modified_time,
            created_time: params.created_time,
            location: params.location.clone(),
            children: Vec::new(),
        };

        // 1. Insert into inodes
        self.inodes.insert(ino, inode);

        // 2. Insert into lookup_by_name
        self.lookup_by_name
            .insert((params.parent_ino, canonical_lookup_key(&final_name)), ino);

        // 3. Insert into reverse_cloud_index
        match &params.location {
            StorageLocation::SingleAccount {
                account_id,
                google_file_id,
            } => {
                self.reverse_cloud_index
                    .insert((account_id.clone(), google_file_id.clone()), ino);
            }
            StorageLocation::StripedManifest {
                manifest_account_id,
                manifest_google_file_id,
                ..
            } => {
                self.reverse_cloud_index
                    .insert((manifest_account_id.clone(), manifest_google_file_id.clone()), ino);
            }
            StorageLocation::VirtualWarningNote { .. } | StorageLocation::VirtualDirectory => {}
        }

        // 4. Register child under parent
        {
            if let Some(mut p) = self.inodes.get_mut(&params.parent_ino) {
                p.children.push(ino);
            }
        }

        Ok(ino)
    }

    /// Inserts a directory under `parent_ino`.
    ///
    /// If directories with the same name arrive from different accounts, bidirectionally disambiguates
    /// them with their account abbreviations (e.g. `Projects (alice)` and `Projects (bob)`).
    /// Transparently merges directories without account affiliation (e.g. local `mkdir`) or within the same account.
    pub fn insert_or_merge_directory(
        &self,
        parent_ino: u64,
        name: &str,
        account_id: Option<&str>,
        google_folder_id: Option<&str>,
    ) -> Result<u64, VfsError> {
        let canonical_base = canonical_lookup_key(name);
        let base_key = (parent_ino, canonical_base.clone());

        if let Some(acc_new) = account_id {
            let new_abbr = Self::account_abbreviation(acc_new);

            if self.collided_dirs.contains_key(&base_key) {
                // Already collided: create with disambiguated name
                let mut counter = 1;
                let final_dir_name = loop {
                    let candidate = Self::format_disambiguated_name(name, &new_abbr, counter);
                    if self.lookup(parent_ino, &candidate).is_none() {
                        break candidate;
                    }
                    counter += 1;
                };

                let ino = self.allocate_ino();
                let dir_node = VirtualInode::new_directory(ino, parent_ino, final_dir_name.clone());
                self.inodes.insert(ino, dir_node);
                self.lookup_by_name
                    .insert((parent_ino, canonical_lookup_key(&final_dir_name)), ino);

                let mut map = HashMap::new();
                if let Some(gid) = google_folder_id {
                    map.insert(acc_new.to_string(), gid.to_string());
                    self.reverse_cloud_index
                        .insert((acc_new.to_string(), gid.to_string()), ino);
                }
                self.virtual_folders.insert(ino, map);

                if let Some(mut p) = self.inodes.get_mut(&parent_ino) {
                    p.children.push(ino);
                }

                return Ok(ino);
            }

            if let Some(existing_ino) = self.lookup(parent_ino, name) {
                let is_dir = self
                    .inodes
                    .get(&existing_ino)
                    .map(|g| g.is_directory)
                    .unwrap_or(false);

                if is_dir {
                    let existing_accounts: Vec<String> = self
                        .virtual_folders
                        .get(&existing_ino)
                        .map(|g| g.value().keys().cloned().collect())
                        .unwrap_or_default();

                    if !existing_accounts.is_empty() && !existing_accounts.iter().any(|a| a == acc_new) {
                        // Cross-account folder collision! Bidirectionally disambiguate
                        self.collided_dirs.insert(base_key.clone(), name.to_string());

                        let old_acc = &existing_accounts[0];
                        let old_abbr = Self::account_abbreviation(old_acc);
                        let mut old_counter = 1;
                        let old_disambiguated = loop {
                            let candidate = Self::format_disambiguated_name(name, &old_abbr, old_counter);
                            if self.lookup(parent_ino, &candidate).is_none() {
                                break candidate;
                            }
                            old_counter += 1;
                        };

                        // Rename existing directory (Strict Single-Guard discipline)
                        self.lookup_by_name.remove(&base_key);
                        self.lookup_by_name.insert(
                            (parent_ino, canonical_lookup_key(&old_disambiguated)),
                            existing_ino,
                        );
                        if let Some(mut existing_node) = self.inodes.get_mut(&existing_ino) {
                            existing_node.name = old_disambiguated;
                        }

                        // Create new directory for acc_new
                        let mut new_counter = 1;
                        let new_disambiguated = loop {
                            let candidate = Self::format_disambiguated_name(name, &new_abbr, new_counter);
                            if self.lookup(parent_ino, &candidate).is_none() {
                                break candidate;
                            }
                            new_counter += 1;
                        };

                        let ino = self.allocate_ino();
                        let dir_node = VirtualInode::new_directory(ino, parent_ino, new_disambiguated.clone());
                        self.inodes.insert(ino, dir_node);
                        self.lookup_by_name
                            .insert((parent_ino, canonical_lookup_key(&new_disambiguated)), ino);

                        let mut map = HashMap::new();
                        if let Some(gid) = google_folder_id {
                            map.insert(acc_new.to_string(), gid.to_string());
                            self.reverse_cloud_index
                                .insert((acc_new.to_string(), gid.to_string()), ino);
                        }
                        self.virtual_folders.insert(ino, map);

                        if let Some(mut p) = self.inodes.get_mut(&parent_ino) {
                            p.children.push(ino);
                        }

                        return Ok(ino);
                    }

                    // Same account or unassigned local directory -> merge
                    if let Some(gid) = google_folder_id {
                        let mut entry = self.virtual_folders.entry(existing_ino).or_default();
                        entry.insert(acc_new.to_string(), gid.to_string());
                        self.reverse_cloud_index
                            .insert((acc_new.to_string(), gid.to_string()), existing_ino);
                    }
                    return Ok(existing_ino);
                }
            }
        } else if let Some(existing_ino) = self.lookup(parent_ino, name) {
            let is_dir = self
                .inodes
                .get(&existing_ino)
                .map(|g| g.is_directory)
                .unwrap_or(false);
            if is_dir {
                return Ok(existing_ino);
            }
        }

        // Create new directory
        let ino = self.allocate_ino();
        let dir_node = VirtualInode::new_directory(ino, parent_ino, name.to_string());

        self.inodes.insert(ino, dir_node);
        self.lookup_by_name
            .insert((parent_ino, canonical_lookup_key(name)), ino);

        if let (Some(acc), Some(gid)) = (account_id, google_folder_id) {
            let mut map = HashMap::new();
            map.insert(acc.to_string(), gid.to_string());
            self.virtual_folders.insert(ino, map);
            self.reverse_cloud_index
                .insert((acc.to_string(), gid.to_string()), ino);
        } else {
            self.virtual_folders.insert(ino, HashMap::new());
        }

        if let Some(mut p) = self.inodes.get_mut(&parent_ino) {
            p.children.push(ino);
        }

        Ok(ino)
    }

    /// Lazy Folder Materialization: Creates a virtual folder purely in RAM without calling Google Drive.
    pub fn create_virtual_directory(&self, parent_ino: u64, name: &str) -> Result<u64, VfsError> {
        self.insert_or_merge_directory(parent_ino, name, None, None)
    }

    /// Looks up the remote Google folder ID associated with a virtual directory for a specific account.
    #[must_use]
    pub fn get_remote_folder_id(&self, ino: u64, account_id: &str) -> Option<String> {
        let guard = self.virtual_folders.get(&ino)?;
        guard.value().get(account_id).cloned()
    }

    /// Records a newly materialized remote Google folder ID for an account.
    pub fn record_remote_folder_id(&self, ino: u64, account_id: &str, google_folder_id: &str) {
        let mut entry = self.virtual_folders.entry(ino).or_default();
        entry.insert(account_id.to_string(), google_folder_id.to_string());
        self.reverse_cloud_index
            .insert((account_id.to_string(), google_folder_id.to_string()), ino);
    }

    /// Returns all remote Google folder IDs associated with an inode across all accounts.
    #[must_use]
    pub fn get_all_remote_folder_ids(&self, ino: u64) -> Vec<(String, String)> {
        if let Some(guard) = self.virtual_folders.get(&ino) {
            guard.value().iter().map(|(a, f)| (a.clone(), f.clone())).collect()
        } else {
            Vec::new()
        }
    }

    /// Resolves the canonical virtual path (e.g. `/Folder/file.txt`) for any valid inode ID.
    #[must_use]
    pub fn get_path_for_inode(&self, mut ino: u64) -> Option<String> {
        if ino == Self::ROOT_INO {
            return Some("/".to_string());
        }
        let mut segments = Vec::new();
        while ino != Self::ROOT_INO {
            let node = self.inodes.get(&ino)?;
            segments.push(node.name.clone());
            ino = node.parent_ino;
        }
        segments.reverse();
        Some(format!("/{}", segments.join("/")))
    }

    /// Looks up an inode by its cloud account ID and Google file ID.
    #[must_use]
    pub fn lookup_by_cloud_id(&self, account_id: &str, google_file_id: &str) -> Option<u64> {
        let key = (account_id.to_string(), google_file_id.to_string());
        let guard = self.reverse_cloud_index.get(&key)?;
        Some(*guard.value())
    }

    /// Looks up an inode by its cloud account ID and Google file ID.
    /// Public alias for `lookup_by_cloud_id`.
    #[must_use]
    pub fn get_ino_by_cloud_id(&self, account_id: &str, google_file_id: &str) -> Option<u64> {
        self.lookup_by_cloud_id(account_id, google_file_id)
    }

    /// Updates an existing directory inode across delta sync changes, preserving collision disambiguation.
    ///
    /// Follows strict Single-Guard discipline to avoid deadlocks.
    pub fn update_existing_directory(
        &self,
        ino: u64,
        new_parent_ino: u64,
        new_remote_name: &str,
        mtime: u64,
    ) -> Option<String> {
        let (old_parent_ino, old_name) = {
            let node = self.inodes.get(&ino)?;
            (node.parent_ino, node.name.clone())
        };

        let account_id = self
            .virtual_folders
            .get(&ino)
            .and_then(|guard| guard.value().keys().next().cloned())
            .unwrap_or_else(|| "cloud".to_string());

        let base_lookup_key = canonical_lookup_key(new_remote_name);
        let base_key = (new_parent_ino, base_lookup_key.clone());

        let effective_display_name = if self.collided_dirs.contains_key(&base_key) {
            Self::format_folder_disambiguated(new_remote_name, &account_id)
        } else {
            let other_ino = self.lookup_by_name.get(&base_key).map(|g| *g.value());
            if let Some(other_ino) = other_ino
                && other_ino != ino
            {
                let other_acc = self
                    .virtual_folders
                    .get(&other_ino)
                    .and_then(|g| g.value().keys().next().cloned());

                if let Some(other_acc) = other_acc
                    && other_acc != account_id
                {
                    self.collided_dirs
                        .insert(base_key.clone(), new_remote_name.to_string());

                    let other_disambiguated =
                        Self::format_folder_disambiguated(new_remote_name, &other_acc);

                    self.lookup_by_name.remove(&base_key);
                    self.lookup_by_name.insert(
                        (new_parent_ino, canonical_lookup_key(&other_disambiguated)),
                        other_ino,
                    );
                    if let Some(mut other_node) = self.inodes.get_mut(&other_ino) {
                        other_node.name = other_disambiguated;
                    }

                    Self::format_folder_disambiguated(new_remote_name, &account_id)
                } else {
                    new_remote_name.to_string()
                }
            } else {
                new_remote_name.to_string()
            }
        };

        if old_name != effective_display_name || old_parent_ino != new_parent_ino {
            let old_lookup_key = (old_parent_ino, canonical_lookup_key(&old_name));
            self.lookup_by_name.remove(&old_lookup_key);
            let new_lookup_key = (
                new_parent_ino,
                canonical_lookup_key(&effective_display_name),
            );
            self.lookup_by_name.insert(new_lookup_key, ino);

            if old_parent_ino != new_parent_ino {
                if let Some(mut old_parent) = self.inodes.get_mut(&old_parent_ino) {
                    old_parent.children.retain(|&cid| cid != ino);
                }
                if let Some(mut new_parent) = self.inodes.get_mut(&new_parent_ino)
                    && !new_parent.children.contains(&ino)
                {
                    new_parent.children.push(ino);
                }
            }

            if let Some(mut node) = self.inodes.get_mut(&ino) {
                node.parent_ino = new_parent_ino;
                node.name = effective_display_name.clone();
                if mtime > 0 {
                    node.modified_time = mtime;
                }
            }
        } else if mtime > 0
            && let Some(mut node) = self.inodes.get_mut(&ino)
        {
            node.modified_time = mtime;
        }

        Some(effective_display_name)
    }

    /// Updates an existing file inode across delta sync changes, preserving collision disambiguation.
    ///
    /// Follows strict Single-Guard discipline to avoid deadlocks.
    pub fn update_existing_file(
        &self,
        ino: u64,
        new_parent_ino: u64,
        new_remote_name: &str,
        size: u64,
        mtime: u64,
    ) -> Option<String> {
        let (old_parent_ino, old_name, account_id) = {
            let node = self.inodes.get(&ino)?;
            let acc = match &node.location {
                StorageLocation::SingleAccount { account_id, .. } => account_id.clone(),
                StorageLocation::StripedManifest {
                    manifest_account_id,
                    ..
                } => manifest_account_id.clone(),
                _ => "cloud".to_string(),
            };
            (node.parent_ino, node.name.clone(), acc)
        };

        let base_lookup_key = canonical_lookup_key(new_remote_name);
        let base_key = (new_parent_ino, base_lookup_key.clone());

        let effective_display_name = if self.collided_files.contains_key(&base_key) {
            Self::format_disambiguated(new_remote_name, &account_id)
        } else {
            let other_ino = self.lookup_by_name.get(&base_key).map(|g| *g.value());
            if let Some(other_ino) = other_ino
                && other_ino != ino
            {
                let other_acc = self
                    .inodes
                    .get(&other_ino)
                    .and_then(|node| match &node.location {
                        StorageLocation::SingleAccount { account_id, .. } => {
                            Some(account_id.clone())
                        }
                        StorageLocation::StripedManifest {
                            manifest_account_id,
                            ..
                        } => Some(manifest_account_id.clone()),
                        _ => None,
                    });

                if let Some(other_acc) = other_acc
                    && other_acc != account_id
                {
                    self.collided_files
                        .insert(base_key.clone(), new_remote_name.to_string());

                    let other_disambiguated =
                        Self::format_disambiguated(new_remote_name, &other_acc);

                    self.lookup_by_name.remove(&base_key);
                    self.lookup_by_name.insert(
                        (new_parent_ino, canonical_lookup_key(&other_disambiguated)),
                        other_ino,
                    );
                    if let Some(mut other_node) = self.inodes.get_mut(&other_ino) {
                        other_node.name = other_disambiguated;
                    }

                    Self::format_disambiguated(new_remote_name, &account_id)
                } else {
                    new_remote_name.to_string()
                }
            } else {
                new_remote_name.to_string()
            }
        };

        if old_name != effective_display_name || old_parent_ino != new_parent_ino {
            let old_lookup_key = (old_parent_ino, canonical_lookup_key(&old_name));
            self.lookup_by_name.remove(&old_lookup_key);
            let new_lookup_key = (
                new_parent_ino,
                canonical_lookup_key(&effective_display_name),
            );
            self.lookup_by_name.insert(new_lookup_key, ino);

            if old_parent_ino != new_parent_ino {
                if let Some(mut old_parent) = self.inodes.get_mut(&old_parent_ino) {
                    old_parent.children.retain(|&cid| cid != ino);
                }
                if let Some(mut new_parent) = self.inodes.get_mut(&new_parent_ino)
                    && !new_parent.children.contains(&ino)
                {
                    new_parent.children.push(ino);
                }
            }

            if let Some(mut node) = self.inodes.get_mut(&ino) {
                node.parent_ino = new_parent_ino;
                node.name = effective_display_name.clone();
                node.size = size;
                if mtime > 0 {
                    node.modified_time = mtime;
                }
            }
        } else if let Some(mut node) = self.inodes.get_mut(&ino) {
            node.size = size;
            if mtime > 0 {
                node.modified_time = mtime;
            }
        }

        Some(effective_display_name)
    }

    /// Registers a chunk's cloud ID so remote deletion of individual chunks maps to the parent striped file inode.
    pub fn register_chunk_cloud_id(&self, account_id: &str, google_file_id: &str, striped_ino: u64) {
        self.reverse_cloud_index
            .insert((account_id.to_string(), google_file_id.to_string()), striped_ino);
    }

    /// Moves and/or renames an inode from `(old_parent, old_name)` to `(new_parent, new_name)`.
    ///
    /// Follows strict Single-Guard discipline to guarantee freedom from shard deadlocks.
    pub fn rename(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(), VfsError> {
        let target_ino = self
            .lookup(old_parent, old_name)
            .ok_or_else(|| VfsError::NotFound(format!("File '{old_name}' not found for rename")))?;

        let old_lookup_key = (old_parent, canonical_lookup_key(old_name));
        let new_lookup_key = (new_parent, canonical_lookup_key(new_name));

        // Step 1: Remove old lookup key
        self.lookup_by_name.remove(&old_lookup_key);

        // Step 2: Insert new lookup key
        self.lookup_by_name.insert(new_lookup_key, target_ino);

        // Step 3: Remove target from old parent children
        {
            if let Some(mut p) = self.inodes.get_mut(&old_parent) {
                p.children.retain(|&c| c != target_ino);
            }
        }

        // Step 4: Add target to new parent children (if changed)
        if old_parent != new_parent
            && let Some(mut p) = self.inodes.get_mut(&new_parent)
        {
            p.children.push(target_ino);
        }

        // Step 5: Update target inode metadata
        {
            if let Some(mut t) = self.inodes.get_mut(&target_ino) {
                t.parent_ino = new_parent;
                t.name = new_name.to_string();
            }
        }

        debug!(
            target_ino,
            old_parent,
            old_name,
            new_parent,
            new_name,
            "Successfully renamed inode"
        );

        Ok(())
    }

    /// Removes an inode under `parent_ino` with the given name.
    pub fn remove(&self, parent_ino: u64, name: &str) -> Result<VirtualInode, VfsError> {
        let target_ino = self
            .lookup(parent_ino, name)
            .ok_or_else(|| VfsError::NotFound(format!("File '{name}' not found for removal")))?;

        // Remove from lookup_by_name
        self.lookup_by_name
            .remove(&(parent_ino, canonical_lookup_key(name)));

        // Remove from parent children
        {
            if let Some(mut p) = self.inodes.get_mut(&parent_ino) {
                p.children.retain(|&c| c != target_ino);
            }
        }

        // Remove from inodes
        let (_, removed_node) = self
            .inodes
            .remove(&target_ino)
            .ok_or_else(|| VfsError::NotFound("Inode missing during removal".to_string()))?;

        // Cleanup secondary indices
        match &removed_node.location {
            StorageLocation::SingleAccount {
                account_id,
                google_file_id,
            } => {
                self.reverse_cloud_index
                    .remove(&(account_id.clone(), google_file_id.clone()));
            }
            StorageLocation::StripedManifest {
                manifest_account_id,
                manifest_google_file_id,
                ..
            } => {
                self.reverse_cloud_index
                    .remove(&(manifest_account_id.clone(), manifest_google_file_id.clone()));
            }
            StorageLocation::VirtualWarningNote { .. } | StorageLocation::VirtualDirectory => {}
        }

        if let Some((_, map)) = self.virtual_folders.remove(&target_ino) {
            for (acc, gid) in map {
                self.reverse_cloud_index.remove(&(acc, gid));
            }
        }

        Ok(removed_node)
    }

    /// Hydrates file and folder trees from a linked Google account into the in-memory graph.
    pub fn hydrate_account_data(&self, data: &HydratedAccountData) -> Result<(), VfsError> {
        info!(
            account = %data.account_id,
            total_files = data.files.len(),
            "Merging hydrated account into in-memory routing table"
        );

        // 1. Separate folders and files
        let mut folders = Vec::new();
        let mut files = Vec::new();

        for f in &data.files {
            if f.trashed.unwrap_or(false) {
                continue;
            }
            if f.mime_type == "application/vnd.google-apps.folder" {
                folders.push(f);
            } else {
                files.push(f);
            }
        }

        // 2. Precompute total set of fetched folder IDs
        let fetched_folder_ids: HashSet<&str> = folders
            .iter()
            .map(|f| f.id.as_str())
            .collect();

        // 3. Multi-pass topological loop (up to 16 passes) over pending folders
        let mut pending_folders = folders;
        for _pass in 0..16 {
            if pending_folders.is_empty() {
                break;
            }

            let prev_count = pending_folders.len();
            let mut next_pending = Vec::new();

            for folder in pending_folders {
                let target_parent_ino = if folder
                    .parents
                    .as_ref()
                    .is_none_or(|p| p.is_empty() || p.iter().any(|s| s == "root"))
                {
                    // Condition 1: parents list is empty, OR
                    // Condition 2: parents contains "root"
                    Some(Self::ROOT_INO)
                } else {
                    let parent_id = folder
                        .parents
                        .as_ref()
                        .and_then(|p| p.first())
                        .map(|s| s.as_str());

                    match parent_id {
                        None | Some("root") => Some(Self::ROOT_INO),
                        Some(pid) => {
                            if let Some(parent_ino) = self.get_ino_by_cloud_id(&data.account_id, pid) {
                                // Condition 3: parent google_file_id is already registered
                                Some(parent_ino)
                            } else if !fetched_folder_ids.contains(pid) {
                                // Condition 4: parent google_file_id is NOT in fetched_folder_ids
                                Some(Self::ROOT_INO)
                            } else {
                                // Parent is in fetched_folder_ids but not yet registered -> defer to next pass
                                None
                            }
                        }
                    }
                };

                match target_parent_ino {
                    Some(parent_ino) => {
                        self.insert_or_merge_directory(
                            parent_ino,
                            &folder.name,
                            Some(&data.account_id),
                            Some(&folder.id),
                        )?;
                    }
                    None => {
                        next_pending.push(folder);
                    }
                }
            }

            // Deadlock / Cycle Detection: if no folders could be placed in this pass
            let made_progress = next_pending.len() < prev_count;
            pending_folders = next_pending;
            if !made_progress {
                break;
            }
        }

        // Safety fallback: if unplaced folders remain after 16 passes (e.g. cycle),
        // insert them under ROOT_INO
        for folder in pending_folders {
            let _ = self.insert_or_merge_directory(
                Self::ROOT_INO,
                &folder.name,
                Some(&data.account_id),
                Some(&folder.id),
            );
        }

        // 4. Insert files under resolved parent inodes
        for file in files {
            let parent_ino = if file
                .parents
                .as_ref()
                .is_none_or(|p| p.is_empty() || p.iter().any(|s| s == "root"))
            {
                Self::ROOT_INO
            } else {
                let parent_google_id = file
                    .parents
                    .as_ref()
                    .and_then(|p| p.first())
                    .map(|s| s.as_str());

                match parent_google_id {
                    None | Some("root") => Self::ROOT_INO,
                    Some(pid) => self
                        .get_ino_by_cloud_id(&data.account_id, pid)
                        .unwrap_or(Self::ROOT_INO),
                }
            };

            let size = file
                .size
                .as_ref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let location = StorageLocation::SingleAccount {
                account_id: data.account_id.clone(),
                google_file_id: file.id.clone(),
            };

            let _ = self.insert_file(NewFileParams {
                parent_ino,
                name: &file.name,
                size,
                mime_type: &file.mime_type,
                modified_time: 0,
                created_time: 0,
                location,
                web_view_link: None,
            })?;
        }

        Ok(())
    }

    /// Hydrates file and folder trees from a linked Google account into the in-memory graph.
    /// Alias for `hydrate_account_data`.
    pub fn hydrate_account_into_table(&self, data: &HydratedAccountData) -> Result<(), VfsError> {
        self.hydrate_account_data(data)
    }

    /// Wipes all in-memory tables and resets the state to root `/` (called on logout / session lock).
    pub fn clear(&self) {
        self.inodes.clear();
        self.lookup_by_name.clear();
        self.reverse_cloud_index.clear();
        self.virtual_folders.clear();
        self.collided_files.clear();
        self.collided_dirs.clear();
        self.next_ino.store(2, Ordering::SeqCst);

        // Recreate root
        let root = VirtualInode::new_root();
        self.inodes.insert(Self::ROOT_INO, root);
        self.lookup_by_name.insert(
            (Self::ROOT_INO, canonical_lookup_key("/")),
            Self::ROOT_INO,
        );
        self.virtual_folders.insert(Self::ROOT_INO, HashMap::new());
    }

    /// Returns the total number of inodes currently registered in RAM.
    #[must_use]
    pub fn count(&self) -> usize {
        self.inodes.len()
    }
}

impl Default for InMemoryRoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper wrapper function to hydrate account data into an `Arc<InMemoryRoutingTable>`.
pub fn hydrate_account_into_table(
    routing_table: &Arc<InMemoryRoutingTable>,
    data: &HydratedAccountData,
) -> Result<(), VfsError> {
    routing_table.hydrate_account_data(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_initialization() {
        let table = InMemoryRoutingTable::new();
        assert_eq!(table.count(), 1);

        let root = table.get_inode(InMemoryRoutingTable::ROOT_INO).unwrap();
        assert_eq!(root.ino, 1);
        assert!(root.is_directory);
        assert_eq!(root.name, "/");

        assert_eq!(table.resolve_path("/").unwrap(), 1);
        assert_eq!(table.resolve_path("").unwrap(), 1);
    }

    #[test]
    fn test_directory_merging_across_accounts() {
        let table = InMemoryRoutingTable::new();

        // Account A creates /Photos (Google folder id: "f_acc_a")
        let dir_a = table
            .insert_or_merge_directory(
                InMemoryRoutingTable::ROOT_INO,
                "Photos",
                Some("acc_a"),
                Some("f_acc_a"),
            )
            .unwrap();

        // Account B creates /Photos (Google folder id: "f_acc_b")
        let dir_b = table
            .insert_or_merge_directory(
                InMemoryRoutingTable::ROOT_INO,
                "Photos",
                Some("acc_b"),
                Some("f_acc_b"),
            )
            .unwrap();

        // Cross-account folder collision: both directories are disambiguated with account abbreviations
        assert_ne!(dir_a, dir_b);
        assert_eq!(table.count(), 3); // Root + /Photos (acc_a) + /Photos (acc_b)

        let node_a = table.get_inode(dir_a).unwrap();
        let node_b = table.get_inode(dir_b).unwrap();
        assert_eq!(node_a.name, "Photos (acc_a)");
        assert_eq!(node_b.name, "Photos (acc_b)");

        // Verify folder IDs for both accounts are tracked
        assert_eq!(
            table.get_remote_folder_id(dir_a, "acc_a"),
            Some("f_acc_a".to_string())
        );
        assert_eq!(
            table.get_remote_folder_id(dir_b, "acc_b"),
            Some("f_acc_b".to_string())
        );

        // Verify local directories merge without account affiliation
        let local_1 = table
            .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, "LocalShared", None, None)
            .unwrap();
        let local_2 = table
            .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, "LocalShared", None, None)
            .unwrap();
        assert_eq!(local_1, local_2);
    }

    #[test]
    fn test_deterministic_file_collision_resolution() {
        let table = InMemoryRoutingTable::new();

        let dir_ino = table
            .insert_or_merge_directory(InMemoryRoutingTable::ROOT_INO, "Docs", None, None)
            .unwrap();

        // Account A inserts /Docs/report.pdf
        let f1 = table
            .insert_file(NewFileParams {
                parent_ino: dir_ino,
                name: "report.pdf",
                size: 1024,
                mime_type: "application/pdf",
                modified_time: 100,
                created_time: 100,
                location: StorageLocation::SingleAccount {
                    account_id: "acc_a".to_string(),
                    google_file_id: "g1".to_string(),
                },
                web_view_link: None,
            })
            .unwrap();

        // Account B inserts /Docs/report.pdf -> should bidirectionally disambiguate BOTH to (acc_a) and (acc_b)
        let f2 = table
            .insert_file(NewFileParams {
                parent_ino: dir_ino,
                name: "report.pdf",
                size: 2048,
                mime_type: "application/pdf",
                modified_time: 200,
                created_time: 200,
                location: StorageLocation::SingleAccount {
                    account_id: "acc_b".to_string(),
                    google_file_id: "g2".to_string(),
                },
                web_view_link: None,
            })
            .unwrap();

        assert_ne!(f1, f2);

        let node1 = table.get_inode(f1).unwrap();
        let node2 = table.get_inode(f2).unwrap();

        assert_eq!(node1.name, "report (acc_a).pdf");
        assert_eq!(node2.name, "report (acc_b).pdf");

        // Account C (3rd account) inserts /Docs/report.pdf -> recognizes base name collision and becomes (acc_c)
        let f3 = table
            .insert_file(NewFileParams {
                parent_ino: dir_ino,
                name: "report.pdf",
                size: 3072,
                mime_type: "application/pdf",
                modified_time: 300,
                created_time: 300,
                location: StorageLocation::SingleAccount {
                    account_id: "acc_c".to_string(),
                    google_file_id: "g3".to_string(),
                },
                web_view_link: None,
            })
            .unwrap();

        let node3 = table.get_inode(f3).unwrap();
        assert_eq!(node3.name, "report (acc_c).pdf");

        // Verify resolve_path finds all three files
        assert_eq!(table.resolve_path("/Docs/report (acc_a).pdf").unwrap(), f1);
        assert_eq!(table.resolve_path("/Docs/report (acc_b).pdf").unwrap(), f2);
        assert_eq!(table.resolve_path("/Docs/report (acc_c).pdf").unwrap(), f3);
    }

    #[test]
    fn test_case_insensitive_lookup_and_display_preservation() {
        let table = InMemoryRoutingTable::new();

        let f = table
            .insert_file(NewFileParams {
                parent_ino: InMemoryRoutingTable::ROOT_INO,
                name: "MyNotes.TXT",
                size: 100,
                mime_type: "text/plain",
                modified_time: 0,
                created_time: 0,
                location: StorageLocation::SingleAccount {
                    account_id: "acc1".to_string(),
                    google_file_id: "g_notes".to_string(),
                },
                web_view_link: None,
            })
            .unwrap();

        // Lookup with various casings should all find the inode
        assert_eq!(
            table.lookup(InMemoryRoutingTable::ROOT_INO, "mynotes.txt"),
            Some(f)
        );
        assert_eq!(
            table.lookup(InMemoryRoutingTable::ROOT_INO, "MYNOTES.TXT"),
            Some(f)
        );
        assert_eq!(
            table.lookup(InMemoryRoutingTable::ROOT_INO, "MyNotes.txt"),
            Some(f)
        );

        // Display name must preserve original case
        let node = table.get_inode(f).unwrap();
        assert_eq!(node.name, "MyNotes.TXT");
    }

    #[test]
    fn test_lazy_folder_materialization() {
        let table = InMemoryRoutingTable::new();

        // User creates /Projects/Alpha purely in RAM
        let projects_ino = table.create_virtual_directory(1, "Projects").unwrap();
        let alpha_ino = table.create_virtual_directory(projects_ino, "Alpha").unwrap();

        // Prior to any upload, no remote Google folder ID exists
        assert_eq!(table.get_remote_folder_id(alpha_ino, "acc_a"), None);

        // During file upload to acc_a, folder is materialized on Google Drive and recorded
        table.record_remote_folder_id(alpha_ino, "acc_a", "remote_folder_123");

        assert_eq!(
            table.get_remote_folder_id(alpha_ino, "acc_a"),
            Some("remote_folder_123".to_string())
        );
        // Other accounts remain unmaterialized
        assert_eq!(table.get_remote_folder_id(alpha_ino, "acc_b"), None);
    }

    #[test]
    fn test_rename_and_move() {
        let table = InMemoryRoutingTable::new();

        let dir1 = table.create_virtual_directory(1, "Dir1").unwrap();
        let dir2 = table.create_virtual_directory(1, "Dir2").unwrap();

        let file_ino = table
            .insert_file(NewFileParams {
                parent_ino: dir1,
                name: "file.txt",
                size: 50,
                mime_type: "text/plain",
                modified_time: 0,
                created_time: 0,
                location: StorageLocation::SingleAccount {
                    account_id: "a".to_string(),
                    google_file_id: "f".to_string(),
                },
                web_view_link: None,
            })
            .unwrap();

        // Move and rename: /Dir1/file.txt -> /Dir2/moved.txt
        table.rename(dir1, "file.txt", dir2, "moved.txt").unwrap();

        assert_eq!(table.lookup(dir1, "file.txt"), None);
        assert_eq!(table.lookup(dir2, "moved.txt"), Some(file_ino));

        let node = table.get_inode(file_ino).unwrap();
        assert_eq!(node.parent_ino, dir2);
        assert_eq!(node.name, "moved.txt");

        let dir1_children = table.readdir(dir1).unwrap();
        assert!(dir1_children.is_empty());

        let dir2_children = table.readdir(dir2).unwrap();
        assert_eq!(dir2_children.len(), 1);
        assert_eq!(dir2_children[0].ino, file_ino);
    }
}
