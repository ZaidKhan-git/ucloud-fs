# Context

> **Protocol note:** This file is a live snapshot, not a log. Every entry reflects the *current* state of the project only. When updating, overwrite stale content — do not append a new dated section on top of the old one. If a fact changes, replace it; if a fact becomes irrelevant, delete it.

---

## 1. Project Overview

- **Project name:** `Driveme` (Unified Cloud VFS Router)
- **Purpose:** High-performance, lightweight, pure-resolver virtual filesystem daemon in Rust that unites multiple Google Drive accounts into a single seamless local drive with zero persistent disk cache (100 MB bounded disk staging allowed), quota-aware rate limiting, and ephemeral in-memory routing.
- **Current status:** Stage 1 Complete (Passed) — Gate Sign-off awaiting approval for Stage 2
- **Entry point(s):**
  ```
  src/main.rs
  src/lib.rs
  src/error.rs
  src/vfs/sandbox.rs
  ```

---

## 2. Recent Changes

- **Change:** Completed Stage 1 implementation (Error Taxonomy, Panic Shield, & Sandboxed Path Validator) and passed all stress and invariant tests.
  **Files:**
  ```
  Cargo.toml
  .cargo/config.toml
  src/lib.rs
  src/main.rs
  src/error.rs
  src/vfs/mod.rs
  src/vfs/sandbox.rs
  project-current-life.md
  context.md
  ```
  **Rationale:** Built foundational safety boundaries to guarantee that no subsequent module can panic across FFI, leak host paths via directory traversal, mishandle NTFS ADS streams, or crash the OS file manager driver.

---

## 3. Active Architecture

- **Stack:** Rust 2024 (`x86_64-pc-windows-gnu` via MinGW `gcc.exe`/`ld.exe`), WinFsp 2025, `thiserror`, `unicode-normalization`, `tracing`, `tracing-subscriber`, `proptest`
- **Core Governance & Anti-Hang Laws in Action:**
  - **Law B (FFI Panic Shield):** `src/vfs/sandbox.rs:ffi_boundary_guard` wraps all callbacks in `catch_unwind(AssertUnwindSafe)` and maps errors cleanly to NTSTATUS/errno without unwinding across C ABI.
  - **Law D (Zero Panic/Unwrap):** Strict `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented)]` active on `src/lib.rs` and `src/main.rs`.
  - **Path Sandbox & Shell Noise Filter:** `src/vfs/sandbox.rs:validate_path` enforces UTF-8 NFC form, blocks traversal (`..`), UNC paths (`\\`), null bytes, control characters, and ADS streams (`:Zone.Identifier`), with microsecond fast-reject for `desktop.ini`, `Thumbs.db`, etc.
  - **Error Taxonomy:** `src/error.rs:VfsError` mapped to `ntstatus` and `errno`.
- **Key modules:**
  ```
  src/lib.rs            - Crate root with strict clippy lints and module exports
  src/main.rs           - Daemon CLI entry point and tracing initialization
  src/error.rs          - Unified VfsError enum and lossless NTSTATUS / errno mappings
  src/vfs/mod.rs        - VFS subsystem module declaration
  src/vfs/sandbox.rs    - FFI boundary panic shield, path validator, and shell noise filter
  ```
- **Data flow / integration points:** OS Callbacks -> `ffi_boundary_guard` -> Path Sandbox / Shell Noise Filter -> Core VFS Router -> Google Drive API v3.

---

## 4. Pending Tasks / Notes

- [ ] Obtain user approval for Stage 1 test gate.
- [ ] Proceed to Stage 2: Cryptographic Vault, OAuth Loopback, & 10-Hour Session Engine (`src/security/vault.rs`, `src/security/session.rs`, `src/security/oauth.rs`).
- **Unresolved dependencies:** None. Toolchain and MinGW GNU target are compiling cleanly offline.
- **Known issues:** None. 10/10 tests passing.

---

## Maintenance Protocol (for the agent)

1. **When to update:** immediately after any code generation, script creation, refactor, or architectural decision — before ending the turn.
2. **How to update:**
   - Section 1 (Overview): edit only if the project's purpose or status has shifted.
   - Section 2 (Recent Changes): replace entries made obsolete by the new change; add the new entry with explicit file paths and rationale.
   - Section 3 (Architecture): edit only the parts affected by the change (e.g. a new module added, a pattern swapped).
   - Section 4 (Pending Tasks): remove completed items, add any new follow-ups the change introduced.
3. **Pruning rule:** if a piece of information is no longer accurate or no longer useful for understanding the current system, delete it. This file should never grow indefinitely — it should stay proportional to the current complexity of the project.
4. **Traceability rule:** every entry in Sections 2 and 3 that references code must include the concrete file path(s) in a fenced code block, not just a prose description.
