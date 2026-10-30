use std::sync::{Arc, Mutex};
use tracing::debug;

/// Trait for triggering OS file manager / shell directory view refreshes.
pub trait ShellNotifier: Send + Sync {
    fn notify_created(&self, path: &str);
    fn notify_deleted(&self, path: &str);
    fn notify_updated(&self, path: &str);
}

/// Windows Explorer shell notifier invoking `SHChangeNotify`.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct WindowsShellNotifier {
    mount_point: String,
}

#[cfg(windows)]
impl Default for WindowsShellNotifier {
    fn default() -> Self {
        Self {
            mount_point: "Y:".to_string(),
        }
    }
}

#[cfg(windows)]
impl WindowsShellNotifier {
    #[must_use]
    pub fn new(mount_point: &str) -> Self {
        Self {
            mount_point: mount_point.trim_end_matches(['\\', '/']).to_string(),
        }
    }

    fn to_dos_path(&self, virtual_path: &str) -> String {
        let clean = virtual_path.replace('/', "\\");
        let trimmed = clean.trim_start_matches('\\');
        if trimmed.is_empty() {
            format!("{}\\", self.mount_point)
        } else {
            format!("{}\\{}", self.mount_point, trimmed)
        }
    }

    fn parent_dos_path(&self, virtual_path: &str) -> String {
        let clean = virtual_path.replace('/', "\\");
        let trimmed = clean.trim_matches('\\');
        if let Some((parent, _)) = trimmed.rsplit_once('\\') {
            format!("{}\\{}", self.mount_point, parent)
        } else {
            format!("{}\\", self.mount_point)
        }
    }

    fn send_raw(&self, event_id: u32, dos_path: &str) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = std::ffi::OsStr::new(dos_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is a valid, null-terminated UTF-16 wide string encoding the virtual path.
        // `SHCNF_PATHW` informs the shell that `dwItem1` points to a null-terminated UTF-16 path.
        // `SHCNF_FLUSHNOWAIT` ensures the notification is posted asynchronously without blocking.
        // `dwItem2` is null as not required for CREATE, DELETE, or UPDATE events.
        unsafe {
            windows_sys::Win32::UI::Shell::SHChangeNotify(
                event_id as i32,
                windows_sys::Win32::UI::Shell::SHCNF_PATHW
                    | windows_sys::Win32::UI::Shell::SHCNF_FLUSHNOWAIT,
                wide.as_ptr().cast(),
                std::ptr::null(),
            );
        }
        debug!(event_id, dos_path, "Sent Windows SHChangeNotify");
    }
}

#[cfg(windows)]
impl ShellNotifier for WindowsShellNotifier {
    fn notify_created(&self, path: &str) {
        let dos = self.to_dos_path(path);
        let parent = self.parent_dos_path(path);
        self.send_raw(windows_sys::Win32::UI::Shell::SHCNE_CREATE, &dos);
        self.send_raw(windows_sys::Win32::UI::Shell::SHCNE_UPDATEDIR, &parent);
    }

    fn notify_deleted(&self, path: &str) {
        let dos = self.to_dos_path(path);
        let parent = self.parent_dos_path(path);
        self.send_raw(windows_sys::Win32::UI::Shell::SHCNE_DELETE, &dos);
        self.send_raw(windows_sys::Win32::UI::Shell::SHCNE_UPDATEDIR, &parent);
    }

    fn notify_updated(&self, path: &str) {
        let dos = self.to_dos_path(path);
        self.send_raw(windows_sys::Win32::UI::Shell::SHCNE_UPDATEDIR, &dos);
    }
}

/// Mock shell notifier recording all notifications in memory for verification.
#[derive(Debug, Clone, Default)]
pub struct MockShellNotifier {
    events: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockShellNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn get_events(&self) -> Vec<(String, String)> {
        self.events.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.events.lock() {
            g.clear();
        }
    }
}

impl ShellNotifier for MockShellNotifier {
    fn notify_created(&self, path: &str) {
        if let Ok(mut g) = self.events.lock() {
            g.push(("CREATE".to_string(), path.to_string()));
        }
    }

    fn notify_deleted(&self, path: &str) {
        if let Ok(mut g) = self.events.lock() {
            g.push(("DELETE".to_string(), path.to_string()));
        }
    }

    fn notify_updated(&self, path: &str) {
        if let Ok(mut g) = self.events.lock() {
            g.push(("UPDATE".to_string(), path.to_string()));
        }
    }
}
