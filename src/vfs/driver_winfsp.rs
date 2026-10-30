use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::error::VfsError;
use crate::vfs::driver::{
    DirEntry, FileInfo, UcloudFsDriver, VolumeInfo, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL,
};

// Windows NTSTATUS Codes
pub const STATUS_SUCCESS: i32 = 0x0000_0000;
pub const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
pub const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001_u32 as i32;
pub const STATUS_END_OF_FILE: i32 = 0xC000_0011_u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
pub const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101_u32 as i32;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

/// Exact `#[repr(C)]` layout of `FSP_FSCTL_VOLUME_PARAMS` matching WinFsp `fsctl.h` (504 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FspFsctlVolumeParams {
    pub version: u16,
    pub sector_size: u16,
    pub sectors_per_allocation_unit: u16,
    pub max_component_length: u16,
    pub volume_creation_time: u64,
    pub volume_serial_number: u32,
    pub transact_timeout: u32,
    pub irp_timeout: u32,
    pub irp_capacity: u32,
    pub file_info_timeout: u32,
    pub flags: u32,
    pub prefix: [u16; 192],
    pub file_system_name: [u16; 16],
    pub flags2: u32,
    pub volume_info_timeout: u32,
    pub dir_info_timeout: u32,
    pub security_timeout: u32,
    pub stream_info_timeout: u32,
    pub ea_timeout: u32,
    pub fsext_control_code: u32,
    pub reserved32: [u32; 1],
    pub reserved64: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<FspFsctlVolumeParams>() == 504);

/// Exact `#[repr(C)]` layout of `FSP_FSCTL_VOLUME_INFO` matching WinFsp `fsctl.h` (88 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FspFsctlVolumeInfo {
    pub total_size: u64,
    pub free_size: u64,
    pub volume_label_length: u16,
    pub volume_label: [u16; 32],
}

const _: () = assert!(std::mem::size_of::<FspFsctlVolumeInfo>() == 88);

/// Exact `#[repr(C)]` layout of `FSP_FSCTL_FILE_INFO` matching WinFsp `fsctl.h` (72 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FspFsctlFileInfo {
    pub file_attributes: u32,
    pub reparse_tag: u32,
    pub allocation_size: u64,
    pub file_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
    pub hard_links: u32,
    pub ea_size: u32,
}

const _: () = assert!(std::mem::size_of::<FspFsctlFileInfo>() == 72);

/// Exact `#[repr(C)]` layout of `FSP_FSCTL_DIR_INFO` matching WinFsp `fsctl.h` (104 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FspFsctlDirInfo {
    pub size: u16,
    pub _pad: [u8; 6],
    pub file_info: FspFsctlFileInfo,
    pub padding: [u8; 24],
}

const _: () = assert!(std::mem::size_of::<FspFsctlDirInfo>() == 104);

/// Representation of the WinFsp `FSP_FILE_SYSTEM` host struct.
#[repr(C)]
pub struct FspFileSystem {
    pub version: u16,
    pub _pad: [u8; 6],
    pub user_context: *mut c_void,
}

/// Exact `#[repr(C)]` callback table matching `FSP_FILE_SYSTEM_INTERFACE` in `winfsp.h` (64 pointers = 512 bytes).
#[repr(C)]
#[allow(clippy::type_complexity)]
pub struct FspFileSystemInterface {
    pub get_volume_info: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut FspFsctlVolumeInfo) -> i32>,
    pub set_volume_label: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, *mut FspFsctlVolumeInfo) -> i32>,
    pub get_security_by_name: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, *mut u32, *mut c_void, *mut usize) -> i32>,
    pub create: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, u32, u32, u32, *mut c_void, u64, *mut *mut c_void, *mut FspFsctlFileInfo) -> i32>,
    pub open: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, u32, u32, *mut *mut c_void, *mut FspFsctlFileInfo) -> i32>,
    pub overwrite: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u32, u8, u64, *mut FspFsctlFileInfo) -> i32>,
    pub cleanup: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, u32)>,
    pub close: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void)>,
    pub read: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut c_void, u64, u32, *mut u32) -> i32>,
    pub write: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const c_void, u64, u32, u8, u8, *mut u32, *mut FspFsctlFileInfo) -> i32>,
    pub flush: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut FspFsctlFileInfo) -> i32>,
    pub get_file_info: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut FspFsctlFileInfo) -> i32>,
    pub set_basic_info: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u32, u64, u64, u64, u64, *mut FspFsctlFileInfo) -> i32>,
    pub set_file_size: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u64, u8, *mut FspFsctlFileInfo) -> i32>,
    pub can_delete: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16) -> i32>,
    pub rename: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *const u16, u8) -> i32>,
    pub get_security: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut c_void, *mut usize) -> i32>,
    pub set_security: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u32, *mut c_void) -> i32>,
    pub read_directory: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *const u16, *mut c_void, u32, *mut u32) -> i32>,
    pub resolve_reparse_points: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, u32, u8, *mut c_void, *mut c_void, *mut usize) -> i32>,
    pub get_reparse_point: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *mut c_void, *mut usize) -> i32>,
    pub set_reparse_point: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *mut c_void, usize) -> i32>,
    pub delete_reparse_point: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *mut c_void, usize) -> i32>,
    pub get_stream_info: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut c_void, u32, *mut u32) -> i32>,
    pub get_dir_info_by_name: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, *mut FspFsctlDirInfo) -> i32>,
    pub control: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u32, *mut c_void, u32, *mut c_void, u32, *mut u32) -> i32>,
    pub set_delete: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *const u16, u8) -> i32>,
    pub create_ex: Option<unsafe extern "system" fn(*mut FspFileSystem, *const u16, u32, u32, u32, *mut c_void, u64, *mut c_void, u32, u8, *mut *mut c_void, *mut FspFsctlFileInfo) -> i32>,
    pub overwrite_ex: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, u32, u8, u64, *mut c_void, u32, *mut FspFsctlFileInfo) -> i32>,
    pub get_ea: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut c_void, u32, *mut u32) -> i32>,
    pub set_ea: Option<unsafe extern "system" fn(*mut FspFileSystem, *mut c_void, *mut c_void, u32, *mut FspFsctlFileInfo) -> i32>,
    pub obsolete0: Option<unsafe extern "system" fn() -> i32>,
    pub dispatcher_stopped: Option<unsafe extern "system" fn(*mut FspFileSystem, u8)>,
    pub reserved: [Option<unsafe extern "system" fn() -> i32>; 31],
}


const _: () = assert!(std::mem::size_of::<FspFileSystemInterface>() == 512);

/// Dynamic API dispatch table for `winfsp-x64.dll`.
pub struct WinFspApi {
    module: windows_sys::Win32::Foundation::HMODULE,
    pub fsp_file_system_preflight: unsafe extern "system" fn(*const u16, *const u16) -> i32,
    pub fsp_file_system_create: unsafe extern "system" fn(
        *const u16,
        *const FspFsctlVolumeParams,
        *const FspFileSystemInterface,
        *mut *mut FspFileSystem,
    ) -> i32,
    pub fsp_file_system_set_mount_point: unsafe extern "system" fn(*mut FspFileSystem, *const u16) -> i32,
    pub fsp_file_system_start_dispatcher: unsafe extern "system" fn(*mut FspFileSystem, u32) -> i32,
    pub fsp_file_system_stop_dispatcher: unsafe extern "system" fn(*mut FspFileSystem),
    pub fsp_file_system_delete: unsafe extern "system" fn(*mut FspFileSystem),
    pub fsp_file_system_add_dir_info: unsafe extern "system" fn(
        *mut FspFsctlDirInfo,
        *mut c_void,
        u32,
        *mut u32,
    ) -> u8,
    pub fsp_posix_map_permissions_to_security_descriptor:
        unsafe extern "system" fn(u32, u32, u32, *mut *mut c_void) -> i32,
    pub fsp_delete_security_descriptor:
        unsafe extern "system" fn(*mut c_void, Option<unsafe extern "system" fn() -> i32>),
}

// SAFETY: All function pointers inside WinFspApi point to thread-safe WinFsp DLL code.
unsafe impl Send for WinFspApi {}
unsafe impl Sync for WinFspApi {}

impl WinFspApi {
    /// Dynamically loads `winfsp-x64.dll` from system or standard installation path.
    #[allow(clippy::missing_transmute_annotations)]
    pub fn load() -> Result<Self, VfsError> {
        let candidates = [
            "winfsp-x64.dll",
            r"C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll",
            r"C:\Program Files\WinFsp\bin\winfsp-x64.dll",
        ];

        let mut module = std::ptr::null_mut();
        for path in candidates {
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: Calling standard LoadLibraryW with a null-terminated UTF-16 wide string.
            let h = unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
            if !h.is_null() {
                module = h;
                debug!(path, "Loaded WinFsp DLL");
                break;
            }
        }

        if module.is_null() {
            return Err(VfsError::InternalFault(
                "WinFsp runtime (winfsp-x64.dll) not found. Please install WinFsp 2025.".to_string(),
            ));
        }

        macro_rules! resolve {
            ($name:ident, $sym:expr, $ty:ty) => {{
                // SAFETY: GetProcAddress with valid module handle and static null-terminated C string.
                let proc = unsafe {
                    windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                        module,
                        concat!($sym, "\0").as_ptr(),
                    )
                };
                if proc.is_none() {
                    // SAFETY: Free library on failure.
                    unsafe { windows_sys::Win32::Foundation::FreeLibrary(module) };
                    return Err(VfsError::InternalFault(format!(
                        "Failed to resolve WinFsp symbol: {}",
                        $sym
                    )));
                }
                // SAFETY: Transmuting function pointer returned by GetProcAddress.
                unsafe { std::mem::transmute::<_, $ty>(proc) }
            }};
        }

        let fsp_file_system_preflight = resolve!(
            fsp_file_system_preflight,
            "FspFileSystemPreflight",
            unsafe extern "system" fn(*const u16, *const u16) -> i32
        );
        let fsp_file_system_create = resolve!(
            fsp_file_system_create,
            "FspFileSystemCreate",
            unsafe extern "system" fn(
                *const u16,
                *const FspFsctlVolumeParams,
                *const FspFileSystemInterface,
                *mut *mut FspFileSystem,
            ) -> i32
        );
        let fsp_file_system_set_mount_point = resolve!(
            fsp_file_system_set_mount_point,
            "FspFileSystemSetMountPoint",
            unsafe extern "system" fn(*mut FspFileSystem, *const u16) -> i32
        );
        let fsp_file_system_start_dispatcher = resolve!(
            fsp_file_system_start_dispatcher,
            "FspFileSystemStartDispatcher",
            unsafe extern "system" fn(*mut FspFileSystem, u32) -> i32
        );
        let fsp_file_system_stop_dispatcher = resolve!(
            fsp_file_system_stop_dispatcher,
            "FspFileSystemStopDispatcher",
            unsafe extern "system" fn(*mut FspFileSystem)
        );
        let fsp_file_system_delete = resolve!(
            fsp_file_system_delete,
            "FspFileSystemDelete",
            unsafe extern "system" fn(*mut FspFileSystem)
        );
        let fsp_file_system_add_dir_info = resolve!(
            fsp_file_system_add_dir_info,
            "FspFileSystemAddDirInfo",
            unsafe extern "system" fn(
                *mut FspFsctlDirInfo,
                *mut c_void,
                u32,
                *mut u32,
            ) -> u8
        );
        let fsp_posix_map_permissions_to_security_descriptor = resolve!(
            fsp_posix_map_permissions_to_security_descriptor,
            "FspPosixMapPermissionsToSecurityDescriptor",
            unsafe extern "system" fn(u32, u32, u32, *mut *mut c_void) -> i32
        );
        let fsp_delete_security_descriptor = resolve!(
            fsp_delete_security_descriptor,
            "FspDeleteSecurityDescriptor",
            unsafe extern "system" fn(*mut c_void, Option<unsafe extern "system" fn() -> i32>)
        );

        Ok(Self {
            module,
            fsp_file_system_preflight,
            fsp_file_system_create,
            fsp_file_system_set_mount_point,
            fsp_file_system_start_dispatcher,
            fsp_file_system_stop_dispatcher,
            fsp_file_system_delete,
            fsp_file_system_add_dir_info,
            fsp_posix_map_permissions_to_security_descriptor,
            fsp_delete_security_descriptor,
        })
    }
}

impl Drop for WinFspApi {
    fn drop(&mut self) {
        if !self.module.is_null() {
            // SAFETY: Releasing the loaded DLL module handle.
            unsafe {
                windows_sys::Win32::Foundation::FreeLibrary(self.module);
            }
        }
    }
}

/// Global mount context attached to `FileSystem->UserContext`.
struct MountContext {
    driver: Arc<UcloudFsDriver>,
    default_sd: *mut c_void,
    default_sd_len: usize,
    is_sddl: bool,
    fsp_api: Arc<WinFspApi>,
}

// SAFETY: MountContext holds thread-safe references to driver and WinFspApi.
unsafe impl Send for MountContext {}
unsafe impl Sync for MountContext {}

/// Context attached to an open file handle (`*FileContext`).
struct WinFspFileContext {
    fh: u64,
    path: std::sync::RwLock<String>,
    is_directory: bool,
    delete_on_cleanup: AtomicBool,
}

impl WinFspFileContext {
    fn new(fh: u64, path: String, is_directory: bool) -> Self {
        Self {
            fh,
            path: std::sync::RwLock::new(path),
            is_directory,
            delete_on_cleanup: AtomicBool::new(false),
        }
    }

    fn get_path(&self) -> String {
        self.path.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_path(&self, new_path: String) {
        let mut lock = self.path.write().unwrap_or_else(|e| e.into_inner());
        *lock = new_path;
    }
}

/// Converts UTF-16 null-terminated pointer into Rust String, normalizing default data stream suffixes like `::$DATA`.
unsafe fn pwstr_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let mut s = String::from_utf16(slice).ok()?;
    if s.ends_with("::$DATA") || s.ends_with("::$data") {
        s.truncate(s.len() - 7);
    }
    Some(s)
}

/// Converts Unix epoch seconds to Windows FILETIME (100ns intervals since Jan 1, 1601 UTC).
fn unix_to_filetime(unix_secs: u64) -> u64 {
    unix_secs.saturating_add(11_644_473_600).saturating_mul(10_000_000)
}

/// Maps internal VFS errors to standard Windows NTSTATUS codes.
fn vfs_error_to_ntstatus(err: &VfsError) -> i32 {
    match err {
        VfsError::InternalFault(msg) if msg.contains("Directory not empty") => {
            STATUS_DIRECTORY_NOT_EMPTY
        }
        VfsError::AlternateDataStreamIgnored(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        _ => err.to_ntstatus(),
    }
}

/// Populates `FSP_FSCTL_FILE_INFO` attributes and timestamps from `FileInfo`.
fn populate_fsp_file_info(fsp_info: *mut FspFsctlFileInfo, info: &FileInfo) {
    if fsp_info.is_null() {
        return;
    }
    // SAFETY: Writing file metadata fields to caller-supplied FspFsctlFileInfo pointer.
    unsafe {
        let attrs = if info.is_directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else if info.is_readonly {
            FILE_ATTRIBUTE_NORMAL | crate::vfs::driver::FILE_ATTRIBUTE_READONLY
        } else if info.file_attributes != 0 {
            info.file_attributes
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        (*fsp_info).file_attributes = attrs;
        (*fsp_info).reparse_tag = 0;
        (*fsp_info).allocation_size = info.allocation_size;
        (*fsp_info).file_size = info.size;
        (*fsp_info).creation_time = unix_to_filetime(info.created_time);
        (*fsp_info).last_access_time = unix_to_filetime(info.modified_time);
        (*fsp_info).last_write_time = unix_to_filetime(info.modified_time);
        (*fsp_info).change_time = unix_to_filetime(info.modified_time);
        (*fsp_info).index_number = info.ino;
        (*fsp_info).hard_links = 0;
        (*fsp_info).ea_size = 0;
    }
}

/// Populates `FspFsctlDirInfo` with file metadata and UTF-16 file name.
fn populate_fsp_dir_info(dir_info: *mut FspFsctlDirInfo, info: &FileInfo, name: &str) -> u16 {
    if dir_info.is_null() {
        return 0;
    }
    let name_u16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = name_u16.len() * 2;
    let dir_info_size = 104 + name_bytes;

    unsafe {
        // Zero out entire 104-byte header including union/padding to prevent ntdll heap corruption (0xc0000374)
        std::ptr::write_bytes(dir_info as *mut u8, 0, 104);

        (*dir_info).size = dir_info_size as u16;
        let attrs = if info.is_directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else if info.is_readonly {
            FILE_ATTRIBUTE_NORMAL | crate::vfs::driver::FILE_ATTRIBUTE_READONLY
        } else if info.file_attributes != 0 {
            info.file_attributes
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        (*dir_info).file_info.file_attributes = attrs;
        (*dir_info).file_info.reparse_tag = 0;
        (*dir_info).file_info.allocation_size = info.allocation_size;
        (*dir_info).file_info.file_size = info.size;
        (*dir_info).file_info.creation_time = unix_to_filetime(info.created_time);
        (*dir_info).file_info.last_access_time = unix_to_filetime(info.modified_time);
        (*dir_info).file_info.last_write_time = unix_to_filetime(info.modified_time);
        (*dir_info).file_info.change_time = unix_to_filetime(info.modified_time);
        (*dir_info).file_info.index_number = info.ino;
        (*dir_info).file_info.hard_links = 0;
        (*dir_info).file_info.ea_size = 0;

        let name_dst = (dir_info as *mut u8).add(104) as *mut u16;
        std::ptr::copy_nonoverlapping(name_u16.as_ptr(), name_dst, name_u16.len());
    }

    dir_info_size as u16
}

// ---------------------------------------------------------------------------
// WinFsp C-ABI Callbacks (Enclosed in std::panic::catch_unwind)
// ---------------------------------------------------------------------------

unsafe extern "system" fn get_volume_info_cb(
    fs: *mut FspFileSystem,
    volume_info: *mut FspFsctlVolumeInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || volume_info.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let info = match mount_ctx.driver.get_volume_info() {
            Ok(v) => v,
            Err(e) => return vfs_error_to_ntstatus(&e),
        };

        (*volume_info).total_size = info.total_bytes;
        (*volume_info).free_size = info.free_bytes;

        let label: Vec<u16> = info.volume_label.encode_utf16().collect();
        let copy_len = label.len().min(31);
        (*volume_info).volume_label_length = (copy_len * 2) as u16;
        std::ptr::copy_nonoverlapping(
            label.as_ptr(),
            std::ptr::addr_of_mut!((*volume_info).volume_label) as *mut u16,
            copy_len,
        );
        let terminator = (std::ptr::addr_of_mut!((*volume_info).volume_label) as *mut u16).add(copy_len);
        *terminator = 0;

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn set_volume_label_cb(
    fs: *mut FspFileSystem,
    _volume_label: *const u16,
    volume_info: *mut FspFsctlVolumeInfo,
) -> i32 {
    unsafe { get_volume_info_cb(fs, volume_info) }
}

unsafe extern "system" fn get_security_by_name_cb(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    p_file_attributes: *mut u32,
    security_descriptor: *mut c_void,
    p_security_descriptor_size: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let path = match pwstr_to_string(file_name) {
            Some(p) => p,
            None => return STATUS_OBJECT_NAME_NOT_FOUND,
        };

        if crate::vfs::sandbox::is_shell_noise(&path) {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }

        let info = match mount_ctx.driver.get_file_info(&path) {
            Ok(i) => i,
            Err(e) => return vfs_error_to_ntstatus(&e),
        };

        if !p_file_attributes.is_null() {
            *p_file_attributes = info.file_attributes;
        }

        if !p_security_descriptor_size.is_null() {
            let sd_len = mount_ctx.default_sd_len;
            if *p_security_descriptor_size < sd_len {
                *p_security_descriptor_size = sd_len;
                return STATUS_BUFFER_OVERFLOW;
            }
            *p_security_descriptor_size = sd_len;
            if !security_descriptor.is_null() {
                std::ptr::copy_nonoverlapping(
                    mount_ctx.default_sd as *const u8,
                    security_descriptor as *mut u8,
                    sd_len,
                );
            }
        }

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn create_cb(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    create_options: u32,
    _granted_access: u32,
    _file_attributes: u32,
    _security_descriptor: *mut c_void,
    allocation_size: u64,
    p_file_context: *mut *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || p_file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let path = match pwstr_to_string(file_name) {
            Some(p) => p,
            None => return STATUS_OBJECT_NAME_NOT_FOUND,
        };

        let is_dir = (create_options & FILE_DIRECTORY_FILE) != 0;
        let fh = if is_dir {
            match mount_ctx.driver.create_directory(&path) {
                Ok(fh) => fh,
                Err(e) => return vfs_error_to_ntstatus(&e),
            }
        } else {
            match mount_ctx.driver.create(&path) {
                Ok(fh) => {
                    if allocation_size > 0 {
                        let _ = mount_ctx.driver.set_file_size(fh, allocation_size);
                    }
                    fh
                }
                Err(e) => return vfs_error_to_ntstatus(&e),
            }
        };

        let ctx = Box::new(WinFspFileContext::new(fh, path.clone(), is_dir));
        *p_file_context = Box::into_raw(ctx) as *mut c_void;

        if !file_info.is_null() {
            if let Ok(info) = mount_ctx.driver.get_file_info_by_handle(fh) {
                populate_fsp_file_info(file_info, &info);
            } else if let Ok(info) = mount_ctx.driver.get_file_info(&path) {
                populate_fsp_file_info(file_info, &info);
            }
        }

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn open_cb(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    _create_options: u32,
    _granted_access: u32,
    p_file_context: *mut *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || p_file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let path = match pwstr_to_string(file_name) {
            Some(p) => p,
            None => return STATUS_OBJECT_NAME_NOT_FOUND,
        };

        if crate::vfs::sandbox::is_shell_noise(&path) {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }

        let info = match mount_ctx.driver.get_file_info(&path) {
            Ok(i) => i,
            Err(e) => return vfs_error_to_ntstatus(&e),
        };

        let fh = match mount_ctx.driver.open(&path) {
            Ok(fh) => fh,
            Err(e) => return vfs_error_to_ntstatus(&e),
        };

        let ctx = Box::new(WinFspFileContext::new(fh, path, info.is_directory));
        *p_file_context = Box::into_raw(ctx) as *mut c_void;

        if !file_info.is_null() {
            populate_fsp_file_info(file_info, &info);
        }

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn overwrite_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_attributes: u32,
    _replace_file_attributes: u8,
    allocation_size: u64,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        if let Err(e) = mount_ctx.driver.set_file_size(ctx.fh, 0) {
            return vfs_error_to_ntstatus(&e);
        }
        if allocation_size > 0 {
            let _ = mount_ctx.driver.set_file_size(ctx.fh, allocation_size);
        }

        if !file_info.is_null() {
            if let Ok(info) = mount_ctx.driver.get_file_info_by_handle(ctx.fh) {
                populate_fsp_file_info(file_info, &info);
            } else if let Ok(info) = mount_ctx.driver.get_file_info(&ctx.get_path()) {
                populate_fsp_file_info(file_info, &info);
            }
        }

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn cleanup_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
    flags: u32,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        const FSP_CLEANUP_DELETE: u32 = 0x01;
        if (flags & FSP_CLEANUP_DELETE) != 0 || ctx.delete_on_cleanup.load(Ordering::SeqCst) {
            let _ = mount_ctx.driver.delete(&ctx.get_path());
        } else {
            let _ = mount_ctx.driver.cleanup(ctx.fh);
        }
    }));
}

unsafe extern "system" fn close_cb(fs: *mut FspFileSystem, file_context: *mut c_void) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if file_context.is_null() {
            return;
        }
        let ctx = Box::from_raw(file_context as *mut WinFspFileContext);
        if !fs.is_null() && !(*fs).user_context.is_null() {
            let mount_ctx = &*((*fs).user_context as *const MountContext);
            let _ = mount_ctx.driver.close(ctx.fh);
        }
    }));
}

unsafe extern "system" fn read_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    p_bytes_transferred: *mut u32,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || buffer.is_null()
            || p_bytes_transferred.is_null()
        {
            return STATUS_UNSUCCESSFUL;
        }
        *p_bytes_transferred = 0;
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        let dest = std::slice::from_raw_parts_mut(buffer as *mut u8, length as usize);
        match mount_ctx.driver.read(ctx.fh, offset, dest) {
            Ok(0) => STATUS_END_OF_FILE,
            Ok(bytes) => {
                *p_bytes_transferred = bytes as u32;
                STATUS_SUCCESS
            }
            Err(e) => vfs_error_to_ntstatus(&e),
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn write_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    buffer: *const c_void,
    offset: u64,
    length: u32,
    _write_to_end_of_file: u8,
    _constrained_io: u8,
    p_bytes_transferred: *mut u32,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || buffer.is_null()
            || p_bytes_transferred.is_null()
        {
            return STATUS_UNSUCCESSFUL;
        }
        *p_bytes_transferred = 0;
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        let src = std::slice::from_raw_parts(buffer as *const u8, length as usize);
        match mount_ctx.driver.write(ctx.fh, offset, src) {
            Ok(bytes) => {
                *p_bytes_transferred = bytes as u32;
                if !file_info.is_null() {
                    if let Ok(info) = mount_ctx.driver.get_file_info_by_handle(ctx.fh) {
                        populate_fsp_file_info(file_info, &info);
                    } else if let Ok(info) = mount_ctx.driver.get_file_info(&ctx.get_path()) {
                        populate_fsp_file_info(file_info, &info);
                    }
                }
                STATUS_SUCCESS
            }
            Err(e) => vfs_error_to_ntstatus(&e),
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn flush_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        if !file_context.is_null()
            && !file_info.is_null()
        {
            let mount_ctx = &*((*fs).user_context as *const MountContext);
            let ctx = &*(file_context as *const WinFspFileContext);
            if let Ok(info) = mount_ctx.driver.get_file_info(&ctx.get_path()) {
                populate_fsp_file_info(file_info, &info);
            }
        }
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn get_file_info_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || file_info.is_null()
        {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        let res = mount_ctx
            .driver
            .get_file_info_by_handle(ctx.fh)
            .or_else(|_| mount_ctx.driver.get_file_info(&ctx.get_path()));
        match res {
            Ok(info) => {
                populate_fsp_file_info(file_info, &info);
                STATUS_SUCCESS
            }
            Err(e) => vfs_error_to_ntstatus(&e),
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn set_basic_info_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_attributes: u32,
    _creation_time: u64,
    _last_access_time: u64,
    _last_write_time: u64,
    _change_time: u64,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        if !file_info.is_null() {
            let res = mount_ctx
                .driver
                .get_file_info_by_handle(ctx.fh)
                .or_else(|_| mount_ctx.driver.get_file_info(&ctx.get_path()));
            if let Ok(mut info) = res {
                if file_attributes != 0 && file_attributes != u32::MAX {
                    info.file_attributes = file_attributes;
                }
                populate_fsp_file_info(file_info, &info);
            }
        }
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn set_file_size_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    new_size: u64,
    _set_allocation_size: u8,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        if let Err(e) = mount_ctx.driver.set_file_size(ctx.fh, new_size) {
            return vfs_error_to_ntstatus(&e);
        }

        if !file_info.is_null() {
            let res = mount_ctx
                .driver
                .get_file_info_by_handle(ctx.fh)
                .or_else(|_| mount_ctx.driver.get_file_info(&ctx.get_path()));
            if let Ok(mut info) = res {
                info.size = new_size;
                info.allocation_size = new_size;
                populate_fsp_file_info(file_info, &info);
            }
        }
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn can_delete_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);

        if ctx.is_directory
            && let Ok(entries) = mount_ctx.driver.read_directory(&ctx.get_path())
            && !entries.is_empty()
        {
            return STATUS_DIRECTORY_NOT_EMPTY;
        }
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn set_delete_cb(
    _fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
    delete_file: u8,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let ctx = &*(file_context as *const WinFspFileContext);
        ctx.delete_on_cleanup.store(delete_file != 0, Ordering::SeqCst);
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn rename_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_name: *const u16,
    new_file_name: *const u16,
    _replace_if_exists: u8,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &mut *(file_context as *mut WinFspFileContext);

        let old_path = match pwstr_to_string(file_name) {
            Some(p) => p,
            None => return STATUS_OBJECT_NAME_NOT_FOUND,
        };
        let new_path = match pwstr_to_string(new_file_name) {
            Some(p) => p,
            None => return STATUS_OBJECT_NAME_NOT_FOUND,
        };

        if let Err(e) = mount_ctx.driver.rename(&old_path, &new_path) {
            return vfs_error_to_ntstatus(&e);
        }

        ctx.set_path(new_path);
        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn get_security_cb(
    fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    security_descriptor: *mut c_void,
    p_security_descriptor_size: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null() || (*fs).user_context.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        let mount_ctx = &*((*fs).user_context as *const MountContext);

        if !p_security_descriptor_size.is_null() {
            let sd_len = mount_ctx.default_sd_len;
            if *p_security_descriptor_size < sd_len {
                *p_security_descriptor_size = sd_len;
                return STATUS_BUFFER_OVERFLOW;
            }
            *p_security_descriptor_size = sd_len;
            if !security_descriptor.is_null() {
                std::ptr::copy_nonoverlapping(
                    mount_ctx.default_sd as *const u8,
                    security_descriptor as *mut u8,
                    sd_len,
                );
            }
        }

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

unsafe extern "system" fn set_security_cb(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _security_information: u32,
    _modification_descriptor: *mut c_void,
) -> i32 {
    STATUS_SUCCESS
}

unsafe extern "system" fn read_directory_cb(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _pattern: *const u16,
    marker: *const u16,
    buffer: *mut c_void,
    length: u32,
    p_bytes_transferred: *mut u32,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || buffer.is_null()
            || p_bytes_transferred.is_null()
        {
            return STATUS_UNSUCCESSFUL;
        }
        *p_bytes_transferred = 0;
        let mount_ctx = &*((*fs).user_context as *const MountContext);
        let ctx = &*(file_context as *const WinFspFileContext);
        let dir_path = ctx.get_path();

        let children = match mount_ctx.driver.read_directory(&dir_path) {
            Ok(c) => c,
            Err(e) => return vfs_error_to_ntstatus(&e),
        };

        let curr_info = mount_ctx.driver.get_file_info(&dir_path).unwrap_or(FileInfo {
            ino: 1,
            size: 0,
            allocation_size: 0,
            is_directory: true,
            is_readonly: false,
            created_time: 0,
            modified_time: 0,
            file_attributes: FILE_ATTRIBUTE_DIRECTORY,
        });

        let is_root = dir_path == "/" || dir_path.is_empty() || dir_path == "\\";
        let mut all_entries = Vec::with_capacity(children.len() + 2);
        if !is_root {
            all_entries.push(DirEntry {
                ino: curr_info.ino,
                name: ".".to_string(),
                is_directory: true,
                size: 0,
            });
            all_entries.push(DirEntry {
                ino: 1,
                name: "..".to_string(),
                is_directory: true,
                size: 0,
            });
        }
        for child in children {
            all_entries.push(child);
        }

        let marker_str = pwstr_to_string(marker);
        let mut start_idx = 0;
        if let Some(m) = marker_str
            && !m.is_empty()
        {
            let m_lower = m.to_lowercase();
            if let Some(pos) = all_entries.iter().position(|e| e.name.to_lowercase() == m_lower) {
                start_idx = pos + 1;
            }
        }

        for entry in &all_entries[start_idx..] {
            let name_u16: Vec<u16> = entry.name.encode_utf16().collect();
            let name_bytes = name_u16.len() * 2;
            let dir_info_size = 104 + name_bytes;

            // Stack buffer for 8-byte aligned FspFsctlDirInfo, avoiding per-entry heap allocations
            let mut stack_buf = [0u64; 128]; // 1024 bytes
            let u64_words = dir_info_size.div_ceil(8);
            let (mut _heap_buf, dir_info) = if u64_words <= 128 {
                (None, stack_buf.as_mut_ptr() as *mut FspFsctlDirInfo)
            } else {
                let mut v = vec![0u64; u64_words];
                let ptr = v.as_mut_ptr() as *mut FspFsctlDirInfo;
                (Some(v), ptr)
            };

            let entry_info = FileInfo {
                ino: entry.ino,
                size: entry.size,
                allocation_size: entry.size,
                is_directory: entry.is_directory,
                is_readonly: false,
                created_time: curr_info.created_time,
                modified_time: curr_info.modified_time,
                file_attributes: if entry.is_directory {
                    FILE_ATTRIBUTE_DIRECTORY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                },
            };
            populate_fsp_dir_info(dir_info, &entry_info, &entry.name);

            let added = (mount_ctx.fsp_api.fsp_file_system_add_dir_info)(
                dir_info,
                buffer,
                length,
                p_bytes_transferred,
            );
            if added == 0 {
                return STATUS_SUCCESS;
            }
        }

        let _ = (mount_ctx.fsp_api.fsp_file_system_add_dir_info)(
            std::ptr::null_mut(),
            buffer,
            length,
            p_bytes_transferred,
        );

        STATUS_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => STATUS_UNSUCCESSFUL,
    }
}

static WINFSP_INTERFACE: FspFileSystemInterface = FspFileSystemInterface {
    get_volume_info: Some(get_volume_info_cb),
    set_volume_label: Some(set_volume_label_cb),
    get_security_by_name: Some(get_security_by_name_cb),
    create: Some(create_cb),
    open: Some(open_cb),
    overwrite: Some(overwrite_cb),
    cleanup: Some(cleanup_cb),
    close: Some(close_cb),
    read: Some(read_cb),
    write: Some(write_cb),
    flush: Some(flush_cb),
    get_file_info: Some(get_file_info_cb),
    set_basic_info: Some(set_basic_info_cb),
    set_file_size: Some(set_file_size_cb),
    can_delete: Some(can_delete_cb),
    rename: Some(rename_cb),
    get_security: Some(get_security_cb),
    set_security: Some(set_security_cb),
    read_directory: Some(read_directory_cb),
    resolve_reparse_points: None,
    get_reparse_point: None,
    set_reparse_point: None,
    delete_reparse_point: None,
    get_stream_info: None,
    get_dir_info_by_name: None,
    control: None,
    set_delete: Some(set_delete_cb),
    create_ex: None,
    overwrite_ex: None,
    get_ea: None,
    set_ea: None,
    obsolete0: None,
    dispatcher_stopped: None,
    reserved: [None; 31],
};

// ---------------------------------------------------------------------------
// WinFsp Mount Configuration & Lifecycle Guard
// ---------------------------------------------------------------------------

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
            mount_point: "Y:".to_string(),
            volume_label: "ucloud-fs".to_string(),
            case_sensitive_search: false,
            case_preserved_names: true,
            unicode_on_disk: true,
            reparse_points: false,
            operation_timeout_ms: 300_000,
        }
    }
}

/// WinFsp Mount Guard managing the live OS mount point and teardown lifecycle.
pub struct WinFspMountGuard {
    driver: Arc<UcloudFsDriver>,
    options: WinFspMountOptions,
    fsp_api: Arc<WinFspApi>,
    fs_ptr: *mut FspFileSystem,
    is_active: AtomicBool,
}

// SAFETY: WinFspMountGuard encapsulates the live FspFileSystem pointer safely across threads.
unsafe impl Send for WinFspMountGuard {}
unsafe impl Sync for WinFspMountGuard {}

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
            "Mounting ucloud-fs Native WinFsp Windows VFS Driver"
        );

        let fsp_api = Arc::new(WinFspApi::load()?);

        let wide_mount: Vec<u16> = options
            .mount_point
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let device_path: Vec<u16> = "WinFsp.Disk"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // 1. Preflight mount check
        // SAFETY: Calling FspFileSystemPreflight with valid null-terminated wide strings.
        let preflight_status = unsafe {
            (fsp_api.fsp_file_system_preflight)(device_path.as_ptr(), wide_mount.as_ptr())
        };
        if preflight_status != STATUS_SUCCESS {
            return Err(VfsError::InternalFault(format!(
                "FspFileSystemPreflight failed for mount point '{}' with NTSTATUS: 0x{:08X}",
                options.mount_point, preflight_status as u32
            )));
        }

        // 2. Generate default permissive Security Descriptor
        // SDDL "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)(A;OICI;FA;;;AU)(A;OICI;FA;;;AC)"
        // grants FILE_ALL_ACCESS (FA: 0x1F01FF) including DELETE to SYSTEM, Built-in Administrators, and Everyone (WD),
        // with complete Owner (O:BA) and Group (G:BA) SIDs so Windows kernel validates the descriptor.
        let mut default_sd: *mut c_void = std::ptr::null_mut();
        let mut sd_size: u32 = 0;
        let mut sddl_wide: Vec<u16> =
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)(A;OICI;FA;;;AU)(A;OICI;FA;;;AC)"
                .encode_utf16()
                .collect();
        sddl_wide.push(0);

        let conv_res = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
                &mut default_sd,
                &mut sd_size,
            )
        };

        let is_sddl = conv_res != 0 && !default_sd.is_null();
        if !is_sddl {
            // SAFETY: Fallback to mapping POSIX 0777 mode permissions to standard Windows security descriptor.
            let sd_status = unsafe {
                (fsp_api.fsp_posix_map_permissions_to_security_descriptor)(
                    0,
                    0,
                    0o777,
                    &mut default_sd,
                )
            };
            if sd_status != STATUS_SUCCESS || default_sd.is_null() {
                return Err(VfsError::InternalFault(format!(
                    "Failed to create default security descriptor: NTSTATUS: 0x{:08X}",
                    sd_status as u32
                )));
            }
        }

        let default_sd_len = unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorLength(default_sd) as usize
        };

        // 3. Configure Volume Parameters
        let mut volume_params = FspFsctlVolumeParams {
            version: 504,
            sector_size: 4096,
            sectors_per_allocation_unit: 1,
            max_component_length: 255,
            volume_creation_time: unix_to_filetime(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            ),
            volume_serial_number: 0x5543_4C44, // "UCLD"
            transact_timeout: 0,
            irp_timeout: options.operation_timeout_ms,
            irp_capacity: 1000,
            file_info_timeout: 5000,
            flags: (1 << 1)   // CasePreservedNames
                | (1 << 2)    // UnicodeOnDisk
                | (1 << 10)   // PostCleanupWhenModifiedOnly
                | (1 << 14)   // FlushAndPurgeOnCleanup
                | (1 << 16)   // UmFileContextIsUserContext2 (maps FileContext strictly to per-handle UserContext2 file descriptor)
                | (1 << 24)   // AllowOpenInKernelMode
                | (1 << 29),  // SupportsPosixUnlinkRename
            prefix: [0u16; 192],
            file_system_name: [0u16; 16],
            flags2: (1 << 0) | (1 << 1) | (1 << 2), // VolumeInfoTimeoutValid, DirInfoTimeoutValid, SecurityTimeoutValid
            volume_info_timeout: 5000,
            dir_info_timeout: 5000,
            security_timeout: 5000,
            stream_info_timeout: 0,
            ea_timeout: 0,
            fsext_control_code: 0,
            reserved32: [0u32; 1],
            reserved64: [0u64; 2],
        };

        let fsname_u16: Vec<u16> = "ucloudfs".encode_utf16().collect();
        for (i, &ch) in fsname_u16.iter().enumerate().take(15) {
            volume_params.file_system_name[i] = ch;
        }

        // 4. Create FileSystemHost using static interface callback table
        let mut fs_ptr: *mut FspFileSystem = std::ptr::null_mut();
        // SAFETY: Calling FspFileSystemCreate with initialized volume params and static interface callbacks.
        let create_status = unsafe {
            (fsp_api.fsp_file_system_create)(
                device_path.as_ptr(),
                &volume_params,
                &WINFSP_INTERFACE,
                &mut fs_ptr,
            )
        };
        if create_status != STATUS_SUCCESS || fs_ptr.is_null() {
            // SAFETY: Cleaning up allocated security descriptor on initialization failure.
            unsafe {
                if is_sddl {
                    windows_sys::Win32::Foundation::LocalFree(default_sd as _);
                } else {
                    (fsp_api.fsp_delete_security_descriptor)(default_sd, None);
                }
            }
            return Err(VfsError::InternalFault(format!(
                "FspFileSystemCreate failed with NTSTATUS: 0x{:08X}",
                create_status as u32
            )));
        }

        // 6. Set UserContext
        let mount_ctx = Box::new(MountContext {
            driver: Arc::clone(&driver),
            default_sd,
            default_sd_len,
            is_sddl,
            fsp_api: Arc::clone(&fsp_api),
        });
        // SAFETY: Attaching heap-allocated MountContext to FspFileSystem UserContext.
        unsafe {
            (*fs_ptr).user_context = Box::into_raw(mount_ctx) as *mut c_void;
        }

        // 7. Set Mount Point
        // SAFETY: Calling FspFileSystemSetMountPoint on valid FspFileSystem pointer.
        let mount_status =
            unsafe { (fsp_api.fsp_file_system_set_mount_point)(fs_ptr, wide_mount.as_ptr()) };
        if mount_status != STATUS_SUCCESS {
            // SAFETY: Clean up UserContext and FileSystemHost on mount error.
            unsafe {
                let user_ctx = (*fs_ptr).user_context;
                if !user_ctx.is_null() {
                    let mount_ctx = Box::from_raw(user_ctx as *mut MountContext);
                    if mount_ctx.is_sddl {
                        windows_sys::Win32::Foundation::LocalFree(mount_ctx.default_sd as _);
                    } else {
                        (fsp_api.fsp_delete_security_descriptor)(mount_ctx.default_sd, None);
                    }
                    (*fs_ptr).user_context = std::ptr::null_mut();
                }
                (fsp_api.fsp_file_system_delete)(fs_ptr);
            }
            return Err(VfsError::InternalFault(format!(
                "FspFileSystemSetMountPoint failed for '{}' with NTSTATUS: 0x{:08X}",
                options.mount_point, mount_status as u32
            )));
        }

        // 8. Start Dispatcher Threads
        // SAFETY: Starting WinFsp background kernel I/O worker threads (0 = auto core count).
        let start_status = unsafe { (fsp_api.fsp_file_system_start_dispatcher)(fs_ptr, 0) };
        if start_status != STATUS_SUCCESS {
            // SAFETY: Clean up on failure.
            unsafe {
                let user_ctx = (*fs_ptr).user_context;
                if !user_ctx.is_null() {
                    let mount_ctx = Box::from_raw(user_ctx as *mut MountContext);
                    if mount_ctx.is_sddl {
                        windows_sys::Win32::Foundation::LocalFree(mount_ctx.default_sd as _);
                    } else {
                        (fsp_api.fsp_delete_security_descriptor)(mount_ctx.default_sd, None);
                    }
                    (*fs_ptr).user_context = std::ptr::null_mut();
                }
                (fsp_api.fsp_file_system_delete)(fs_ptr);
            }
            return Err(VfsError::InternalFault(format!(
                "FspFileSystemStartDispatcher failed with NTSTATUS: 0x{:08X}",
                start_status as u32
            )));
        }

        info!(mount_point = %options.mount_point, "WinFsp Native Dispatcher started successfully");

        Ok(Self {
            driver,
            options,
            fsp_api,
            fs_ptr,
            is_active: AtomicBool::new(true),
        })
    }

    /// Queries volume statistics from the driver.
    pub fn get_volume_info(&self) -> Result<VolumeInfo, VfsError> {
        self.driver.get_volume_info()
    }

    /// Unmounts the drive cleanly, preventing zombie mount icons in Windows Explorer.
    pub fn unmount(&self) -> Result<(), VfsError> {
        if self.is_active.swap(false, Ordering::SeqCst) {
            info!(mount_point = %self.options.mount_point, "Unmounting WinFsp drive");
            if !self.fs_ptr.is_null() {
                // SAFETY: Stopping dispatcher, deleting filesystem host, and reclaiming UserContext.
                unsafe {
                    (self.fsp_api.fsp_file_system_stop_dispatcher)(self.fs_ptr);

                    let user_ctx = (*self.fs_ptr).user_context;
                    (*self.fs_ptr).user_context = std::ptr::null_mut();

                    (self.fsp_api.fsp_file_system_delete)(self.fs_ptr);

                    if !user_ctx.is_null() {
                        let mount_ctx = Box::from_raw(user_ctx as *mut MountContext);
                        if mount_ctx.is_sddl {
                            windows_sys::Win32::Foundation::LocalFree(mount_ctx.default_sd as _);
                        } else {
                            (self.fsp_api.fsp_delete_security_descriptor)(mount_ctx.default_sd, None);
                        }
                    }
                }
            }
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
