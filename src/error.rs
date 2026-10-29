use thiserror::Error;

/// Windows NTSTATUS error codes
pub mod ntstatus {
    pub const STATUS_SUCCESS: i32 = 0x0000_0000;
    pub const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;
    pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
    pub const STATUS_DISK_FULL: i32 = 0xC000_007F_u32 as i32;
    pub const STATUS_DEVICE_OFF_LINE: i32 = 0xC000_008A_u32 as i32;
    pub const STATUS_DEVICE_BUSY: i32 = 0x8000_0011_u32 as i32;
    pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BB_u32 as i32;
    pub const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9_u32 as i32;
    pub const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101_u32 as i32;
    pub const STATUS_FILE_DELETED: i32 = 0xC000_0123_u32 as i32;
}

/// POSIX errno constants
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const ENOSPC: i32 = 28;
    pub const ENOTSUP: i32 = 95;
    pub const ENETDOWN: i32 = 100;
}

/// Unified VfsError taxonomy with deterministic mappings to NTSTATUS and POSIX errno.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VfsError {
    #[error("Session is locked or expired")]
    SessionLocked,

    #[error("File or directory not found: {0}")]
    NotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Storage pool or account is full")]
    DiskFull,

    #[error("User rejected permission to chunk oversized file across multiple accounts")]
    UserCancelledChunking,

    #[error("Invalid seek or out-of-order write: expected offset {expected}, got {actual}")]
    InvalidSeekOrOutOfOrderWrite { expected: u64, actual: u64 },

    #[error("Path traversal or illegal path pattern blocked: {0}")]
    PathTraversalBlocked(String),

    #[error("NTFS alternate data stream ignored: {0}")]
    AlternateDataStreamIgnored(String),

    #[error("Rate limit timeout exceeded")]
    RateLimitTimeout,

    #[error("Network connection offline")]
    NetworkOffline,

    #[error("Internal fault: {0}")]
    InternalFault(String),
}

impl VfsError {
    /// Deterministic conversion to Windows NTSTATUS error code.
    #[must_use]
    pub const fn to_ntstatus(&self) -> i32 {
        match self {
            Self::SessionLocked => ntstatus::STATUS_ACCESS_DENIED,
            Self::NotFound(_) => ntstatus::STATUS_OBJECT_NAME_NOT_FOUND,
            Self::PermissionDenied(_) => ntstatus::STATUS_ACCESS_DENIED,
            Self::DiskFull | Self::UserCancelledChunking => ntstatus::STATUS_DISK_FULL,
            Self::InvalidSeekOrOutOfOrderWrite { .. } => ntstatus::STATUS_NOT_SUPPORTED,
            Self::PathTraversalBlocked(_) => ntstatus::STATUS_ACCESS_DENIED,
            Self::AlternateDataStreamIgnored(_) => ntstatus::STATUS_NOT_SUPPORTED,
            Self::RateLimitTimeout => ntstatus::STATUS_DEVICE_BUSY,
            Self::NetworkOffline => ntstatus::STATUS_DEVICE_OFF_LINE,
            Self::InternalFault(_) => ntstatus::STATUS_UNEXPECTED_IO_ERROR,
        }
    }

    /// Deterministic conversion to POSIX errno code.
    #[must_use]
    pub const fn to_errno(&self) -> i32 {
        match self {
            Self::SessionLocked => errno::EACCES,
            Self::NotFound(_) => errno::ENOENT,
            Self::PermissionDenied(_) => errno::EACCES,
            Self::DiskFull | Self::UserCancelledChunking => errno::ENOSPC,
            Self::InvalidSeekOrOutOfOrderWrite { .. } => errno::ENOTSUP,
            Self::PathTraversalBlocked(_) => errno::EACCES,
            Self::AlternateDataStreamIgnored(_) => errno::ENOTSUP,
            Self::RateLimitTimeout => errno::EBUSY,
            Self::NetworkOffline => errno::ENETDOWN,
            Self::InternalFault(_) => errno::EIO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_ntstatus_mapping() {
        assert_eq!(VfsError::SessionLocked.to_ntstatus(), ntstatus::STATUS_ACCESS_DENIED);
        assert_eq!(VfsError::NotFound("test.txt".into()).to_ntstatus(), ntstatus::STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(VfsError::PermissionDenied("test.txt".into()).to_ntstatus(), ntstatus::STATUS_ACCESS_DENIED);
        assert_eq!(VfsError::DiskFull.to_ntstatus(), ntstatus::STATUS_DISK_FULL);
        assert_eq!(VfsError::UserCancelledChunking.to_ntstatus(), ntstatus::STATUS_DISK_FULL);
        assert_eq!(
            VfsError::InvalidSeekOrOutOfOrderWrite { expected: 100, actual: 50 }.to_ntstatus(),
            ntstatus::STATUS_NOT_SUPPORTED
        );
        assert_eq!(VfsError::PathTraversalBlocked("..".into()).to_ntstatus(), ntstatus::STATUS_ACCESS_DENIED);
        assert_eq!(VfsError::AlternateDataStreamIgnored("file:zone".into()).to_ntstatus(), ntstatus::STATUS_NOT_SUPPORTED);
        assert_eq!(VfsError::RateLimitTimeout.to_ntstatus(), ntstatus::STATUS_DEVICE_BUSY);
        assert_eq!(VfsError::NetworkOffline.to_ntstatus(), ntstatus::STATUS_DEVICE_OFF_LINE);
        assert_eq!(VfsError::InternalFault("bug".into()).to_ntstatus(), ntstatus::STATUS_UNEXPECTED_IO_ERROR);
    }

    #[test]
    fn test_error_errno_mapping() {
        assert_eq!(VfsError::SessionLocked.to_errno(), errno::EACCES);
        assert_eq!(VfsError::NotFound("x".into()).to_errno(), errno::ENOENT);
        assert_eq!(VfsError::PermissionDenied("x".into()).to_errno(), errno::EACCES);
        assert_eq!(VfsError::DiskFull.to_errno(), errno::ENOSPC);
        assert_eq!(VfsError::UserCancelledChunking.to_errno(), errno::ENOSPC);
        assert_eq!(
            VfsError::InvalidSeekOrOutOfOrderWrite { expected: 10, actual: 0 }.to_errno(),
            errno::ENOTSUP
        );
        assert_eq!(VfsError::PathTraversalBlocked("..".into()).to_errno(), errno::EACCES);
        assert_eq!(VfsError::AlternateDataStreamIgnored("ads".into()).to_errno(), errno::ENOTSUP);
        assert_eq!(VfsError::RateLimitTimeout.to_errno(), errno::EBUSY);
        assert_eq!(VfsError::NetworkOffline.to_errno(), errno::ENETDOWN);
        assert_eq!(VfsError::InternalFault("bug".into()).to_errno(), errno::EIO);
    }
}
