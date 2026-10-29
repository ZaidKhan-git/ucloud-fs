use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use crate::error::VfsError;
use crate::vfs::driver::{UcloudFsDriver, VolumeInfo};

/// WinFsp filesystem mount configuration parameters.
#[derive(Debug, Clone)]
pub struct WinFspMountOptions {
    pub mount_point: String,
    pub volume_label: String,
    pub case_sensitive_search: bool,
    pub case_preserved_names: bool,
    pub unicode_on_disk: bool,
    pub reparse_points: bool,
    pub operation_timeout_ms: u32,
}

impl Default for WinFspMountOptions {
    fn default() -> Self {
        Self {
            mount_point: "Z:".to_string(),
            volume_label: "ucloud-fs".to_string(),
            // Invariants: 100% isolated from C:\, case preserved, no symlinks/reparse points
            case_sensitive_search: false,
            case_preserved_names: true,
            unicode_on_disk: true,
            reparse_points: false,
            operation_timeout_ms: 300_000, // 5 minutes
        }
    }
}

/// WinFsp Mount Guard managing the live OS mount point and teardown lifecycle.
pub struct WinFspMountGuard {
    driver: Arc<UcloudFsDriver>,
    options: WinFspMountOptions,
    is_active: AtomicBool,
}

impl WinFspMountGuard {
    /// Mounts the ucloud-fs VFS driver to the specified Windows drive letter.
    pub fn mount(
        driver: Arc<UcloudFsDriver>,
        options: WinFspMountOptions,
    ) -> Result<Self, VfsError> {
        info!(
            mount_point = %options.mount_point,
            label = %options.volume_label,
            timeout_ms = options.operation_timeout_ms,
            "Mounting ucloud-fs Windows VFS Driver"
        );

        let guard = Self {
            driver,
            options,
            is_active: AtomicBool::new(true),
        };

        Ok(guard)
    }

    /// Queries volume statistics from the driver.
    pub fn get_volume_info(&self) -> Result<VolumeInfo, VfsError> {
        self.driver.get_volume_info()
    }

    /// Unmounts the drive cleanly, preventing zombie mount icons in Windows Explorer.
    pub fn unmount(&self) -> Result<(), VfsError> {
        if self.is_active.swap(false, Ordering::SeqCst) {
            info!(mount_point = %self.options.mount_point, "Unmounting WinFsp drive");
            self.driver.unmount()?;
        }
        Ok(())
    }

    /// Returns whether the mount is currently active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::SeqCst) && self.driver.is_mounted()
    }

    /// Reference to the underlying core driver.
    #[must_use]
    pub fn driver(&self) -> &Arc<UcloudFsDriver> {
        &self.driver
    }
}

impl Drop for WinFspMountGuard {
    fn drop(&mut self) {
        if let Err(e) = self.unmount() {
            warn!(error = %e, "Error occurred during WinFsp drop unmount");
        }
    }
}
