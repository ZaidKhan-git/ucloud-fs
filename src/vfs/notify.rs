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
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsShellNotifier;

#[cfg(windows)]
impl WindowsShellNotifier {
    fn send_notification(event_id: u32, path: &str) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = std::ffi::OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            windows_sys::Win32::UI::Shell::SHChangeNotify(
                event_id as i32,
                windows_sys::Win32::UI::Shell::SHCNF_PATHW
                    | windows_sys::Win32::UI::Shell::SHCNF_FLUSHNOWAIT,
                wide.as_ptr().cast(),
                std::ptr::null(),
            );
        }
        debug!(event_id, path, "Sent Windows SHChangeNotify");
    }
}

#[cfg(windows)]
impl ShellNotifier for WindowsShellNotifier {
    fn notify_created(&self, path: &str) {
        Self::send_notification(windows_sys::Win32::UI::Shell::SHCNE_CREATE, path);
    }

    fn notify_deleted(&self, path: &str) {
        Self::send_notification(windows_sys::Win32::UI::Shell::SHCNE_DELETE, path);
    }

    fn notify_updated(&self, path: &str) {
        Self::send_notification(windows_sys::Win32::UI::Shell::SHCNE_UPDATEDIR, path);
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
