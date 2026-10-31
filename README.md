# ucloud-fs (v0.1.0)

**Pure-Rust Unified Cloud Virtual Filesystem Daemon** for Windows 11 / 10.

`ucloud-fs` mounts multiple Google Drive accounts into a unified, high-performance local Windows filesystem (`X:` or `Y:`) backed by [WinFsp](https://winfsp.dev/). It behaves like a native physical drive in Windows Explorer, Command Prompt, and PowerShell with zero persistent disk caching and instant progressive video/media streaming.

---

## 🌟 Key Features

- **Zero Persistent Disk Footprint**: Reads and writes stream directly to/from Google Drive. No massive temporary cache files filling your local SSD.
- **Sub-Second Progressive Streaming**: Built with a lock-free direct page cache (`SharedBlockCache`) using 512 KB blocks and paced background read-ahead. Media players (e.g. VLC, Windows Media Player) start playing instantly without freezing or buffering pauses.
- **Multi-Account Storage Pooling & Affinity**: Combines storage across multiple Google accounts. Files inherit parent folder account affinity, while files exceeding single-account capacity are transparently striped across accounts.
- **Collision Disambiguation**: When different accounts contain folders or files with the same name, `ucloud-fs` disambiguates them cleanly in Windows Explorer (e.g. `Projects (mohdzaidk035)` vs `Projects (ucloudfs)`).
- **3-Pass Topological Delta Sync**: Background engine monitors remote Google Drive changes (`changes.list`) every 10 seconds. Out-of-order hierarchies and remote deletions are reconciled topologically without parent loss, duplicate mirroring, or resurrection loops.
- **Zeroized Encrypted Vault**: Account OAuth credentials and refresh tokens are protected at rest using `Argon2id` key derivation and authenticated `ChaCha20-Poly1305` encryption, with memory zeroization on drop.

---

## 📋 Prerequisites

1. **Windows 10 / 11** (64-bit `x86_64`)
2. **WinFsp**: Install the latest [WinFsp release](https://github.com/winfsp/winfsp/releases) (v2.0 or higher) for kernel-mode filesystem redirection.

---

## 🚀 Quick Start Guide

### 1. Authenticate an Account

To use your personal Google Cloud Console OAuth credentials, set the environment variables in PowerShell:
```powershell
$env:UCLOUD_CLIENT_ID = "<YOUR_CLIENT_ID>"
$env:UCLOUD_CLIENT_SECRET = "<YOUR_CLIENT_SECRET>"
```
*(Or pass them directly via `--client-id` and `--client-secret`).*

Then initiate authentication:
```powershell
ucloud-fs.exe login
```
*(Your default web browser will open to Google's **Account Chooser** screen. You can select any of your authorized Google accounts or add a new account to link).*

To link additional accounts to the unified storage pool:
```powershell
ucloud-fs.exe add-account
```

### 2. Check Storage & Account Status
Verify all linked accounts and their real-time Google Drive storage quotas:
```powershell
ucloud-fs.exe status
```

### 3. Mount the Virtual Drive
Mount the unified filesystem onto a virtual drive letter (default `X:`):
```powershell
ucloud-fs.exe mount X:
```

Custom volume label options:
```powershell
ucloud-fs.exe mount X: --volume-label "CloudPool"
```

Open Windows Explorer to browse `X:` as a standard local drive!

---

## 🛠️ CLI Command Reference

| Command | Arguments | Description |
|---|---|---|
| `login` / `add-account` | `[--client-id <ID>] [--client-secret <SECRET>]` | Links a Google Drive account via browser PKCE loopback. |
| `set-credentials` | `[--client-id <ID>] [--client-secret <SECRET>] [--reset]` | Sets your personal Google OAuth Client ID & Secret as primary, or resets to default. |
| `status` | None | Displays linked accounts, individual quotas, combined storage, and vault status. |
| `mount` | `[DRIVE] [--volume-label <NAME>]` | Mounts the virtual drive (default: `X:`, volume label: `ucloud`). |
| `remove-account` | `<EMAIL>` | Unlinks a specific account and removes its tokens from the encrypted vault. |
| `clear-accounts` | None | Removes all linked accounts from the vault. |
| `prune-accounts` | `[KEEP_EMAIL]` | Removes all accounts except the specified email. |
| `test-live` | None | Runs real-world live integration tests against linked accounts. |
| `help` | None | Displays help message and CLI options. |

---

## 🔑 Using Your Personal Google OAuth Credentials

> [!IMPORTANT]
> **Why set up your own Google Cloud project?**
> By default, `ucloud-fs` includes a built-in project Client ID and Secret for immediate out-of-the-box use. However, because Google marks unverified third-party OAuth apps in **"Testing"** mode, Google blocks access for accounts not explicitly added to that specific project's test users list (showing error: `Error 403: access_denied - ucloud-fs has not completed the Google verification process`).
>
> Setting up your own free Google Cloud Console project takes less than 2 minutes and grants you **100% independence**, allowing you to link any of your personal or work accounts without restrictions.

### Step-by-Step Setup Guide

#### 1. Create a Google Cloud Project
1. Navigate to the [Google Cloud Console](https://console.cloud.google.com/).
2. Click the project dropdown in the top navigation bar and select **New Project**.
3. Name your project (e.g., `My-UcloudFS`) and click **Create**.
4. Make sure your newly created project is selected in the top bar.

#### 2. Enable the Google Drive API
1. In the left navigation menu (or search bar), go to **APIs & Services** > **Library**.
2. Search for `Google Drive API`.
3. Click **Google Drive API** in the results, then click the **Enable** button.

#### 3. Configure the OAuth Consent Screen
1. Go to **APIs & Services** > **OAuth consent screen**.
2. Select **External** under User Type (allows you to authorize your own accounts), then click **Create**.
3. Fill in the basic application information:
   - **App name**: `ucloud-fs` (or any name you choose)
   - **User support email**: Select your email address
   - **Developer contact email**: Enter your email address
4. Click **Save and Continue**.
5. **Scopes**:
   - Click **Add or Remove Scopes**.
   - Filter or search for `.../auth/drive` (`https://www.googleapis.com/auth/drive`).
   - Check the box next to `https://www.googleapis.com/auth/drive` (See, edit, create, and delete all of your Google Drive files).
   - Click **Update**, then click **Save and Continue**.
6. **Test Users**:
   - Click **+ ADD USERS**.
   - Add your primary Gmail address and any secondary Google accounts you plan to mount with `ucloud-fs`.
   - Click **Add**, then click **Save and Continue**.
7. Click **Back to Dashboard**.

#### 4. Create OAuth Client ID & Secret
1. Go to **APIs & Services** > **Credentials**.
2. Click **+ CREATE CREDENTIALS** at the top, and select **OAuth client ID**.
3. Under **Application type**, select **Desktop app**.
4. In the **Name** field, enter `ucloud-fs-client`.
5. Click **Create**.
6. A dialog will appear displaying:
   - **Client ID** (e.g. `your-client-id.apps.googleusercontent.com`)
   - **Client Secret** (e.g. `your-client-secret-here`)
7. Copy both values, or click **Download JSON**.

#### 5. Set Your Credentials as Primary in `ucloud-fs`

You can set your credentials as primary using any of the following methods:

- **Method A: CLI Configuration Command (Recommended)**:
  ```powershell
  ucloud-fs.exe set-credentials --client-id "<YOUR_CLIENT_ID>" --client-secret "<YOUR_CLIENT_SECRET>"
  ```
  *(Or run `ucloud-fs.exe set-credentials` without arguments to enter them interactively).*

- **Method B: Pass During Initial Login**:
  ```powershell
  ucloud-fs.exe login --client-id "<YOUR_CLIENT_ID>" --client-secret "<YOUR_CLIENT_SECRET>"
  ```
  `ucloud-fs` will link your account and automatically persist these credentials as primary for all future account additions (`add-account`) and background token refreshes.

- **Method C: Drop Downloaded JSON File**:
  Place your downloaded `credentials.json` or `client_secret.json` in the same directory as `ucloud-fs.exe`. The daemon will automatically detect and load your credentials.

- **Method D: Environment Variables**:
  ```powershell
  $env:UCLOUD_CLIENT_ID = "<YOUR_CLIENT_ID>"
  $env:UCLOUD_CLIENT_SECRET = "<YOUR_CLIENT_SECRET>"
  ```

#### Reverting to Built-in Defaults
If you ever want to clear your custom credentials and revert to the built-in defaults:
```powershell
ucloud-fs.exe set-credentials --reset
```

---

## 🏗️ Architecture Overview

```
                      +-----------------------------+
                      |   Windows Kernel Subsystem  |
                      |  (Explorer, Shell, VLC, etc)|
                      +--------------+--------------+
                                     |
                                     v
                      +-----------------------------+
                      |   WinFsp Driver (Native)    |
                      +--------------+--------------+
                                     |
                                     v
                      +-----------------------------+
                      |  src/vfs/driver_winfsp.rs   |
                      |   WinFsp C-ABI Callbacks    |
                      +--------------+--------------+
                                     |
                                     v
                      +-----------------------------+
                      |      src/vfs/driver.rs      |
                      | Lock-free SharedBlockCache  |
                      |   Paced Readahead Engine    |
                      +-------+--------------+------+
                              |              |
              +---------------+              +--------------+
              v                                             v
+---------------------------+                 +---------------------------+
|    src/routing/table.rs   |                 |   src/routing/sync.rs     |
| In-Memory Dual-Tree VFS   |                 | 3-Pass Topological Delta  |
| Account Disambiguation    |                 | Sync & Token Tracking     |
+-------------+-------------+                 +-------------+-------------+
              |                                             |
              +---------------+              +--------------+
                              v              v
                      +-----------------------------+
                      |   src/routing/placement.rs  |
                      |  Storage Tiering & Striping |
                      +--------------+--------------+
                                     |
                                     v
                      +-----------------------------+
                      |    src/network/client.rs    |
                      |  Google Drive v3 REST Client|
                      +-----------------------------+
```

---

## 🧪 Running Tests & Building From Source

Prerequisites for building:
- Rust toolchain (`x86_64-pc-windows-gnu`)
- WinFsp developer SDK

```powershell
# Run all unit and integration tests
cargo test --offline -j 1 -- --test-threads=1

# Compile optimized release binary
cargo build --release --offline -j 1
```

---

## 📄 License

Licensed under the MIT License. See [LICENSE](LICENSE) for details.
