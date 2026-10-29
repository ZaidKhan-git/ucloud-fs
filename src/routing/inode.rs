/// Storage location resolution for a virtual inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageLocation {
    /// Standard file residing on a single Google account.
    SingleAccount {
        account_id: String,
        google_file_id: String,
    },
    /// Multi-account striped file governed by a chunk manifest.
    StripedManifest {
        manifest_id: String,
        manifest_account_id: String,
        manifest_google_file_id: String,
    },
    /// Synthetic in-memory warning note accompanying a striped file.
    VirtualWarningNote {
        parent_striped_file_name: String,
        content: String,
    },
    /// Unified virtual directory spanning zero or more linked accounts.
    VirtualDirectory,
}

/// In-memory representation of an inode in the virtual filesystem.
#[derive(Debug, Clone)]
pub struct VirtualInode {
    /// Monotonic 64-bit inode number (1 is always virtual root `/`).
    pub ino: u64,
    /// Inode number of the parent directory (root is self-referential: 1).
    pub parent_ino: u64,
    /// Display name of the file or directory.
    pub name: String,
    /// Size in bytes (for Google Workspace native docs, virtual export size or shortcut size).
    pub size: u64,
    /// MIME type reported by Google Drive or synthetic for local virtual nodes.
    pub mime_type: String,
    /// Whether this inode represents a directory.
    pub is_directory: bool,
    /// Whether this is a Google Workspace native document (Doc, Sheet, Slides, etc.).
    pub is_workspace_doc: bool,
    /// Remote Google Drive webViewLink (for opening in browser).
    pub web_view_link: Option<String>,
    /// Last modified time as Unix epoch seconds.
    pub modified_time: u64,
    /// Creation time as Unix epoch seconds.
    pub created_time: u64,
    /// Physical location resolution enum.
    pub location: StorageLocation,
    /// Child inode IDs if this inode is a directory.
    pub children: Vec<u64>,
}

impl VirtualInode {
    /// Creates the virtual root inode (`ino = 1`).
    #[must_use]
    pub fn new_root() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            ino: 1,
            parent_ino: 1,
            name: "/".to_string(),
            size: 0,
            mime_type: "inode/directory".to_string(),
            is_directory: true,
            is_workspace_doc: false,
            web_view_link: None,
            modified_time: now,
            created_time: now,
            location: StorageLocation::VirtualDirectory,
            children: Vec::new(),
        }
    }

    /// Creates a new virtual directory under a parent inode.
    #[must_use]
    pub fn new_directory(ino: u64, parent_ino: u64, name: String) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            ino,
            parent_ino,
            name,
            size: 0,
            mime_type: "inode/directory".to_string(),
            is_directory: true,
            is_workspace_doc: false,
            web_view_link: None,
            modified_time: now,
            created_time: now,
            location: StorageLocation::VirtualDirectory,
            children: Vec::new(),
        }
    }

    /// Checks if a MIME type indicates a Google Workspace native document.
    #[must_use]
    pub fn is_google_workspace_mime(mime: &str) -> bool {
        mime.starts_with("application/vnd.google-apps.")
            && mime != "application/vnd.google-apps.folder"
    }
}

/// Parameters for inserting a new file into the routing table.
#[derive(Debug, Clone)]
pub struct NewFileParams<'a> {
    pub parent_ino: u64,
    pub name: &'a str,
    pub size: u64,
    pub mime_type: &'a str,
    pub modified_time: u64,
    pub created_time: u64,
    pub location: StorageLocation,
    pub web_view_link: Option<String>,
}
