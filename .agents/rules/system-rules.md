# SYSTEM PROMPT & MASTER ARCHITECTURE PLAN: UNIFIED CLOUD VFS ROUTER

## 🚨 STRICT SYSTEM DIRECTIVES & OPERATING RULES (NON-NEGOTIABLE)

1. **ZERO-ASSUMPTION & MANDATORY CLARIFICATION PROTOCOL:**
   **PLEASE MAKE SURE, IF THERE IS ANY AMBIGUITY (IN YOUR UNDERSTANDING), COME OUT AND SAY SO. DO NOT ASSUME ANYTHING ON YOUR OWN IF NOT TOLD. ASK EVERYTHING AND QUERY EVERY DECISION.**
   - Before writing code for any phase, inspect the requirements for that phase. If a file path, OS target, OAuth Client ID/Secret delivery mechanism, encryption key derivation parameter, chunk size, UI behavior, or error recovery edge case is not 100% explicitly specified by the user, **STOP IMMEDIATELY**.
   - List your specific questions clearly and wait for the user's explicit confirmation before generating code or modifying files.
   - Never silently choose defaults for security parameters, system paths, drive letters, or fallback behaviors.

2. **STRICT BUILD, STRESS-TEST, & PHASE-GATED EXECUTION:**
   - You must build this software strictly one Phase at a time (Phase 1 -> Phase 7).
   - **Build & Stress-Test Rule:** For every phase, you must **always build first**, then **rigorously stress-test** what you built across all desired operating scenarios, edge cases, concurrency loads, and failure modes. 
   - You are strictly forbidden from starting Phase $N+1$ until:
     1. The code for Phase $N$ builds successfully.
     2. All stress tests, unit tests, and integration tests for Phase $N$ execute and pass with `0 errors` and `0 warnings`.
     3. You show the test and stress-test execution output to the user and receive explicit approval to proceed.
   - All test and stress suites must reside inside `#[cfg(test)]` modules or the `/tests` directory so they are completely stripped out of the final production `--release` binary.

3. **CORE ARCHITECTURAL CONSTRAINTS:**
   - **Pure Resolver (DNS-Like Behavior):** The daemon acts strictly as a routing middleman between the local OS Virtual File System (VFS) and multiple linked Google Drive accounts.
   - **Zero Persistent Disk Cache:** Never store cached file payloads or persistent metadata databases on the user's hard drive. All routing tables, file trees, and stream buffers must live strictly in ephemeral RAM during an active session and be cryptographically zeroed out (`zeroize`) and purged on logout or session expiry.
   - **Minimal Resource Footprint:** Target <15 MB idle RAM and ~0% idle CPU. Use native Rust (`tokio`, `reqwest`, `dashmap`, `governor`, `rfd`, `winfsp`/`fuser`). Avoid heavy webview/Electron GUI frameworks.
   - **Zero Unhandled Panics:** Every production module must enforce `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`. All errors must propagate cleanly to OS-native error codes (`NTSTATUS` on Windows / `errno` on POSIX) so the backend daemon never crashes.

---

## 📋 STEP-BY-STEP EXECUTION BLUEPRINT

### PHASE 0: Pre-Flight Clarification & Environment Discovery
**Objective:** Establish exact environment parameters with the user before initializing the repository.
- **Action 0.1:** Inspect the host OS and toolchain (`rustc`, `cargo`, C build tools, `WinFsp` SDK on Windows or `libfuse3-dev` on Linux).
- **Action 0.2 (MANDATORY STOP & ASK):** Halt and ask the user to confirm:
  1. Target operating system(s) for the initial build (Windows `WinFsp`, Linux `FUSE`, or cross-platform).
  2. Desired isolated mount point (e.g., `Z:\` on Windows or `/mnt/unified_drive` on Linux).
  3. How the user wants to provide their Google Cloud OAuth 2.0 `client_id` and `client_secret` (e.g., environment variables, user-supplied config file, or CLI prompt).
  4. How the user wants to enter their login passphrase (CLI prompt, native OS password dialog, or local IPC socket).
  5. Where the encrypted OAuth token vault file is allowed to be stored on disk (since cached metadata/files cannot touch disk, confirm if a single encrypted `<4KB` credential vault file is permitted, or if OAuth authentication happens fresh per session).

---

### PHASE 1: Cryptographic Login System & 10-Hour Session Gatekeeper
**Objective:** Build the authentication manager, OAuth 2.0 loopback receiver, in-memory credential vault, and strict 10-hour session lifecycle controller.
- **Step 1.1 — OAuth 2.0 Loopback Flow:**
  - Implement a temporary localhost HTTP listener that captures the Google OAuth 2.0 authorization code when the user links $N$ Google accounts, exchanges it for `access_token` and `refresh_token`, and immediately shuts down the listener.
- **Step 1.2 — In-Memory Encrypted Token Vault:**
  - Store all account credentials in RAM using `ChaCha20-Poly1305` (with keys derived via `Argon2id` from the user's login passphrase).
  - Wrap all sensitive structs in `Zeroize` and `ZeroizeOnDrop` so raw token bytes are overwritten with `0x00` the instant they leave scope.
- **Step 1.3 — The 10-Hour Session Continuity Rule:**
  - Record an immutable monotonic `Instant` upon login.
  - Guarantee the user is **never** logged out before 10 hours (`36,000` seconds) have elapsed, automatically refreshing short-lived 1-hour Google `access_token`s silently in the background using the stored `refresh_token`s.
  - Spawn a background watchdog timer and an inline per-request atomic gatekeeper. The exact second the 10-hour timer expires (or when the user triggers manual logout), lock the drive (`EACCES` / `STATUS_ACCESS_DENIED`), zeroize all tokens in RAM, and purge 100% of cached routing metadata.
- **Step 1.4 — Phase 1 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 1.1 (Concurrency Hammer):* Spawn 1,000 concurrent async tasks reading tokens and triggering simulated background OAuth token refreshes simultaneously; verify zero deadlocks, race conditions, or memory leaks.
  - *Stress Test 1.2 (Tamper & Wrong-Credential Fuzzing):* Feed corrupted ciphertexts, bit-flipped nonces, expired refresh tokens, and malformed HTTP 401/403 OAuth responses into the login handler.
  - *Stress Test 1.3 (10-Hour Boundary Verification):* Simulate session clock at `9h 59m 59s` (verify session stays unlocked) and `10h 00m 01s` (verify immediate lock and 100% RAM cache purge).
- **GATE 1:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 2, and wait for approval.

---

### PHASE 2: Quota-Aware Rate Limiter & Google Drive HTTP/2 Client
**Objective:** Guarantee that no matter how aggressively the user or OS File Explorer hammers the mounted drive, Google Drive API rate limits are never exceeded.
- **Step 2.1 — Per-Account Token Bucket Rate Limiter:**
  - Implement a strict token-bucket rate limiter (`governor`) per linked Google account and globally per IP.
  - Account for exact Google Drive v3 API quota unit weights (`files.get` = 5 units, `files.create`/`update` = 50 units, `files.list` = 100 units, media download = 200 units), staying safely below Google's per-minute ceiling.
  - Enforce a per-account concurrency semaphore (e.g., max 32 simultaneous in-flight streams) so OS burst requests queue cleanly in RAM rather than triggering HTTP `429 Too Many Requests` or `403 User Rate Limit Exceeded`.
- **Step 2.2 — Adaptive Exponential Backoff & Retry Jitter:**
  - If Google ever returns HTTP `429` or `5xx`, automatically pause that specific account's bucket and retry with randomized exponential backoff without dropping the OS file operation.
- **Step 2.3 — Login-Time Quota & Metadata Hydration:**
  - Immediately upon user login, concurrently query `about.get` (`storageQuota`) and `files.list` across all linked Google accounts to fetch live storage capacity, used bytes, and file trees.
- **Step 2.4 — Phase 2 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 2.1 (Explorer Flood Simulation):* Spin up a local mock HTTP server (`wiremock`). Fire 5,000 simultaneous metadata and download requests at the client within 1 second. Verify the rate limiter queues and paces requests so the mock server never receives bursts exceeding the configured quota ceiling.
  - *Stress Test 2.2 (429 Recovery Stress Test):* Program the mock server to inject random HTTP `429` and `503` responses on 30% of calls; verify 100% of calls eventually succeed via backoff without crashing or leaking memory.
- **GATE 2:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 3, and wait for approval.

---

### PHASE 3: Ephemeral In-Memory DNS Routing Table (Zero Disk Persistence)
**Objective:** Build the lock-free, in-memory virtual namespace index that maps unified local file paths to their true `(account_id, google_file_id)` location in microseconds.
- **Step 3.1 — Lock-Free Inode Graph:**
  - Construct a purely in-memory routing table using `DashMap` (no SQLite files on disk, no temp files in `AppData` or `/tmp`).
  - Maintain three synchronized maps in RAM:
    1. `Inode ID -> VirtualInode Metadata & StorageTarget`
    2. `(Parent Inode, Filename) -> Inode ID` (for sub-microsecond path resolution)
    3. `(Account ID, Google File ID) -> Inode ID` (for instant reverse lookup during cloud sync)
- **Step 3.2 — Deduplication & Name Collision Resolution Policy:**
  - **STOP & ASK USER:** If `Account A` and `Account B` both already have a file named `/Documents/resume.pdf` when the accounts are merged, ask the user how collisions should be displayed in the unified view (e.g., appending `resume (Account A).pdf` or merging folders while renaming duplicate files).
- **Step 3.3 — Zero-Residue Teardown:**
  - Implement a deterministic teardown method that clears and shrinks all in-memory maps (`shrink_to_fit()`) when the session ends.
- **Step 3.4 — Phase 3 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 3.1 (100,000-File Scale & RAM Benchmark):* Populate the in-memory routing table with 100,000 synthetic file records across 5 accounts under heavy concurrent read/write threads; verify lookup latency is `<10 microseconds` and total heap overhead stays minimal.
  - *Stress Test 3.2 (Zero-Residue Verification):* Trigger session teardown and verify all map lengths and capacities drop to `0`.
- **GATE 3:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 4, and wait for approval.

---

### PHASE 4: Smart Placement Engine & Multi-Account Oversized File Chunker
**Objective:** Route incoming file uploads to the optimal Google account, and handle files larger than any single account's free space by asking user permission to split them across multiple accounts.
- **Step 4.1 — Single-Account Best-Fit Routing:**
  - When a file of size $S$ is written, inspect the live cached quotas of all linked accounts.
  - If one or more accounts have free space $\ge S$, route the entire file to the single account with the highest available free space using a Google Drive Resumable Upload session.
- **Step 4.2 — Oversized File Detection & Native User Permission Prompt:**
  - If $S$ is greater than the free space of **every** individual account, check if $S$ fits within the **combined free space pool** across all linked accounts.
  - If it fits in the combined pool, hold the OS write initialization and display a lightweight, native OS modal dialog (`rfd` / Win32 `MessageBox`) asking the user for permission to break the file into optimized chunks across multiple accounts.
  - Explicitly display the required warning in the prompt: *"Note: Because this file will be split across multiple Google accounts, you cannot open or access this file directly from the Google Drive web interface without downloading/coining it through this unified drive."*
- **Step 4.3 — Multi-Account Chunk Striping & Warning Note Generation:**
  - If the user clicks **No**, abort the write cleanly and return `ERROR_DISK_FULL` / `ENOSPC` to the OS.
  - If the user clicks **Yes**:
    1. Slice the incoming file stream into optimal chunks (e.g., `16 MB`, an exact multiple of Google Drive's mandatory `256 KB` resumable upload alignment unit) in a sliding RAM buffer.
    2. Stream each chunk to whichever account has available quota, storing a small JSON manifest so the VFS router knows how to reassemble the chunks seamlessly when read back.
    3. Generate a visible virtual companion text note (`<filename>.READ_WARNING.txt`) alongside the file in the folder reminding the user that the file is chunked across accounts and must be downloaded/copied out via the unified drive to be used.
- **Step 4.4 — Phase 4 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 4.1 (Boundary Routing Matrix):* Test uploads where: (a) file fits on Account B but not Account A, (b) file exceeds all individual accounts but fits in the combined pool with user approval, (c) user declines the chunking prompt, and (d) file exceeds the entire combined pool.
  - *Stress Test 4.2 (Byte-Exact Chunk Reassembly & Hash Verification):* Split a synthetic multi-part stream with a known SHA-256 hash across 3 mocked accounts under simulated packet jitter, read it back through the virtual reassembler, and verify the output SHA-256 matches 100%.
- **GATE 4:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 5, and wait for approval.

---

### PHASE 5: Real-Time Cloud-to-Local Delta Sync (`changes.list`)
**Objective:** Ensure that if a user deletes, renames, or adds a file directly inside the Google Drive web interface (`drive.google.com`), the local mounted folder reflects the change automatically.
- **Step 5.1 — Background Delta Poller:**
  - Run a lightweight background `tokio` task that polls Google Drive's `changes.list` API (using the stored `startPageToken` per account) at a safe, rate-limit-compliant interval.
- **Step 5.2 — Live Routing Table Mutation & Quota Recalculation:**
  - When `changes.list` reports `removed: true` or `trashed: true` for a `fileId`, immediately evict the corresponding inode from the in-memory routing table and update that account's cached storage quota.
- **Step 5.3 — OS Shell Notification (Instant Disappearance Without F5):**
  - Immediately invoke OS-native filesystem change notifications (`SHChangeNotify` on Windows / FUSE kernel invalidation on Linux) so the deleted file disappears from the user's open File Explorer window automatically.
- **Step 5.4 — Phase 5 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 5.1 (Remote Deletion Sync):* Inject a mocked `changes.list` deletion payload for an active file; verify the inode is purged from the routing table and the OS notification hook fires within milliseconds.
  - *Stress Test 5.2 (Delete-During-Read Race Condition):* Simulate a user deleting a file on `drive.google.com` at the exact millisecond the local OS is halfway through reading that file; verify the daemon returns a clean EOF/error code to the OS without crashing or dereferencing a missing inode.
- **GATE 5:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 6, and wait for approval.

---

### PHASE 6: Sandboxed OS VFS Mount & Seamless Drag-and-Drop Streaming
**Objective:** Connect the pure-resolver core to the OS file system driver (`WinFsp` / `fuser`) with strict isolation from primary OS drives (`C:\`, `/`) and zero-disk streaming.
- **Step 6.1 — Strict Drive Isolation Sandbox:**
  - Mount the unified drive strictly as an isolated virtual volume.
  - Block all path traversal sequences (`..`), cross-volume symbolic links, hard links, NTFS junction points, or device files so that even a catastrophic bug inside the VFS layer can never touch or corrupt files on the host's primary drives.
- **Step 6.2 — Zero-Disk Ephemeral Read/Write Sliding Windows:**
  - Implement an in-memory sliding ring buffer (e.g., `8 MB` per open file handle) to coalesce tiny `4 KB–64 KB` OS Explorer read/write syscalls into efficient HTTP/2 range streams and `256 KB`-aligned Google Resumable Upload chunks.
  - Immediately drop consumed byte slices as the read/write cursor advances, and zeroize the buffer on `release()` / `close()`.
- **Step 6.3 — Seamless Explorer Drag-and-Drop & Copy-Paste Support:**
  - Configure extended IRP/FUSE operation timeouts so multi-gigabyte uploads and user permission prompts never cause Windows Explorer to time out.
  - Handle Windows Explorer's pre-allocation (`set_file_size`), metadata queries (`getattr`, `statfs` reporting combined pool quota), and hidden shell probes (`desktop.ini`, `Thumbs.db`) cleanly in memory without wasting Google API calls.
- **Step 6.4 — Offline / Network Drop Resilience:**
  - Detect internet disconnections gracefully, pausing in-flight resumable streams where possible or returning clean OS network-unavailable error codes without unmounting or crashing the daemon.
- **Step 6.5 — Phase 6 Build & Stress-Testing Suite (`#[cfg(test)]`):**
  - *Build first*, then stress test across desired scenarios:
  - *Stress Test 6.1 (Sandbox Escape Fuzzing):* Use property-based testing (`proptest`) to throw 10,000 malicious paths (`..\..\Windows\System32`, UNC paths, null bytes, NTFS alternate data streams) at the VFS path resolver; verify 100% are blocked.
  - *Stress Test 6.2 (Concurrent Drag-and-Drop Stream Stress):* Simulate 20 concurrent file copy-in and copy-out streams of varying sizes (`1 byte` to `50 MB`) while simultaneously hammering `readdir` on the root directory; verify bounded RAM usage and zero data corruption.
- **GATE 6:** Confirm build success, review stress-test pass logs with the user, clarify any questions for Phase 7, and wait for approval.

---

### PHASE 7: Comprehensive Cybersecurity Audit & Final Release Verification
**Objective:** Execute rigorous static analysis, dependency vulnerability scans, and end-to-end security hardening checks to ensure zero memory vulnerabilities, injection vectors, or data leaks before release compilation.
- **Step 7.1 — Static Analysis & Lint Enforcement:**
  - Run `cargo clippy --all-targets -- -D warnings` and fix every single warning.
  - Enforce `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, unsafe_code)]` (where unsafe code is restricted strictly to OS driver FFI boundaries with mandatory safety documentation).
- **Step 7.2 — Cryptographic & Memory Leak Audit:**
  - Verify that `zeroize` is correctly implemented on all structs containing OAuth tokens, encryption keys, and active stream buffers.
  - Verify that no sensitive material is written to swap/paging files or disk logs.
- **Step 7.3 — Dependency Vulnerability Scan:**
  - Run `cargo audit` to verify zero known CVEs in third-party crates (`tokio`, `reqwest`, `winfsp`, etc.).
- **Step 7.4 — Final Release Build & Verification:**
  - Build the production binary using `cargo build --release`, ensuring zero test mocks (`wiremock`, `proptest`) or debug symbols are included.
  - Present the final verification report to the user detailing idle RAM usage, binary size, and instructions for starting the secure daemon.
- **GATE 7:** Confirm all cybersecurity checks pass successfully and receive final sign-off from the user.