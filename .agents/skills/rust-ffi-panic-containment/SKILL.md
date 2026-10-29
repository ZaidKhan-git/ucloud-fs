---
name: rust-ffi-panic-containment
description: Defensive panic boundaries for Rust code that is called by the OS or C code, such as FUSE (fuser, libfuse) and WinFsp virtual filesystem callbacks, C-ABI trampolines, plugin hooks, and driver bridges. Use this skill whenever the user writes or reviews getattr, read, write, readdir, lookup or similar filesystem callbacks, any `extern "C"` function, `catch_unwind`, OS error code mapping (errno, NTSTATUS), or asks why a mounted drive freezes, Explorer hangs, or a Rust daemon dies silently. Use it even if the user only says "my VFS crashes" or "unwrap in a callback".
---

# Rust FFI Panic Containment

## Why this matters

A virtual filesystem daemon is a server that the operating system calls into thousands of times per second. A panic that escapes one callback does not behave like a web-app exception. It either kills the thread that serves the mount, aborts the whole process, or (in raw C-ABI code on old compilers) is undefined behaviour. The user-visible result is the same: the mount disappears, File Explorer hangs, and applications get I/O errors. In-flight writes are lost.

The goal is simple: **a bug in one request must become one failed request, never a dead mount.**

## What actually happens (correct mental model)

Check which situation you are in before choosing a design.

1. **Raw `extern "C"` callback (WinFsp C API, libfuse bindings, custom trampolines).** Since Rust 1.81, a panic that tries to unwind out of an `extern "C"` function aborts the process. Before 1.81 it was undefined behaviour. Either way the daemon is gone. `extern "C-unwind"` permits unwinding, but the C side (FUSE, WinFsp) is not written to survive it, so do not use it for these callbacks.
2. **`fuser` in its pure-Rust mode.** Callbacks are ordinary trait methods called from Rust. A panic unwinds into the session loop thread and kills the mount loop. There is no C ABI crossing, but the outcome is still a dead mount.
3. **A binding crate such as `winfsp-rs`.** Read its source to learn whether it already contains panics. Do not assume. Contain panics yourself regardless, because a second guard is cheap.

A crash here does not normally cause a kernel panic or blue screen, since FUSE and WinFsp run your logic in user mode. It tears the filesystem down. Describe it that way to users so they do not over- or under-estimate the damage.

## Core rules

- Wrap **every** entry point the OS can reach, with no exceptions for "simple" callbacks. The one you skip is the one that panics.
- Do all real work in an inner function that returns `Result`. The boundary function only guards, maps the error, and replies.
- Map every failure (panic or ordinary error) to a specific OS code: `EIO`, `ENOENT`, `EACCES`, `ENOSPC` on Unix; `STATUS_UNEXPECTED_IO_ERROR`, `STATUS_OBJECT_NAME_NOT_FOUND`, `STATUS_ACCESS_DENIED` on Windows. Reserve the generic I/O code for "this is a bug".
- Always answer the request exactly once. Check how your binding treats a dropped reply object (some send `EIO` on drop, some leave the caller waiting).
- Never `unwrap`, `expect`, index with `[]`, or divide by an unchecked value inside a callback. Panic containment is the last line of defence, not the plan.

## The boundary pattern

```rust
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

pub static PANICS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum FsError { NotFound, Access, NoSpace, Io }

pub fn guard<T>(op: &'static str, f: impl FnOnce() -> Result<T, FsError>) -> Result<T, FsError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            PANICS.fetch_add(1, Ordering::Relaxed);
            let msg = payload
                .downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".into());
            tracing::error!(op, %msg, "panic contained at filesystem boundary");
            Err(FsError::Io)
        }
    }
}
```

Use it in a `fuser` callback (the exact signature varies by `fuser` version):

```rust
fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32,
        _flags: i32, _lock: Option<u64>, reply: ReplyData) {
    match guard("read", || self.core.read(ino, offset, size)) {
        Ok(bytes) => reply.data(&bytes),
        Err(e) => reply.error(e.errno()),
    }
}
```

For a raw C callback, the same idea returns a status code instead:

```rust
extern "C" fn cb_read(/* ... */) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| do_read(/* ... */))) {
        Ok(status) => status,
        Err(_) => STATUS_UNEXPECTED_IO_ERROR,
    }
}
```

## Details that bite

- **`panic = "abort"` disables all of this.** `catch_unwind` only works when the profile uses `panic = "unwind"` (the default). Check `Cargo.toml` for `[profile.*] panic = "abort"` and fail the build or a startup assertion if it is set.
- **`AssertUnwindSafe` is a promise.** You are telling the compiler that state touched by the closure is still valid after a panic. State that was half-updated is not. Keep callbacks as pure lookups plus one commit step, so a panic leaves the old consistent state.
- **Lock poisoning.** A panic while holding a `std::sync::Mutex` poisons it, and later `lock().unwrap()` panics again, turning one bug into a cascade. `parking_lot` locks do not poison, which hides the problem differently: the data may be half-written. Decide deliberately and document it.
- **Stack overflow and out-of-memory are not panics.** `catch_unwind` cannot save you from unbounded recursion on a deep directory tree. Bound recursion depth explicitly.
- **Panics in Tokio tasks** are caught by the runtime and surface as a `JoinError` on the join handle. Always inspect it. A background task that panics and is never joined fails silently and your delta sync stops without any log line.
- **Panic inside `Drop` during unwinding** aborts. Keep destructors infallible.
- **Panic hook for diagnostics.** Install `std::panic::set_hook` once at startup to capture `Backtrace::force_capture()` into a bounded in-memory ring buffer. Log the message, location and backtrace there, because the catch site only sees the payload.

## Circuit breaker (recommended)

Count contained panics in a sliding window. If more than N occur in M seconds, stop pretending everything is fine: switch the mount to read-only, or unmount cleanly and exit with a distinct code so a supervisor can restart. A filesystem that repeatedly panics is likely corrupting state.

## Verification checklist

1. Add a test-only fault injector (a feature flag or an env-gated hook) that panics inside each callback.
2. Assert that the call returns the expected error code and that the *next* request still succeeds.
3. Assert `PANICS` incremented and a log line with a backtrace was written.
4. Run a grep in CI for `unwrap()` and `expect(` inside callback modules and require an explicit allow comment.
5. Confirm the release profile has `panic = "unwind"`.

## Common mistakes to flag in review

- Guarding only the "big" callbacks and leaving `getattr` or `release` bare.
- Returning `Ok(empty)` after a panic, which tells the application the file is empty and risks silent data loss. Always return an error.
- Catching the panic and then continuing to use a poisoned or half-updated lock.
- Treating a contained panic as a success in metrics.
