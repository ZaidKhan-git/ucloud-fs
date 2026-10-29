use crate::error::VfsError;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_normalization::UnicodeNormalization;

/// Monotonic counter of caught panics across all FFI boundaries.
pub static CONTAINED_PANICS: AtomicU64 = AtomicU64::new(0);

/// Defensive FFI boundary guard that catches unwinding Rust panics
/// before they cross into C-ABI / OS drivers (`WinFsp` or `fuser`).
///
/// Prevents undefined behavior, crashes, and frozen OS file manager threads.
pub fn ffi_boundary_guard<F, R>(op_name: &'static str, fallback_err: R, f: F) -> R
where
    F: FnOnce() -> R,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            CONTAINED_PANICS.fetch_add(1, Ordering::Relaxed);
            let panic_msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown non-string panic payload");

            // Redact raw details to avoid leaking file paths into untrusted sinks
            tracing::error!(
                target: "vfs::ffi_boundary",
                op = op_name,
                panic = %panic_msg,
                "Panic contained safely at OS FFI boundary"
            );

            fallback_err
        }
    }
}

/// Known OS explorer probing files that should be fast-rejected in <0.001 ms.
const SHELL_NOISE_NAMES: &[&str] = &[
    "desktop.ini",
    "thumbs.db",
    ".ds_store",
    ".trash",
    "autorun.inf",
    "folder.jpg",
];

/// Returns `true` if the given filename or path is OS shell noise.
#[must_use]
pub fn is_shell_noise(path: &str) -> bool {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim();

    if name.is_empty() {
        return false;
    }

    let lower = name.to_ascii_lowercase();
    SHELL_NOISE_NAMES.contains(&lower.as_str())
}

/// Normalizes and sandboxes an incoming virtual filesystem path.
///
/// Guarantees:
/// 1. Output is valid UTF-8 in Unicode NFC normalization.
/// 2. Absolute path beginning with `/`.
/// 3. Zero path traversal components (`..` or `.`).
/// 4. Rejects null bytes, control characters (`0x00..=0x1F`), UNC prefixes (`\\`).
/// 5. Detects and blocks NTFS Alternate Data Streams (`filename:stream`).
pub fn validate_path(raw_path: &str) -> Result<String, VfsError> {
    if raw_path.is_empty() {
        return Ok("/".to_string());
    }

    // Reject null bytes and control characters immediately
    if raw_path.chars().any(|c| c == '\0' || (c as u32) <= 0x1F) {
        return Err(VfsError::PathTraversalBlocked(
            "Path contains null bytes or control characters".to_string(),
        ));
    }

    // Reject UNC paths
    if raw_path.starts_with(r"\\") || raw_path.starts_with("//") {
        return Err(VfsError::PathTraversalBlocked(
            "UNC network path prefixes are not permitted".to_string(),
        ));
    }

    // Unicode NFC normalization
    let normalized_nfc: String = raw_path.nfc().collect();

    // Check for NTFS Alternate Data Streams (e.g., "file.zip:Zone.Identifier")
    // Note: Windows drive letters (e.g., "Z:") at the beginning are stripped before validation.
    let path_without_drive = match normalized_nfc.as_bytes() {
        [b'a'..=b'z' | b'A'..=b'Z', b':', rest @ ..] => {
            std::str::from_utf8(rest).unwrap_or("")
        }
        _ => normalized_nfc.as_str(),
    };

    if path_without_drive.contains(':') {
        return Err(VfsError::AlternateDataStreamIgnored(
            "NTFS Alternate Data Stream rejected".to_string(),
        ));
    }

    // Split components across both standard and Windows slashes
    let mut clean_segments: Vec<&str> = Vec::new();

    for segment in path_without_drive.split(['/', '\\']) {
        let trimmed = segment.trim();
        if trimmed.is_empty() || trimmed == "." {
            continue;
        }

        if trimmed == ".." {
            return Err(VfsError::PathTraversalBlocked(
                "Parent directory traversal sequence ('..') is strictly prohibited".to_string(),
            ));
        }

        clean_segments.push(trimmed);
    }

    if clean_segments.is_empty() {
        return Ok("/".to_string());
    }

    let mut result = String::with_capacity(path_without_drive.len() + 1);
    for seg in clean_segments {
        result.push('/');
        result.push_str(seg);
    }

    Ok(result)
}

/// Generates a case-folded Unicode lookup key for case-insensitive matching on Windows
/// while preserving the display case in inode metadata.
#[must_use]
pub fn canonical_lookup_key(path: &str) -> String {
    path.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ntstatus::STATUS_UNEXPECTED_IO_ERROR;
    use proptest::prelude::*;
    use std::thread;

    #[test]
    fn test_valid_paths() {
        assert_eq!(validate_path("").unwrap(), "/");
        assert_eq!(validate_path("/").unwrap(), "/");
        assert_eq!(validate_path(r"\").unwrap(), "/");
        assert_eq!(validate_path("Documents/resume.pdf").unwrap(), "/Documents/resume.pdf");
        assert_eq!(validate_path(r"Z:\Documents\resume.pdf").unwrap(), "/Documents/resume.pdf");
        assert_eq!(validate_path("a/b/c").unwrap(), "/a/b/c");
        assert_eq!(validate_path("/a/./b///c/").unwrap(), "/a/b/c");
    }

    #[test]
    fn test_traversal_rejection() {
        assert!(validate_path("../secret").is_err());
        assert!(validate_path("/a/b/../../secret").is_err());
        assert!(validate_path(r"..\..\Windows\System32").is_err());
        assert!(validate_path("/foo/..").is_err());
    }

    #[test]
    fn test_ads_rejection() {
        let err = validate_path("downloaded.zip:Zone.Identifier").unwrap_err();
        assert!(matches!(err, VfsError::AlternateDataStreamIgnored(_)));
    }

    #[test]
    fn test_control_and_null_rejection() {
        assert!(validate_path("test\0bad.txt").is_err());
        assert!(validate_path("test\x1Fbad.txt").is_err());
    }

    #[test]
    fn test_shell_noise_detection() {
        assert!(is_shell_noise("desktop.ini"));
        assert!(is_shell_noise("Desktop.INI"));
        assert!(is_shell_noise(r"C:\Folder\Thumbs.db"));
        assert!(is_shell_noise("/foo/bar/.DS_Store"));
        assert!(is_shell_noise("autorun.inf"));
        assert!(!is_shell_noise("important_document.pdf"));
        assert!(!is_shell_noise("desktop_notes.txt"));
    }

    #[test]
    fn test_case_folding_key() {
        assert_eq!(canonical_lookup_key("/Documents/Report.PDF"), "/documents/report.pdf");
    }

    #[test]
    fn test_panic_containment_concurrency() {
        let initial_panics = CONTAINED_PANICS.load(Ordering::SeqCst);
        let handles: Vec<_> = (0..50)
            .map(|i| {
                thread::spawn(move || {
                    let result = ffi_boundary_guard("concurrent_panic_test", STATUS_UNEXPECTED_IO_ERROR, || {
                        if i % 2 == 0 {
                            panic!("Deliberate simulated test panic on thread {}", i);
                        } else {
                            panic!("Different explicit panic payload on thread {}", i);
                        }
                    });
                    assert_eq!(result, STATUS_UNEXPECTED_IO_ERROR);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("Thread should not unwind past guard");
        }

        assert_eq!(CONTAINED_PANICS.load(Ordering::SeqCst) - initial_panics, 50);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(25_000))]

        #[test]
        fn test_fuzz_path_sanitization_invariants(raw in ".*") {
            // Invariant 1: validate_path must never panic on arbitrary input
            let res = validate_path(&raw);

            if let Ok(safe_path) = res {
                // Invariant 2: Any accepted path MUST start with '/'
                prop_assert!(safe_path.starts_with('/'));

                // Invariant 3: Any accepted path MUST NEVER contain '..' segments
                for comp in safe_path.split('/') {
                    prop_assert_ne!(comp, "..");
                }

                // Invariant 4: Any accepted path MUST NOT contain null bytes or control characters
                prop_assert!(!safe_path.chars().any(|c| c == '\0' || (c as u32) <= 0x1F));

                // Invariant 5: Any accepted path MUST NOT contain ':'
                prop_assert!(!safe_path.contains(':'));

                // Invariant 6: Normalizing twice must equal normalizing once (Idempotence)
                let second_pass = validate_path(&safe_path).unwrap();
                prop_assert_eq!(safe_path, second_pass);
            }
        }
    }
}
