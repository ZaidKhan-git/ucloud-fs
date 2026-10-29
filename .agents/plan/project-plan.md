# MASTER IMPLEMENTATION SPECIFICATION: PURE-RUST UNIFIED CLOUD VFS ROUTER

## SECTION 1: NON-NEGOTIABLE SYSTEM DIRECTIVES & OPERATING RULES

1. **ZERO-ASSUMPTION & MANDATORY CLARIFICATION PROTOCOL:**
   **PLEASE MAKE SURE, IF THERE IS ANY AMBIGUITY (IN YOUR UNDERSTANDING), COME OUT AND SAY SO. DO NOT ASSUME ANYTHING ON YOUR OWN IF NOT TOLD. ASK EVERYTHING AND QUERY EVERY DECISION.**
   - Before writing any code for a Stage, inspect every requirement in that Stage. If any parameter, OS behavior, credential source, error code mapping, UI text, or edge-case behavior is not 100% explicitly confirmed by the user, **HALT IMMEDIATELY**.
   - Present a numbered list of precise questions to the user and wait for explicit answers before creating or editing any files.
   - Never guess defaults for drive letters, mount paths, encryption salt storage, OAuth client secrets, or conflict resolution rules.

2. **STRICT BUILD -> STRESS-TEST -> VERIFY LOOP (NO SKIPPING):**
   - You must execute this blueprint strictly one Stage at a time (`Stage 0` through `Stage 9`).
   - Within every Stage, you must follow this exact sequence:
     1. **Clarify:** Ask the user any remaining questions for the current Stage.
     2. **Implement:** Write the modular Rust implementation for the current Stageonly.
     3. **Build:** Run `cargo check` and `cargo build` until there are zero compiler errors and zero warnings.
     4. **Stress-Test:** Write and execute the mandatory unit, concurrency, fuzzing, and chaos tests inside `#[cfg(test)]` modules (so test code is never compiled into the final `--release` binary).
     5. **Gate Sign-Off:** Show the terminal test output to the user and wait for explicit permission before moving to the next Stage.

3. **DEBUGGING-LOOP PREVENTION & ANTI-HANG LAWS:**
   - **Law A — Never Block the OS Kernel Thread Without a Timeout:** Every asynchronous task invoked from a synchronous VFS callback (`WinFsp` or `FUSE`) via `tokio::runtime::Handle::block_on` MUST be wrapped in a `tokio::time::timeout`. If an AI agent writes an unbounded `.block_on()` and the network or a channel hangs, the OS File Explorer will freeze permanently and require a hard reboot.
   - **Law B — FFI Panic Containment (`catch_unwind`):** Every single VFS callback function exposed to `WinFsp` or `fuser` MUST wrap its entire body inside `std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ...))`. Allowing a Rust panic to unwind across the C-ABI boundary into `WinFsp`/`libfuse` causes immediate undefined behavior and crashes the OS shell.
   - **Law C — Strict Lock Acquisition Hierarchy (Anti-Deadlock):** Never acquire two `DashMap` shard locks (`get` / `get_mut`) on the same thread simultaneously. Always clone the tiny metadata struct or `InodeId` (`u64`) and drop the first `DashMap` reference guard *before* performing the next lookup or mutation. Never hold any lock across an `.await` point.
   - **Law D — Zero Production `unwrap()` / `expect()` / `panic!()`:** Enforce `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented)]` at the top of `src/lib.rs` and `src/main.rs`. Every fallible operation must return a typed `Result<T, VfsError>` mapped cleanly to an OS error code (`NTSTATUS` on Windows, `libc::c_int` errno on POSIX).

---

## SECTION 2: WORKSPACE & MODULE TOPOLOGY

Before writing business logic, structure the Cargo project with strict separation of concerns so the core routing logic can be 100% stress-tested in memory without mounting a real OS drive until Stage 8:

- `src/main.rs` — CLI entry point, signal handlers (`Ctrl+C` / `SIGTERM` graceful unmount), and runtime initialization.
- `src/lib.rs` — Crate-level lints (`deny(unwrap_used)`), global constants, and module exports.
- `src/error.rs` — Unified `VfsError` enum and lossless mappings to Windows `NTSTATUS` and POSIX `errno`.
- `src/security/`
  - `vault.rs` — `Argon2id` key derivation, `ChaCha20-Poly1305` AEAD encryption, `ZeroizeOnDrop` token wrappers, and OS memory locking (`VirtualLock` / `mlock`).
  - `session.rs` — The 10-hour monotonic session clock, atomic lock state, and deterministic zero-residue cache purge controller.
  - `oauth.rs` — Ephemeral `127.0.0.1` loopback receiver with PKCE (`S256`), CSRF `state` token validation, and background token refresh coordinator.
- `src/network/`
  - `limiter.rs` — Dual-layer rate limiter (`governor` token bucket per account + global IP limiter + concurrency `Semaphore`).
  - `client.rs` — Multiplexed HTTP/2 `reqwest` client, Google Drive v3 API wrappers, and exponential backoff with full jitter.
- `src/routing/`
  - `inode.rs` — Lock-free `DashMap` in-memory DNS routing table, virtual inode allocator, and path canonicalization/collision resolver.
  - `placement.rs` — Quota evaluator, single-account best-fit selector, and multi-account `16 MB` chunk calculator.
  - `sync.rs` — Background `changes.list` delta poller, tombstone eviction, and OS shell refresh notifier (`SHChangeNotify` / FUSE invalidator).
- `src/stream/`
  - `read_window.rs` — Per-handle (`fh`) `8 MB` sliding RAM ring buffer with instant byte eviction on cursor advance and MP4/MKV `moov`-atom tail-seek handling.
  - `write_spool.rs` — Strict `256 KB`-aligned resumable upload chunk accumulator and multi-account `.cmanifest` builder.
- `src/vfs/`
  - `sandbox.rs` — Path traversal blocker, symlink/junction blocker, and OS shell noise filter (`desktop.ini`, `Thumbs.db`, `autorun.inf`).
  - `prompt.rs` — Non-blocking native OS modal dialog dispatcher (`rfd`) on a dedicated OS thread with re-entrancy deduplication.
  - `driver_winfsp.rs` / `driver_fuse.rs` — Thin, panic-safe OS VFS trait adapters.

---

## SECTION 3: STEP-BY-STEP IMPLEMENTATION STAGES

### STAGE 0: Pre-Flight Discovery & User Interrogation
**Goal:** Lock down all ambiguous environment and UX parameters before creating any source files.
1. Inspect the environment (`cargo --version`, OS platform, installed SDKs).
2. **STOP AND ASK THE USER:**
   - **Q1 (OS Target):** Are we building the VFS mount layer for Windows (`WinFsp`), Linux (`FUSE`), or both?
   - **Q2 (Isolated Mount Point):** What exact drive letter (e.g., `Z:\`) or mount directory should be used, and what should happen if that drive letter is already in use?
   - **Q3 (Encrypted Credential Persistence vs. Pure Ephemeral Auth):** Since zero file or metadata caches are allowed on disk, should the encrypted OAuth `refresh_token` vault be stored in a single `<4 KB` encrypted file on disk (unlocked via password on startup), or should the user re-authenticate via browser on every 10-hour login session?
   - **Q4 (Duplicate Filename Collision Rule):** If `Google Account A` and `Google Account B` both contain a file with the exact same path (e.g., `/Notes/todo.txt`), how should the unified folder display them (e.g., `todo (accountA@gmail.com).txt`)?
   - **Q5 (Google Cloud OAuth Credentials):** How will you supply the Google Cloud Project `client_id` and `client_secret` (environment variables or local config)?
3. Wait for the user's answers before starting Stage 1.

---

### STAGE 1: Error Taxonomy, Panic Shield, & Sandboxed Path Validator
**Goal:** Build the foundational safety rails so no subsequent module can panic, leak host paths, or crash the OS driver.

#### 1.1 Implementation Details
- **Unified Error Mapping (`src/error.rs`):**
  - Define `VfsError` covering: `SessionLocked`, `NotFound`, `PermissionDenied`, `DiskFull`, `UserCancelledChunking`, `InvalidSeekOrOutOfOrderWrite`, `PathTraversalBlocked`, `RateLimitTimeout`, `NetworkOffline`, and `InternalFault`.
  - Implement deterministic conversion functions:
    - Windows (`NTSTATUS`): `SessionLocked` -> `STATUS_ACCESS_DENIED`, `NotFound` -> `STATUS_OBJECT_NAME_NOT_FOUND`, `DiskFull` / `UserCancelledChunking` -> `STATUS_DISK_FULL`, `InvalidSeekOrOutOfOrderWrite` -> `STATUS_NOT_SUPPORTED`, `NetworkOffline` -> `STATUS_DEVICE_OFF_LINE`.
    - POSIX (`errno`): `EACCES`, `ENOENT`, `ENOSPC`, `ENOTSUP`, `ENETDOWN`, `EIO`.
- **FFI Panic Shield Macro (`src/vfs/sandbox.rs`):**
  - Create a reusable wrapper function `ffi_boundary_guard<F, R>(op_name: &str, fallback_err: R, f: F) -> R` using `std::panic::catch_unwind`. If any inner code ever panics, capture the panic payload without printing sensitive filenames, return `fallback_err`, and keep the daemon alive.
- **Strict Path Sandbox & OS Shell Noise Filter (`src/vfs/sandbox.rs`):**
  - Normalize incoming paths to UTF-8 NFC form.
  - Reject any path containing: `..` components, drive colons (`:`), UNC prefixes (`\\`), null bytes (`\0`), NTFS Alternate Data Streams (`filename.txt:Zone.Identifier`), or control characters (`0x00..=0x1F`).
  - Create a fast-reject filter for OS Explorer background probing files (`desktop.ini`, `Thumbs.db`, `.DS_Store`, `.Trash`, `autorun.inf`, `folder.jpg`) so the VFS immediately returns `STATUS_OBJECT_NAME_NOT_FOUND` / `ENOENT` in `0.001 ms` without touching the routing table or network.

#### 1.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Allowing Windows NTFS Alternate Data Streams (`file.zip:Zone.Identifier`) to pass into Google Drive uploads when a user drags a downloaded file into `Z:\`.
  - *Prevention:* Explicitly detect `:` after the filename inside `sandbox.rs` and either silently discard the `:Zone.Identifier` stream write as a no-op success or return `STATUS_NOT_SUPPORTED` so the main file copy succeeds without creating garbage metadata files on Google Drive.
- **Mistake:** Case-sensitivity mismatches between Windows Explorer (case-insensitive) and Google Drive/Linux (case-preserving/sensitive).
  - *Prevention:* Store original case for display, but index lookup keys in `DashMap` using Unicode case-folded (`to_lowercase()`) keys on Windows so `Report.pdf` and `report.pdf` resolve consistently.

#### 1.3 Stage 1 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *Fuzz Test (`proptest`):* Generate 25,000 random strings containing traversal payloads (`..\..\Windows`), null bytes, ADS streams (`:Zone.Identifier`), and Unicode homoglyphs; verify 100% are sanitized or rejected in `<5 microseconds` with zero panics.
  2. *Panic Containment Test:* Deliberately trigger an explicit panic inside `ffi_boundary_guard` across 50 concurrent threads; verify 100% return `STATUS_UNEXPECTED_IO_ERROR` cleanly without crashing the test runner.
- **STOP:** Present test results to the user and await approval for Stage 2.

---

### STAGE 2: Cryptographic Vault, OAuth Loopback, & 10-Hour Session Engine
**Goal:** Build the authentication engine that keeps the user logged in for a strict 10-hour window, refreshes short-lived OAuth tokens transparently, locks RAM pages against OS swap files, and wipes 100% of memory upon expiration.

#### 2.1 Implementation Details
- **Memory-Locked Secret Containers (`src/security/vault.rs`):**
  - Wrap all OAuth `refresh_token`, `access_token`, and AEAD key structs in `#[derive(Zeroize, ZeroizeOnDrop)]`.
  - Use `argon2::Argon2::default()` (`Argon2id`, minimum 64 MB memory cost, 3 iterations) to derive a 256-bit key from the user's passphrase, and encrypt/decrypt credentials with `ChaCha20Poly1305`.
  - Lock sensitive key buffers in physical RAM using `VirtualLock` (Windows) or `libc::mlock` (Linux) so the OS kernel never pages plaintext OAuth tokens onto the hard drive's `pagefile.sys` or swap partition.
- **PKCE OAuth 2.0 Loopback Receiver (`src/security/oauth.rs`):**
  - When adding a Google account, bind an ephemeral TCP listener on `127.0.0.1:0` (OS-assigned random high port).
  - Generate a cryptographic PKCE `code_verifier`, `code_challenge` (`S256`), and random CSRF `state` token.
  - Validate that the incoming loopback HTTP request matches the exact `state` token, exchange the authorization code for tokens, and immediately drop the TCP listener.
- **The 10-Hour Session Controller (`src/security/session.rs`):**
  - Store session start time using `std::time::Instant` (monotonic clock immune to NTP or manual system clock tampering) alongside `AtomicBool` (`is_unlocked`).
  - **The 10-Hour Continuity Guarantee:** Google OAuth `access_token`s expire every 3,600 seconds (1 hour). The user's VFS session MUST remain continuously logged in for **36,000 seconds (10 hours)**.
  - Implement a **Single-Flight Token Refresh Lock (`tokio::sync::RwLock`)** per account: When an `access_token` is within 300 seconds of expiry, the first task acquires the write lock and refreshes the token via Google's token endpoint, while any other concurrent requests for that account wait on the lock and reuse the newly minted token (preventing 50 concurrent threads from firing 50 redundant token refresh calls).
  - **Hard 10-Hour Expiration & Cache Wipe:** Run a background watchdog every 1 second AND an inline nanosecond check (`verify_session_active()`) at the top of every VFS operation. The moment `Instant::elapsed() >= Duration::from_secs(36_000)` (or on manual logout):
    1. Flip `is_unlocked` to `false`.
    2. Abort all active background sync tasks.
    3. Clear and `shrink_to_fit()` every `DashMap` in the routing table.
    4. Drop and `zeroize` all active read/write stream buffers and OAuth tokens in RAM.

#### 2.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Using `SystemTime::now()` for the 10-hour timer, which breaks if the laptop syncs its clock via NTP or changes time zones.
  - *Prevention:* Strictly use `std::time::Instant` (and OS uninterrupted boot-time counters if tracking across OS sleep/hibernation).
- **Mistake:** Thundering-herd token refresh when 20 file reads notice an expired 1-hour `access_token` at the exact same millisecond.
  - *Prevention:* Inside the write-lock of the token refresh handler, double-check `if token.expires_at > now + 300s { return Ok(token.clone()); }` before making the HTTP POST request.

#### 2.3 Stage 2 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *Thundering-Herd Refresh Stress Test:* Expire an account's `access_token` against a `wiremock` server and fire 1,000 concurrent file-read tasks at the exact same microsecond. Verify the mock server receives **strictly 1** OAuth refresh HTTP POST call and all 1,000 tasks receive the new token without error.
  2. *10-Hour Boundary & Zero-Residue Test:* Test elapsed time at `35,999s` (verify 100% operational) and `36,001s` (verify immediate `VfsError::SessionLocked` and assert all credential/routing maps have length `0`).
  3. *Ciphertext Tamper Fuzzing:* Flip random bits in the encrypted vault payload and nonce; verify 100% return clean auth failure with zero panics.
- **STOP:** Present test results to the user and await approval for Stage 3.

---

### STAGE 3: Multi-Tier Quota Rate Limiter & Resilient Google Drive HTTP/2 Client
**Goal:** Guarantee that regardless of how violently the user or OS File Explorer hammers the mounted drive, Google Drive API rate limits are never exceeded and transient network drops never crash the daemon.

#### 3.1 Implementation Details
- **Weighted Quota-Unit Rate Limiter (`src/network/limiter.rs`):**
  - Implement a three-layer admission controller before any HTTP request leaves the machine:
    1. **Per-Account Weighted Token Bucket (`governor`):** Configured safely below Google Drive's per-user quota limit (e.g., max `4,500` quota units/second). Deduct exact method costs (`about.get` = 5, `files.get` = 5, `changes.list` = 10, `files.create`/`update` = 50, `files.list` = 100, media stream initiation = 200).
    2. **Global IP Rate Limiter (`governor`):** Caps combined requests across all linked accounts so linking 10 accounts on the same home Wi-Fi IP never triggers Google's global per-IP rate limit.
    3. **Per-Account Concurrency Semaphore (`tokio::sync::Semaphore`):** Strict cap of `24` concurrent in-flight HTTP streams per account.
- **Bounded Queue & Backpressure Timeout:**
  - Wrap `acquire_api_budget()` in a configurable timeout (e.g., `25 seconds`). If an app launches a massive denial-of-service storm of 50,000 requests, requests that cannot acquire budget within 25 seconds fail gracefully with `STATUS_DEVICE_BUSY` / `EBUSY` rather than accumulating infinite tasks in RAM.
- **Adaptive Exponential Backoff with Full Jitter (`src/network/client.rs`):**
  - If Google ever responds with `429 Too Many Requests`, `403 usageLimits / userRateLimitExceeded`, or `500/502/503/504`:
    - Parse the `Retry-After` header if present; otherwise apply full-jitter exponential backoff: `sleep = rand(0, min(max_backoff, base * 2^attempt))`.
    - Temporarily trip a per-account circuit breaker so other queued tasks for that account pause until the cooldown window elapses.
- **Login-Time Parallel Hydration:**
  - On login, concurrently fetch `about.get?fields=storageQuota,user` and `changes.getStartPageToken` across all linked accounts, followed by paginated `files.list` queries (`pageSize=1000`, requesting only essential fields: `nextPageToken,files(id,name,mimeType,size,modifiedTime,parents,trashed,appProperties)`) to populate the in-memory routing table.

#### 3.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Requesting full file payloads or omitting the `fields=` parameter in `files.list`, which causes Google Drive to return massive JSON payloads and throttle the connection.
  - *Prevention:* Hardcode minimal partial-response `fields` masks on every single Google Drive v3 API call.
- **Mistake:** Creating a new `reqwest::Client` per request, exhausting ephemeral OS TCP ports and performing redundant TLS handshakes (200ms penalty per call).
  - *Prevention:* Share a single `Arc<reqwest::Client>` configured with `http2_adaptive_window(true)`, `http2_keep_alive_interval(Duration::from_secs(20))`, and connection pooling across the entire daemon.

#### 3.3 Stage 3 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *5,000-Request Burst Flood Test:* Fire 5,000 concurrent simulated API calls across 3 accounts against a `wiremock` server that tracks request timestamps and active concurrency. Assert that active concurrency per account never exceeds `24` and per-second quota unit consumption never exceeds `4,500`.
  2. *Chaos Network & 429 Injection Test:* Configure `wiremock` to randomly return `429 Too Many Requests`, `503 Service Unavailable`, and sudden TCP resets on 40% of requests. Verify 100% of calls cleanly retry and succeed without panicking or leaking semaphore permits.
- **STOP:** Present test results to the user and await approval for Stage 4.

---

### STAGE 4: Lock-Free In-Memory DNS Routing Table (Zero Disk Persistence)
**Goal:** Build the sub-microsecond, purely in-memory virtual file system graph (`Virtual Path <-> [Account_ID, Google_File_ID]`) that leaves zero footprint on the local hard drive.

#### 4.1 Implementation Details
- **In-Memory Graph Structure (`src/routing/inode.rs`):**
  - Allocate monotonic 64-bit `InodeId`s starting at `1` (where `1` is always the virtual root directory `/`).
  - Maintain four lock-free `DashMap` indices in RAM:
    1. `inodes: DashMap<u64, VirtualInode>` — Full metadata, size, timestamps, children list, and `StorageLocation` enum (`SingleAccount`, `StripedManifest`, `VirtualWarningNote`, or `VirtualDirectory`).
    2. `lookup_by_name: DashMap<(u64, String), u64>` — Maps `(parent_inode, normalized_child_name)` -> `child_inode` in $O(1)$ time (`<1 microsecond`).
    3. `reverse_cloud_index: DashMap<(String, String), u64>` — Maps `(account_id, google_file_id)` -> `inode` for instant delta-sync updates.
    4. `virtual_folders: DashMap<u64, HashMap<String, String>>` — For virtual directories, maps `account_id -> google_folder_id` so the router knows which real folder ID corresponds to `/Projects/2026` on each linked Google account.
- **Lazy Folder Materialization (Anti-Clutter Rule):**
  - When a user has 5 linked Google accounts and creates a new folder `/Projects/Alpha`, **DO NOT** create empty folders across all 5 Google accounts!
  - Create the folder purely in the in-memory `inodes` table first, and only materialize the physical folder on a specific Google account when the Placement Engine actually routes a file upload inside `/Projects/Alpha` to that account.
- **Deterministic Collision Resolution:**
  - When merging existing file trees from multiple accounts during login hydration:
    - **Directories** with identical paths (`/Photos`) merge transparently into a single unified virtual directory whose children are the union of files across all accounts.
    - **Files** with identical paths (`/Photos/img01.jpg` on both Account A and Account B) are disambiguated deterministically in memory according to the user's Stage 0 rule (e.g., `img01 (account_b).jpg`) without renaming the actual file on Google's servers unless explicitly requested by the user.

#### 4.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Deadlocking `DashMap` by holding a reference guard (`let parent = inodes.get_mut(&1)`) while calling `inodes.insert(2, child)` on the same thread when keys `1` and `2` hash to the same internal `DashMap` shard!
  - *Prevention:* **Strict Single-Guard Rule:** Never hold more than one `DashMap` guard at a time in any function scope. Scope every `.get()` or `.get_mut()` inside a tight `{ ... }` block so the shard lock drops immediately.
- **Mistake:** Google Workspace native docs (`application/vnd.google-apps.document`, `.spreadsheet`) report `size: None` or `0` in `files.list`, causing Windows Explorer to treat them as 0-byte corrupted files.
  - *Prevention:* Detect `vnd.google-apps.*` MIME types during hydration and either expose them as lightweight `.url` / `.gdoc` shortcut files pointing to their `webViewLink` with exact byte length of the shortcut JSON, or map them cleanly as read-only virtual files.

#### 4.3 Stage 4 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *100,000-Inode Concurrent Stress Test:* Insert 100,000 synthetic files and nested directories across 5 accounts using 16 concurrent threads while simultaneously running `readdir`, `lookup`, and `rename` operations. Assert zero deadlocks, `<10 microsecond` average lookup latency, and `<35 MB` total heap allocation.
  2. *Shard-Collision Deadlock Test:* Force two inodes that hash to the exact same `DashMap` shard to undergo parent-child move (`rename`) operations 10,000 times; verify zero hangs.
- **STOP:** Present test results to the user and await approval for Stage 5.

---

### STAGE 5: Smart Placement Engine, Oversized File Chunker, & Non-Blocking Native Prompt
**Goal:** Route normal files to the single best Google account, and intercept files larger than any single account's free quota—prompting the user for permission to split the file into optimal chunks across multiple accounts and generating the required local warning note.

#### 5.1 Implementation Details
- **Quota Accounting & Pending-Write Reservations (`src/routing/placement.rs`):**
  - Track three numbers per account in RAM: `limit_bytes`, `used_bytes` (from Google), and `reserved_in_flight_bytes` (atomic counter of uploads currently in progress).
  - Effective free space is `limit_bytes - used_bytes - reserved_in_flight_bytes`. This prevents a critical race condition where the user drags two 8 GB files simultaneously into an account with 10 GB free, and both files think the account has 10 GB available!
- **Three-Tier Placement Resolution:**
  - When the OS initiates a file write of size $S$ (captured via `set_file_size` / allocation size on Windows `WinFsp` or write stream inspection):
    1. **Tier 1 (Fits Single Account):** If at least one account has `effective_free_bytes >= S + 16MB_safety_margin`, select the account with the largest free space, atomically increment its `reserved_in_flight_bytes` by $S$, and initiate a direct single-account Resumable Upload.
    2. **Tier 2 (Exceeds All Single Accounts, Fits Combined Pool):** If `max_single_free < S`, but `sum(all_effective_free_bytes) >= S + (num_accounts * 16MB_safety_margin)`:
       - Trigger the **Non-Blocking Native OS Permission Dialog (`src/vfs/prompt.rs`)**.
    3. **Tier 3 (Exceeds Combined Pool):** Immediately reject with `VfsError::DiskFull` (`STATUS_DISK_FULL` / `ENOSPC`).
- **Non-Blocking Native OS Modal Dialog (`src/vfs/prompt.rs`):**
  - Spawn the native dialog (`rfd::MessageDialog`) on a dedicated OS background thread linked via a `tokio::sync::oneshot` channel so it never blocks the Tokio async worker pool.
  - Implement a **Prompt Deduplication Cache** (`DashMap<String, bool>` with a 60-second TTL): If Windows Explorer retries the failed/paused `create`/`set_file_size` IRP multiple times for the same file, do not spam the user with 5 identical popups.
  - Display the exact warning text:
    > *"The file '<name>' (<size> GB) is larger than the free space on any single Google account (Max single account free: <max_single> GB), but fits in your combined storage pool (<total_pool> GB free). Do you grant permission to split it into 16 MB chunks across multiple accounts? IMPORTANT NOTE: You will NOT be able to open or access this file from the Google Drive web interface without downloading/copying it out through this unified drive."*
- **Optimal Multi-Account Chunk Striping (`16 MB` Chunks) & Companion Warning Note:**
  - Why `16 MB` (`16,777,216` bytes)? Google Drive's Resumable Upload API mandates chunks in multiples of `256 KB`. `16 MB` is exactly $64 \times 256\text{ KB}$, large enough to amortize HTTP TLS handshake overhead (`<1%` overhead) while small enough to fit inside a low-memory sliding RAM buffer and pack tightly into nearly-full Google accounts.
  - Each chunk is uploaded as an opaque binary file (`appProperties: { ucloud_chunk: "true", parent_manifest_id: "<uuid>", chunk_index: "N" }`) to its assigned account, computing an incremental `SHA-256` digest as bytes stream through RAM.
  - Once all chunks succeed, upload a small JSON manifest (`<filename>.ucloud_manifest.json`) containing the ordered list of `(chunk_index, account_id, google_file_id, byte_offset, byte_len, sha256)` to the account with the most remaining space.
  - Immediately insert two virtual inodes in the directory:
    1. The virtual unified file itself (`size = S`, `location = StorageLocation::StripedManifest { ... }`), which transparently stitches the chunks back together in order whenever the user copies/downloads it from the local drive.
    2. The required companion warning note (`<filename>.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt`, `location = StorageLocation::VirtualWarningNote`), containing a clear human-readable explanation of which accounts hold the chunks and reminding the user that the file must be copied/downloaded via the unified drive.
- **Transactional Rollback on Interrupted Striped Upload:**
  - If the user cancels the file copy in Windows Explorer at 60%, or the internet drops permanently mid-transfer, automatically iterate through all already-uploaded chunks for that incomplete manifest and issue background `files.delete` calls so orphaned chunks never clog the user's Google accounts!

#### 5.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** What if an app streams a file without declaring the total size upfront (`set_file_size` is not called first)?
  - *Prevention:* Start streaming to the single account with the highest free space (`Account_Max`). Monitor the cumulative bytes written. If the live stream approaches `Account_Max.effective_free_bytes - 16 MB` and the stream still hasn't finished (`EOF`), automatically pause the stream, pop the native permission dialog asking to convert the upload into a multi-account striped file, convert the first part on `Account_Max` into `Chunk 0..K`, and seamlessly continue streaming `Chunk K+1..M` onto the next account!

#### 5.3 Stage 5 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *Concurrent Quota Reservation Race Test:* Configure 2 accounts with `10 GB` free each. Simultaneously initiate two `8 GB` uploads at the exact same microsecond. Verify that Upload 1 claims `Account A` (`reserved_in_flight = 8 GB`), and Upload 2 sees only `2 GB` left on `Account A` and automatically routes to `Account B` instead of colliding on `Account A`!
  2. *Multi-Account Striping & Rollback Chaos Test:* Stream a synthetic `64 MB` file across 3 accounts limited to `25 MB` free each (forcing four `16 MB` chunks across the 3 accounts). Verify:
     - Path A (Success): All 4 chunks + manifest + `.CANNOT_ACCESS_WITHOUT_DOWNLOADING.txt` are created, and reading the file back produces the exact SHA-256 digest of the original `64 MB` buffer.
     - Path B (Mid-Flight Abort at Chunk 3): Inject a fatal error on Chunk 3; verify Chunks 0, 1, and 2 are automatically deleted from the mock Google servers and `reserved_in_flight_bytes` returns to `0`.
- **STOP:** Present test results to the user and await approval for Stage 6.

---

### STAGE 6: Real-Time Cloud-to-Local Delta Sync (`changes.list`) & OS Shell Refresh
**Goal:** Ensure that if a user deletes, trashes, renames, or uploads a file directly inside the `drive.google.com` web interface, the local mounted drive reflects the change automatically and updates Windows Explorer / POSIX directory views without requiring a manual refresh.

#### 6.1 Implementation Details
- **Background Delta Polling Loop (`src/routing/sync.rs`):**
  - Spawn a background task per linked account (active only while `session.is_unlocked() == true`) that polls `GET https://www.googleapis.com/drive/v3/changes?pageToken=<token>&includeItemsFromAllDrives=false&fields=newStartPageToken,nextPageToken,changes(fileId,removed,file(id,name,mimeType,size,trashed,parents,modifiedTime,appProperties))` every `15 seconds` (configurable), respecting the Stage 3 rate limiter.
- **State Reconciliation Rules:**
  - **Remote Deletion (`removed == true` or `file.trashed == true`):**
    1. Look up `(account_id, file_id)` in `reverse_cloud_index`.
    2. If it belongs to a `SingleAccount` file, remove the inode from `inodes` and `lookup_by_name`, detach it from its parent directory's `children` vector, and invoke `notify_os_shell_deleted(virtual_path)`.
    3. If it belongs to a chunk or manifest of a `StripedManifest` file, mark the parent striped file as broken/deleted, remove the virtual file and its companion warning note from the directory, and refresh the OS shell.
  - **Remote Addition or Rename:**
    1. Insert or update the `VirtualInode` in `inodes` and `lookup_by_name`, and invoke `notify_os_shell_updated(virtual_path)`.
  - **Live Quota Sync:**
    1. Whenever `changes.list` returns a non-empty list of changes for an account, fire a lightweight `about.get?fields=storageQuota` (cost: 5 units) to keep that account's `used_bytes` 100% accurate in RAM.
- **OS Shell Notification Bridge:**
  - On Windows: Call `windows::Win32::UI::Shell::SHChangeNotify` with `SHCNE_DELETE` / `SHCNE_CREATE` / `SHCNE_UPDATEDIR` and `SHCNF_PATHW | SHCNF_FLUSHNOWAIT`.
  - On Linux (`fuser`): Call `Notifier::inval_entry` and `Notifier::inval_inode`.

#### 6.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** "Self-Echo" flicker—when the user uploads or deletes a file locally in `Z:\`, Google Drive records that event in `changes.list`. Five seconds later, the delta poller sees the change and tries to re-apply or re-delete it while the local operation is still settling.
  - *Prevention:* Maintain a lightweight `recent_local_mutations: DashMap<(String, String), Instant>` with a 30-second TTL. If `changes.list` reports a change for a `(account_id, file_id)` that matches our expected local state, update the `pageToken` and ignore the redundant shell notification.
- **Mistake:** Deleting an inode from RAM while a local video player or file copy currently has an open file handle (`fh`) reading from that inode.
  - *Prevention:* Unlink the inode from `lookup_by_name` and parent `children` immediately (so it disappears from Explorer instantly), and set an `is_unlinked: AtomicBool` flag on the open file handle so the next `read()` call cleanly returns `STATUS_FILE_DELETED` / `ENOENT` instead of panicking on a missing map entry.

#### 6.3 Stage 6 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *Web-UI Deletion Sync Test:* Populate 50 files across 3 accounts, inject a mocked `changes.list` batch marking 10 files as `removed: true` and 5 files as `trashed: true`, and verify all 15 files vanish from `inodes` and `lookup_by_name` within `<5 ms`.
  2. *Delete-While-Reading Race Condition Test:* Start an active streaming read on a 20 MB file, and mid-stream (at byte offset 8 MB), fire a remote deletion event for that file. Verify the stream terminates cleanly with `VfsError::NotFound` and zero panics or dangling memory.
- **STOP:** Present test results to the user and await approval for Stage 7.

---

### STAGE 7: Zero-Disk Ephemeral Read/Write Streaming Engine
**Goal:** Deliver native-feeling file reads, video playback, and drag-and-drop file copies using bounded in-memory sliding windows (`8 MB` per open file handle) with **zero temp files on disk** and automatic byte eviction.

#### 7.1 Implementation Details
- **Per-Handle Ephemeral Read Ring Buffer (`src/stream/read_window.rs`):**
  - When a file is opened for reading (`open`), allocate an `EphemeralReadHandle` bound to the OS `fh` (`u64`), containing an `8 MB` maximum capacity sliding buffer (`VecDeque<u8>` or `Bytes` slices) protected by an async mutex, wrapped in `ZeroizeOnDrop`.
  - **Coalescing Tiny OS Reads:** When the OS asks for `read(offset = 0, len = 4096)`:
    1. Check if `[offset, offset + len)` resides inside the current `8 MB` RAM window.
    2. If **Yes (Cache Hit)**: Copy the `4096` bytes to the OS buffer in `<0.005 ms`, and **immediately drop/evict all bytes prior to `offset`** from the front of the sliding window so RAM is freed as the read cursor moves forward!
    3. If **No (Cache Miss)**: Issue a single HTTP/2 `GET` with `Range: bytes=<offset>-<min(offset + 8MB - 1, file_size - 1)>` to the resolved Google account (or across the corresponding `16 MB` chunks if reading a `StripedManifest` file). Stream the response into the `8 MB` RAM window, return the requested slice to the OS immediately as soon as the first `4096` bytes arrive (do not wait for all 8 MB to finish downloading before returning the first 4 KB to the OS!), and fill the remainder of the 8 MB window in the background.
- **Media Player Tail-Seek Optimization (`moov` Atom Handling):**
  - When opening an `.mp4`, `.mkv`, or `.zip` file, media players and archivers immediately read the first `64 KB` (`offset = 0`), then seek to the final `128 KB` (`offset = file_size - 131072`) to parse index headers, and then jump back to `offset = 65536`.
  - Reserve a tiny secondary `256 KB` "Tail Header Slot" in `EphemeralReadHandle` alongside the main `8 MB` forward sliding window. If a read request lands within the final `512 KB` of a file, populate the `256 KB` Tail Slot without blowing away the `8 MB` forward streaming window at the beginning of the file!
- **Per-Handle `256 KB`-Aligned Resumable Write Spooler (`src/stream/write_spool.rs`):**
  - When a file is opened for writing (`create`), allocate an `EphemeralWriteHandle` in RAM with a bounded `8 MB` staging accumulator (`ZeroizeOnDrop`).
  - As the OS sends arbitrary write chunks (`4 KB`, `64 KB`, etc.), append them into the RAM accumulator.
  - Whenever the accumulator reaches a multiple of `256 KB` (e.g., every `4 MB` or `8 MB`), flush that aligned slice via `PUT` (`Content-Range: bytes <start>-<end>/*`) to the Google Drive Resumable Session URI, and immediately clear those bytes from RAM.
  - When `flush()` / `cleanup()` / `release()` is called by the OS, send the final remaining bytes (`Content-Range: bytes <start>-<total-1>/<total>`), capture the returned Google `file_id`, register the new `VirtualInode` in the in-memory routing table, and zeroize the write handle.
- **Global RAM Backpressure Valve:**
  - Maintain a global `AtomicUsize` tracking total active stream buffer bytes across all open file handles, capped at a strict ceiling (e.g., `128 MB` total across the whole daemon). If a user opens 30 files simultaneously, new stream buffers wait on a global memory semaphore or shrink their window from `8 MB` to `2 MB` so the daemon never consumes excessive system RAM.

#### 7.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Waiting for the entire `8 MB` HTTP range request to finish downloading before returning a `4 KB` OS `read()` call, which adds 2 seconds of lag on slow connections.
  - *Prevention:* Use `reqwest::Response::bytes_stream()`. As soon as the stream yields enough bytes to satisfy the OS's immediate `4 KB` request, copy those `4 KB` to the caller and let a spawned background task finish draining the remaining `7.99 MB` into the handle's sliding window.
- **Mistake:** Out-of-order random writes inside an active Google Drive Resumable Upload stream (Google's Resumable API strictly requires sequential offsets).
  - *Prevention:* Track `expected_next_write_offset`. If the OS writes slightly out-of-order within the currently un-flushed `8 MB` RAM window, reorder it seamlessly in RAM! If an application attempts to seek backward to byte `0` after `50 MB` have already been flushed to Google's servers, return `STATUS_NOT_SUPPORTED` / `ENOTSUP` cleanly instead of corrupting the cloud file.

#### 7.3 Stage 7 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *MP4 Media Player Seek Simulation Test:* Simulate opening a `500 MB` virtual video file: read bytes `0..4096`, jump to `499_900_000..500_000_000`, jump back to `4096..65536`, and read sequentially in `16 KB` increments up to `20 MB`. Verify that **only 3 HTTP Range requests** are made to the mock server (not 1,250 requests!), and verify the handle's RAM usage never exceeds `8.25 MB`.
  2. *Unaligned Write Stream & Memory Ceiling Test:* Feed 10,000 random unaligned write chunks (`137 bytes`, `4096 bytes`, `65535 bytes`) into `EphemeralWriteHandle`. Verify every intermediate HTTP `PUT` sent to the mock Google server is an exact multiple of `262,144` bytes (`256 KB`), the final `PUT` matches the exact SHA-256 of the input stream, and RAM drops to `0` on `release()`.
- **STOP:** Present test results to the user and await approval for Stage 8.

---

### STAGE 8: Sandboxed OS VFS Driver Integration (`WinFsp` / `fuser`)
**Goal:** Wire the verified, stress-tested core into the OS VFS mount driver with strict timeout protection, seamless drag-and-drop copy/paste, and clean unmount handling.

#### 8.1 Implementation Details
- **Volume Configuration & Isolation (`src/vfs/driver_winfsp.rs` / `driver_fuse.rs`):**
  - Configure volume parameters:
    - Report `total_bytes = sum(all_accounts_limit)` and `free_bytes = sum(all_accounts_effective_free)` in `get_volume_info` / `statfs` so Windows Explorer's drive capacity bar displays the unified storage pool accurately and updates immediately after login.
    - Set `CaseSensitiveSearch = false`, `CasePreservedNames = true`, `UnicodeOnDisk = true`, and `ReparsePoints = false` (explicitly disabling symlinks/junctions at the filesystem capability level so the drive is 100% isolated from `C:\`).
    - Set `OperationTimeout = 300_000` (5 minutes) so large file operations or chunking permission dialogs never trigger premature OS driver resets.
- **Async-to-Sync Bridge with Mandatory Timeouts:**
  - Every VFS trait method (`open`, `create`, `read`, `write`, `read_directory`, `get_file_info`, `set_file_size`, `rename`, `cleanup`, `close`) must follow this exact 4-step wrapper pattern:
    1. Enter `ffi_boundary_guard` (`catch_unwind`).
    2. Call `core.verify_session_active()?` (if the 10-hour session is locked or expired, immediately return `STATUS_ACCESS_DENIED` / `EACCES` in `2 nanoseconds`).
    3. Validate path via `sandbox::validate_path(path)?`.
    4. Execute async work via `rt_handle.block_on(tokio::time::timeout(Duration::from_secs(120), async_op))`.
- **Windows Explorer Drag-and-Drop Lifecycle Perfection:**
  - When a user drags and drops a file into `Z:\` on Windows, Explorer executes this exact sequence:
    1. `get_file_info("/Z:/target.zip")` -> Return `STATUS_OBJECT_NAME_NOT_FOUND` (confirms file doesn't exist yet).
    2. `create("/Z:/target.zip")` -> Create a pending in-memory inode (`size = 0`) and return an `EphemeralWriteHandle`.
    3. `set_file_size("/Z:/target.zip", allocation_size)` -> **Intercept `allocation_size` here!** Run `PlacementEngine::resolve_upload_target` (triggering the native chunking prompt if oversized) and initialize the Google Resumable Upload session(s).
    4. Repeated `write(fh, offset, buffer)` -> Feed into `EphemeralWriteHandle`.
    5. `cleanup(fh)` / `close(fh)` -> Finalize the Resumable Upload stream, commit the `google_file_id` to the routing table, and zeroize the write buffer.
  - For **Directory Deletion** in `WinFsp`: In `can_delete` / `set_delete`, check that the directory's `children` list is empty (`STATUS_DIRECTORY_NOT_EMPTY`), and perform the actual Google Drive trash/delete call inside `cleanup` when `DeleteFile` flag is set.
- **Graceful Shutdown & Zombie Mount Prevention (`src/main.rs`):**
  - Register a `tokio::signal::ctrl_c()` and OS termination handler that cleanly unmounts `Z:\` / `/mnt/ucloud`, flushes or aborts in-flight resumable sessions, calls `lock_and_purge_all_caches()`, and exits with code `0` so the OS never leaves behind a dead "zombie" drive icon.

#### 8.2 Common Mistakes & Edge Cases to Prevent
- **Mistake:** Executing network calls inside `get_file_info` (`getattr`) or `read_directory` (`readdir`).
  - *Prevention:* `get_file_info` and `read_directory` must **100% of the time** read strictly from the in-memory `DashMap` routing table (`<0.01 ms` response time). Zero network calls are ever permitted inside `get_file_info` or `read_directory`.

#### 8.3 Stage 8 Build & Stress-Test Gate (`#[cfg(test)]`)
- **Build first**, then run:
  1. *Full Lifecycle VFS Integration Stress Test:* Against a local `wiremock` cluster simulating 3 Google Drive accounts:
     - Mount the VFS handler in test mode.
     - Execute 50 concurrent directory listings (`read_directory`), 10 single-account drag-and-drop file copies (`1 MB` to `20 MB`), 2 oversized multi-account striped file uploads (`40 MB` split across 3 accounts), 5 file renames, and 5 remote web-UI deletions.
     - Verify 100% byte-for-byte SHA-256 integrity on all downloaded/copied files, zero rate-limit violations, and idle memory returning to `<15 MB` once handles close.
- **STOP:** Present test results to the user and await approval for Stage 9.

---

### STAGE 9: Final Cybersecurity Audit & Production Hardening
**Goal:** Verify that all security controls, memory zeroization guarantees, and static analysis checks pass with zero flaws before building the final `--release` binary.

1. **Static & Unsafe Code Audit:**
   - Run `cargo clippy --all-targets -- -D warnings`.
   - Verify that `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` is active across all non-test targets.
   - Audit every `unsafe` block (restricted solely to OS `WinFsp` / `SHChangeNotify` / `VirtualLock` FFI calls) and document its memory safety invariants with a `// SAFETY:` comment.
2. **Secret & Log Sanitization Audit:**
   - Inspect every `tracing::info!`, `warn!`, `error!`, and `Debug` implementation across the codebase to confirm that `access_token`, `refresh_token`, encryption keys, and file contents can **never** be printed to stdout/stderr (`#[derive(Debug)]` must be omitted or manually redacted on all credential structs).
3. **Dependency & Supply-Chain Audit:**
   - Run `cargo audit` (or inspect `Cargo.lock`) to ensure zero known vulnerabilities in third-party dependencies.
4. **Release Compilation & Resource Verification:**
   - Configure `[profile.release]` in `Cargo.toml` with `lto = "fat"`, `codegen-units = 1`, `panic = "unwind"` (required so `catch_unwind` works across FFI boundaries—**never set `panic = "abort"` in a VFS driver that uses `catch_unwind`!**), and `strip = "symbols"`.
   - Run `cargo build --release` and report final binary size and idle RAM footprint to the user.

---

## 🛑 INITIAL INSTRUCTION TO THE AI AGENT
Acknowledge that you have read and bound yourself to all rules in **Section 1, Section 2, and Section 3**. **Do not write any code yet.** Begin **Stage 0** immediately by inspecting the workspace and asking me the pre-flight clarification questions.