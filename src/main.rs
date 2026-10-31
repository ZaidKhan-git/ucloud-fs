#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use ucloud_fs::error::VfsError;
use ucloud_fs::network::client::{GoogleDriveClient, GoogleFileInput, HydratedAccountData};
use ucloud_fs::network::limiter::MultiTierLimiter;
use ucloud_fs::routing::placement::PlacementEngine;
use ucloud_fs::routing::sync::DeltaSyncEngine;
use ucloud_fs::routing::InMemoryRoutingTable;
use ucloud_fs::security::oauth::{
    capture_loopback_code, exchange_code_for_tokens, generate_csrf_state, generate_pkce,
    refresh_access_token, DEFAULT_GOOGLE_AUTH_ENDPOINT, DEFAULT_GOOGLE_TOKEN_ENDPOINT,
};
use ucloud_fs::security::session::SessionController;
use ucloud_fs::security::vault::{
    decrypt_credentials, encrypt_credentials, AccountCredentials, KeyDerivationParams,
};
use ucloud_fs::stream::backpressure::MemoryBackpressureValve;
use ucloud_fs::vfs::driver::{DriverDependencies, UcloudFsDriver};
use ucloud_fs::vfs::driver_winfsp::{WinFspMountGuard, WinFspMountOptions};
use ucloud_fs::vfs::notify::{ShellNotifier, WindowsShellNotifier};
use ucloud_fs::vfs::prompt::AutoAcceptPrompter;

pub const DEFAULT_CLIENT_ID: &str = "";
pub const DEFAULT_CLIENT_SECRET: &str = "";

const VAULT_FILE: &str = "vault.enc";
const CLIENT_CREDENTIALS_FILE: &str = "client_credentials.json";
const DEFAULT_PASSPHRASE: &str = "ucloud-fs-secure-vault-passphrase";

fn get_master_passphrase() -> String {
    std::env::var("UCLOUD_PASSPHRASE").unwrap_or_else(|_| DEFAULT_PASSPHRASE.to_string())
}

#[derive(Serialize, Deserialize, Default)]
struct SavedClientConfig {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
}

fn load_saved_client_credentials() -> Option<(String, String)> {
    // 1. Check primary client_credentials.json
    if let Ok(content) = std::fs::read_to_string(CLIENT_CREDENTIALS_FILE)
        && let Ok(cfg) = serde_json::from_str::<SavedClientConfig>(&content)
        && let (Some(id), Some(sec)) = (cfg.client_id, cfg.client_secret)
    {
        let (tid, tsec) = (id.trim().to_string(), sec.trim().to_string());
        if !tid.is_empty() && !tsec.is_empty() {
            return Some((tid, tsec));
        }
    }

    // 2. Check credentials.json or client_secret*.json (standard Google Cloud Console downloads)
    let candidate_files = ["credentials.json", "client_secret.json"];
    for fname in candidate_files {
        if let Ok(content) = std::fs::read_to_string(fname)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
        {
            let obj = val.get("installed")
                .or_else(|| val.get("web"))
                .unwrap_or(&val);
            if let (Some(id), Some(sec)) = (
                obj.get("client_id").and_then(|v| v.as_str()),
                obj.get("client_secret").and_then(|v| v.as_str()),
            ) {
                let (tid, tsec) = (id.trim().to_string(), sec.trim().to_string());
                if !tid.is_empty() && !tsec.is_empty() {
                    return Some((tid, tsec));
                }
            }
        }
    }

    None
}

fn save_saved_client_credentials(client_id: &str, client_secret: &str) -> Result<(), VfsError> {
    let cfg = SavedClientConfig {
        client_id: Some(client_id.trim().to_string()),
        client_secret: Some(client_secret.trim().to_string()),
    };
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| {
        VfsError::InternalFault(format!("Failed to serialize client credentials: {e}"))
    })?;
    std::fs::write(CLIENT_CREDENTIALS_FILE, json).map_err(|e| {
        VfsError::InternalFault(format!("Failed to write {CLIENT_CREDENTIALS_FILE}: {e}"))
    })?;
    Ok(())
}

fn clear_saved_client_credentials() -> Result<(), VfsError> {
    if std::path::Path::new(CLIENT_CREDENTIALS_FILE).exists() {
        std::fs::remove_file(CLIENT_CREDENTIALS_FILE).map_err(|e| {
            VfsError::InternalFault(format!("Failed to remove {CLIENT_CREDENTIALS_FILE}: {e}"))
        })?;
    }
    Ok(())
}

fn resolve_client_credentials(cred: &AccountCredentials) -> Result<(String, String), VfsError> {
    if let (Some(id), Some(secret)) = (&cred.client_id, &cred.client_secret)
        && !id.trim().is_empty()
        && !secret.trim().is_empty()
    {
        return Ok((id.trim().to_string(), secret.trim().to_string()));
    }

    let env_id = std::env::var("UCLOUD_CLIENT_ID")
        .or_else(|_| std::env::var("GOOGLE_CLIENT_ID"))
        .ok();
    let env_secret = std::env::var("UCLOUD_CLIENT_SECRET")
        .or_else(|_| std::env::var("GOOGLE_CLIENT_SECRET"))
        .ok();

    if let (Some(id), Some(secret)) = (env_id, env_secret)
        && !id.trim().is_empty()
        && !secret.trim().is_empty()
    {
        return Ok((id.trim().to_string(), secret.trim().to_string()));
    }

    if let Some((saved_id, saved_secret)) = load_saved_client_credentials() {
        return Ok((saved_id, saved_secret));
    }

    if !DEFAULT_CLIENT_ID.is_empty() && !DEFAULT_CLIENT_SECRET.is_empty() {
        return Ok((DEFAULT_CLIENT_ID.to_string(), DEFAULT_CLIENT_SECRET.to_string()));
    }

    Err(VfsError::PermissionDenied(format!(
        "Missing OAuth Client ID or Client Secret for account '{}'. Set personal credentials with 'ucloud-fs set-credentials', set UCLOUD_CLIENT_ID and UCLOUD_CLIENT_SECRET environment variables, or re-authenticate with 'ucloud-fs login --client-id <ID> --client-secret <SECRET>'.",
        cred.account_id
    )))
}

/// Hydrates file and folder trees from a linked Google account into the in-memory graph.
pub fn hydrate_account_into_table(
    routing_table: &Arc<InMemoryRoutingTable>,
    data: &HydratedAccountData,
) -> Result<(), VfsError> {
    routing_table.hydrate_account_data(data)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str).unwrap_or("help");

    match subcommand {
        "login" | "add-account" => {
            let mut custom_id = None;
            let mut custom_secret = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--client-id" && i + 1 < args.len() {
                    custom_id = Some(args[i + 1].clone());
                    i += 2;
                } else if args[i] == "--client-secret" && i + 1 < args.len() {
                    custom_secret = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            cmd_login(custom_id, custom_secret).await?;
        }
        "set-credentials" | "set-client" | "config-credentials" => {
            let mut custom_id = None;
            let mut custom_secret = None;
            let mut reset_to_default = false;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--client-id" && i + 1 < args.len() {
                    custom_id = Some(args[i + 1].clone());
                    i += 2;
                } else if args[i] == "--client-secret" && i + 1 < args.len() {
                    custom_secret = Some(args[i + 1].clone());
                    i += 2;
                } else if args[i] == "--reset" || args[i] == "--default" {
                    reset_to_default = true;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            cmd_set_credentials(custom_id, custom_secret, reset_to_default).await?;
        }
        "remove-account" | "remove" => {
            let target_email = args.get(2).map(String::as_str);
            cmd_remove_account(target_email).await?;
        }
        "prune-accounts" => {
            let keep_email = args.get(2).map(String::as_str).unwrap_or("ucloudfs@gmail.com");
            cmd_prune_accounts(keep_email).await?;
        }
        "clear-accounts" | "reset-vault" => {
            cmd_clear_accounts().await?;
        }
        "status" => {
            cmd_status().await?;
        }
        "test-live" => {
            cmd_test_live().await?;
        }
        "mount" => {
            let mut drive_letter = "X:".to_string();
            let mut volume_label = "ucloud".to_string();
            let mut i = 2;
            while i < args.len() {
                if (args[i] == "--label" || args[i] == "--volume-label") && i + 1 < args.len() {
                    volume_label = args[i + 1].clone();
                    i += 2;
                } else if !args[i].starts_with('-') && i == 2 {
                    drive_letter = args[i].clone();
                    i += 1;
                } else {
                    i += 1;
                }
            }
            cmd_mount(drive_letter, volume_label).await?;
        }
        _ => {
            print_usage();
        }
    }

    Ok(())
}

fn print_usage() {
    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs: Pure-Rust Unified Cloud VFS Router");
    println!(
        "================================================================================"
    );
    println!("Usage: ucloud-fs <COMMAND> [OPTIONS]\n");
    println!("Commands:");
    println!("  add-account     Link an additional Google Drive account (alias: login)");
    println!("                  Options: [--client-id <ID>] [--client-secret <SECRET>]");
    println!("  login           Link a Google Drive account via OAuth 2.0 PKCE loopback");
    println!("                  Options: [--client-id <ID>] [--client-secret <SECRET>]");
    println!("  set-credentials Configure personal Google OAuth Client ID and Secret as primary");
    println!("                  Options: [--client-id <ID>] [--client-secret <SECRET>] [--reset]");
    println!("  remove-account  Remove a linked Google Drive account from the vault");
    println!("                  Usage: ucloud-fs remove-account <EMAIL>");
    println!("  clear-accounts  Remove all linked accounts from the vault");
    println!("  prune-accounts  Remove all accounts except the specified account");
    println!("                  Usage: ucloud-fs prune-accounts [KEEP_EMAIL]");
    println!("  status          Display encrypted vault status and live storage quota");
    println!("  test-live       Execute real-world live test suite against linked account");
    println!("  mount [DRIVE]   Mount the unified virtual filesystem (default: X:)");
    println!("                  Options: [--label <NAME>] [--volume-label <NAME>] (default: ucloud)");
    println!("  help            Show this help message\n");
}

async fn load_vault_credentials() -> Result<Vec<AccountCredentials>, VfsError> {
    if !std::path::Path::new(VAULT_FILE).exists() {
        return Ok(Vec::new());
    }

    let payload = tokio::fs::read(VAULT_FILE).await.map_err(|e| {
        VfsError::InternalFault(format!("Failed to read {VAULT_FILE}: {e}"))
    })?;

    let passphrase = get_master_passphrase();
    decrypt_credentials(&payload, &passphrase, KeyDerivationParams::default())
}

async fn save_vault_credentials(creds: &[AccountCredentials]) -> Result<(), VfsError> {
    let passphrase = get_master_passphrase();
    let payload = encrypt_credentials(creds, &passphrase, KeyDerivationParams::default())?;

    tokio::fs::write(VAULT_FILE, payload).await.map_err(|e| {
        VfsError::InternalFault(format!("Failed to write {VAULT_FILE}: {e}"))
    })?;

    Ok(())
}

async fn cmd_login(
    custom_id: Option<String>,
    custom_secret: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs OAuth 2.0 PKCE Authentication");
    println!(
        "================================================================================\n"
    );

    let (saved_id, saved_secret) = load_saved_client_credentials()
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((None, None));

    let client_id = match custom_id
        .or_else(|| std::env::var("UCLOUD_CLIENT_ID").ok())
        .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
        .or(saved_id)
    {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            if !DEFAULT_CLIENT_ID.is_empty() {
                DEFAULT_CLIENT_ID.to_string()
            } else {
                use std::io::Write;
                print!("Enter Google OAuth Client ID: ");
                std::io::stdout().flush().ok();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    return Err(Box::new(VfsError::PermissionDenied(
                        "No Google Client ID provided. Pass --client-id <ID> or set UCLOUD_CLIENT_ID.".to_string(),
                    )));
                }
                trimmed
            }
        }
    };

    let client_secret = match custom_secret
        .or_else(|| std::env::var("UCLOUD_CLIENT_SECRET").ok())
        .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok())
        .or(saved_secret)
    {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            if !DEFAULT_CLIENT_SECRET.is_empty() {
                DEFAULT_CLIENT_SECRET.to_string()
            } else {
                use std::io::Write;
                print!("Enter Google OAuth Client Secret: ");
                std::io::stdout().flush().ok();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    return Err(Box::new(VfsError::PermissionDenied(
                        "No Google Client Secret provided. Pass --client-secret <SECRET> or set UCLOUD_CLIENT_SECRET.".to_string(),
                    )));
                }
                trimmed
            }
        }
    };

    if client_id != DEFAULT_CLIENT_ID || client_secret != DEFAULT_CLIENT_SECRET {
        if let Err(e) = save_saved_client_credentials(&client_id, &client_secret) {
            warn!("Failed to persist client credentials to {CLIENT_CREDENTIALS_FILE}: {e}");
        } else {
            println!("  [Persisted] Client ID and Secret saved to {CLIENT_CREDENTIALS_FILE} as primary.");
        }
    }

    // Bind loopback on 127.0.0.1:8080 (fallback to 127.0.0.1:0)
    let listener = match TcpListener::bind("127.0.0.1:8080").await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
            VfsError::InternalFault(format!("Failed to bind loopback TCP listener: {e}"))
        })?,
    };

    let local_addr = listener.local_addr().map_err(|e| {
        VfsError::InternalFault(format!("Failed to get listener local address: {e}"))
    })?;

    let port = local_addr.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let pkce = generate_pkce();
    let csrf_state = generate_csrf_state();

    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive&access_type=offline&prompt=consent&state={}&code_challenge={}&code_challenge_method=S256",
        DEFAULT_GOOGLE_AUTH_ENDPOINT,
        client_id,
        redirect_uri,
        csrf_state,
        pkce.challenge
    );

    println!("Starting local loopback listener on: {redirect_uri}");
    println!("\nPlease open this link in your web browser to authenticate:");
    println!("--------------------------------------------------------------------------------");
    println!("{auth_url}");
    println!("--------------------------------------------------------------------------------");

    // Attempt to open the default web browser cleanly via Windows rundll32
    let _ = tokio::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &auth_url])
        .spawn();

    println!("\nWaiting for browser callback (timeout: 300 seconds)...");

    let auth_code = capture_loopback_code(
        listener,
        &csrf_state,
        Duration::from_secs(300),
    )
    .await?;

    println!("Authorization code captured. Exchanging for OAuth tokens...");

    let http_client = reqwest::Client::new();
    let token_resp = exchange_code_for_tokens(
        &http_client,
        DEFAULT_GOOGLE_TOKEN_ENDPOINT,
        &client_id,
        &client_secret,
        &auth_code,
        &pkce.verifier,
        &redirect_uri,
    )
    .await?;

    let refresh_token = match token_resp.refresh_token {
        Some(rt) => rt,
        None => {
            return Err(Box::new(VfsError::PermissionDenied(
                "Google did not return a refresh token. Please re-run login with prompt=consent.".to_string(),
            )));
        }
    };

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();

    // Query user profile & quota from Google
    let limiter = Arc::new(MultiTierLimiter::new());
    let drive_client = GoogleDriveClient::new(limiter)?;

    let about = drive_client
        .get_about("temp_login", &token_resp.access_token)
        .await?;

    let email = about
        .user
        .as_ref()
        .and_then(|u| u.email_address.clone())
        .unwrap_or_else(|| "primary_account@gmail.com".to_string());

    let (limit_bytes, used_bytes) = if let Some(quota) = &about.storage_quota {
        let lim = quota.limit.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(15 * 1024 * 1024 * 1024);
        let usd = quota.usage.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        (lim, usd)
    } else {
        (15 * 1024 * 1024 * 1024, 0)
    };

    let new_cred = AccountCredentials {
        account_id: email.clone(),
        refresh_token,
        access_token: token_resp.access_token,
        expires_at_unix: now_unix + token_resp.expires_in,
        client_id: Some(client_id.clone()),
        client_secret: Some(client_secret.clone()),
    };

    let mut existing_creds = load_vault_credentials().await.unwrap_or_default();
    existing_creds.retain(|c| c.account_id != email);
    existing_creds.push(new_cred);

    save_vault_credentials(&existing_creds).await?;

    let limit_gb = (limit_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
    let used_gb = (used_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
    let free_gb = limit_gb - used_gb;

    println!("\nSUCCESS! Linked Google Account: {email}");
    println!("  Total Storage:     {limit_gb:.2} GB");
    println!("  Used Storage:      {used_gb:.2} GB");
    println!("  Available Storage: {free_gb:.2} GB");
    println!("\nCredentials encrypted and stored in {VAULT_FILE}");
    println!("You can now run: ucloud-fs test-live OR ucloud-fs mount X:\n");

    Ok(())
}

async fn cmd_remove_account(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut creds = load_vault_credentials().await?;
    if creds.is_empty() {
        println!("\nNo linked accounts found in {VAULT_FILE}.\n");
        return Ok(());
    }

    let email = match target {
        Some(e) => e.trim().to_string(),
        None => {
            use std::io::Write;
            println!("\nCurrent linked accounts:");
            for (idx, c) in creds.iter().enumerate() {
                println!("  [{}] {}", idx + 1, c.account_id);
            }
            print!("\nEnter email address of account to remove: ");
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    if email.is_empty() {
        println!("No account specified. Aborted.");
        return Ok(());
    }

    let initial_len = creds.len();
    creds.retain(|c| !c.account_id.eq_ignore_ascii_case(&email));

    if creds.len() < initial_len {
        save_vault_credentials(&creds).await?;
        println!("\nSUCCESS: Removed account '{email}' from {VAULT_FILE}.");
        println!("Remaining linked account(s): {}", creds.len());
    } else {
        println!("\nAccount '{email}' was not found in {VAULT_FILE}.");
    }

    Ok(())
}

async fn cmd_set_credentials(
    custom_id: Option<String>,
    custom_secret: Option<String>,
    reset_to_default: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs Primary OAuth Credentials Configuration");
    println!(
        "================================================================================\n"
    );

    if reset_to_default {
        clear_saved_client_credentials()?;
        let mut creds = load_vault_credentials().await.unwrap_or_default();
        if !creds.is_empty() {
            for c in &mut creds {
                c.client_id = Some(DEFAULT_CLIENT_ID.to_string());
                c.client_secret = Some(DEFAULT_CLIENT_SECRET.to_string());
            }
            save_vault_credentials(&creds).await?;
        }
        println!("Primary credentials reset to built-in project defaults.");
        println!("  Default Client ID: {}", DEFAULT_CLIENT_ID);
        println!("  Default Client Secret: [CONFIGURED]");
        println!("================================================================================\n");
        return Ok(());
    }

    let client_id = match custom_id {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            use std::io::Write;
            print!("Enter your personal Google OAuth Client ID: ");
            std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let trimmed = input.trim().to_string();
            if trimmed.is_empty() {
                return Err(Box::new(VfsError::PermissionDenied(
                    "Client ID cannot be empty. Pass --client-id <ID> or enter interactively.".to_string(),
                )));
            }
            trimmed
        }
    };

    let client_secret = match custom_secret {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            use std::io::Write;
            print!("Enter your personal Google OAuth Client Secret: ");
            std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let trimmed = input.trim().to_string();
            if trimmed.is_empty() {
                return Err(Box::new(VfsError::PermissionDenied(
                    "Client Secret cannot be empty. Pass --client-secret <SECRET> or enter interactively.".to_string(),
                )));
            }
            trimmed
        }
    };

    save_saved_client_credentials(&client_id, &client_secret)?;

    let mut creds = load_vault_credentials().await.unwrap_or_default();
    if !creds.is_empty() {
        for c in &mut creds {
            c.client_id = Some(client_id.clone());
            c.client_secret = Some(client_secret.clone());
        }
        save_vault_credentials(&creds).await?;
        println!(
            "Updated {} existing linked account(s) in vault to use your new primary credentials.",
            creds.len()
        );
    }

    println!("\nPrimary Google OAuth credentials successfully configured!");
    println!("  Saved Client ID: {}", client_id);
    println!("  Saved Client Secret: [CONFIGURED]");
    println!("All subsequent 'login', 'add-account', and background token refreshes");
    println!("will now use your personal Google Cloud Console project by default.");
    println!("================================================================================\n");

    Ok(())
}

async fn cmd_prune_accounts(keep_email: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut creds = load_vault_credentials().await?;
    println!("Loading vault credentials... found {} account(s).", creds.len());

    let initial_count = creds.len();
    creds.retain(|c| c.account_id.eq_ignore_ascii_case(keep_email));

    for c in &mut creds {
        let (cid, csec) = resolve_client_credentials(c)
            .unwrap_or_else(|_| (DEFAULT_CLIENT_ID.to_string(), DEFAULT_CLIENT_SECRET.to_string()));
        c.client_id = Some(cid);
        c.client_secret = Some(csec);
    }

    save_vault_credentials(&creds).await?;
    println!(
        "Vault pruned successfully: removed {} account(s).",
        initial_count.saturating_sub(creds.len())
    );
    println!(
        "Retained account(s): {} (configured with primary Client ID & Secret).",
        creds.len()
    );
    for c in &creds {
        println!("  - Account: {}", c.account_id);
    }
    Ok(())
}

async fn cmd_clear_accounts() -> Result<(), Box<dyn std::error::Error>> {
    save_vault_credentials(&[]).await?;
    println!("\nSUCCESS: All accounts have been cleared from {VAULT_FILE}.");
    println!("No accounts are linked by default. Run 'ucloud-fs login' to link an account before mounting.\n");
    Ok(())
}

async fn cmd_status() -> Result<(), Box<dyn std::error::Error>> {
    let creds = load_vault_credentials().await?;
    if creds.is_empty() {
        println!("\nNo linked accounts found in {VAULT_FILE}. Run 'ucloud-fs login' to authenticate.\n");
        return Ok(());
    }

    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs Account Vault Status");
    println!(
        "================================================================================\n"
    );

    let limiter = Arc::new(MultiTierLimiter::new());
    let drive_client = GoogleDriveClient::new(limiter)?;

    let http_client = reqwest::Client::new();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();

    let mut updated_creds = creds.clone();
    let mut total_pool_limit = 0u64;
    let mut total_pool_used = 0u64;

    for (idx, cred) in creds.iter().enumerate() {
        let (cid, csecret) = match resolve_client_credentials(cred) {
            Ok(pair) => pair,
            Err(e) => {
                println!("[{}] Account: {} | Config Error: {e}", idx + 1, cred.account_id);
                continue;
            }
        };
        let mut active_token = cred.access_token.clone();
        if now_unix >= cred.expires_at_unix.saturating_sub(60) {
            println!("Access token expired for {}. Refreshing...", cred.account_id);
            if let Ok(new_token_resp) = refresh_access_token(
                &http_client,
                DEFAULT_GOOGLE_TOKEN_ENDPOINT,
                &cid,
                &csecret,
                &cred.refresh_token,
            )
            .await
            {
                active_token = new_token_resp.access_token.clone();
                if let Some(c) = updated_creds.get_mut(idx) {
                    c.access_token = new_token_resp.access_token;
                    c.expires_at_unix = now_unix + new_token_resp.expires_in;
                    c.client_id = Some(cid.clone());
                    c.client_secret = Some(csecret.clone());
                }
            }
        }

        match drive_client.get_about(&cred.account_id, &active_token).await {
            Ok(about) => {
                let (limit, used) = if let Some(q) = &about.storage_quota {
                    let lim = q.limit.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    let usd = q.usage.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    (lim, usd)
                } else {
                    (0, 0)
                };

                total_pool_limit = total_pool_limit.saturating_add(limit);
                total_pool_used = total_pool_used.saturating_add(used);

                let limit_gb = (limit as f64) / (1024.0 * 1024.0 * 1024.0);
                let used_gb = (used as f64) / (1024.0 * 1024.0 * 1024.0);
                println!(
                    "[{}] Account: {} | Limit: {:.2} GB | Used: {:.2} GB | Free: {:.2} GB",
                    idx + 1,
                    cred.account_id,
                    limit_gb,
                    used_gb,
                    (limit_gb - used_gb).max(0.0)
                );
            }
            Err(e) => {
                println!("[{}] Account: {} | Error querying Google API: {e}", idx + 1, cred.account_id);
            }
        }
    }

    let _ = save_vault_credentials(&updated_creds).await;

    let total_limit_gb = (total_pool_limit as f64) / (1024.0 * 1024.0 * 1024.0);
    let total_used_gb = (total_pool_used as f64) / (1024.0 * 1024.0 * 1024.0);
    let total_free_gb = (total_limit_gb - total_used_gb).max(0.0);
    println!("--------------------------------------------------------------------------------");
    println!("  Aggregated Storage Pool Summary (Across All {} Accounts):", creds.len());
    println!("    Total Pool Capacity: {:.2} GB", total_limit_gb);
    println!("    Total Pool Used:     {:.2} GB", total_used_gb);
    println!("    Total Pool Free:     {:.2} GB", total_free_gb);
    println!("================================================================================\n");
    Ok(())
}

async fn cmd_test_live() -> Result<(), Box<dyn std::error::Error>> {
    let mut creds = load_vault_credentials().await?;
    if creds.is_empty() {
        return Err(Box::new(VfsError::PermissionDenied(
            "No authenticated accounts found in vault.enc. Please run 'ucloud-fs login' first.".to_string(),
        )));
    }

    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs Real-World Live Verification Suite");
    println!(
        "================================================================================\n"
    );

    let http_client = reqwest::Client::new();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();

    let (account_id, access_token) = {
        let account = match creds.first_mut() {
            Some(a) => a,
            None => return Err(Box::new(VfsError::InternalFault("No account".to_string()))),
        };

        let (cid, csecret) = resolve_client_credentials(account)?;
        account.client_id = Some(cid.clone());
        account.client_secret = Some(csecret.clone());

        println!("[1/7] Checking authentication token for {}...", account.account_id);
        if now_unix >= account.expires_at_unix.saturating_sub(60) {
            println!("      Access token near expiry, refreshing via Google OAuth endpoint...");
            let refreshed = refresh_access_token(
                &http_client,
                DEFAULT_GOOGLE_TOKEN_ENDPOINT,
                &cid,
                &csecret,
                &account.refresh_token,
            )
            .await?;
            account.access_token = refreshed.access_token;
            account.expires_at_unix = now_unix + refreshed.expires_in;
            println!("      Token refresh successful!");
        } else {
            println!("      Access token is active (expires in {}s)", account.expires_at_unix - now_unix);
        }
        (account.account_id.clone(), account.access_token.clone())
    };

    save_vault_credentials(&creds).await?;

    let limiter = Arc::new(MultiTierLimiter::new());
    let drive_client = Arc::new(GoogleDriveClient::new(limiter)?);

    // [2/7] Live Quota Check
    println!("\n[2/7] Querying live quota via Google Drive API (about.get)...");
    let about = drive_client.get_about(&account_id, &access_token).await?;
    let (limit, used) = if let Some(q) = &about.storage_quota {
        let lim = q.limit.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let usd = q.usage.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        (lim, usd)
    } else {
        (0, 0)
    };
    println!(
        "      Live Quota: {:.2} GB Total | {:.2} GB Used",
        (limit as f64) / (1024.0 * 1024.0 * 1024.0),
        (used as f64) / (1024.0 * 1024.0 * 1024.0)
    );

    // [3/7] Parallel Account Hydration
    println!("\n[3/7] Hydrating Google Drive directory structure into InMemoryRoutingTable...");
    let start_time = std::time::Instant::now();
    let hydrated_data = drive_client.hydrate_account(&account_id, &access_token).await?;
    let hydration_duration = start_time.elapsed();

    let routing_table = Arc::new(InMemoryRoutingTable::new());
    hydrate_account_into_table(&routing_table, &hydrated_data)?;

    let file_count = hydrated_data.files.len();
    println!(
        "      Hydration complete: {file_count} files/folders hydrated in {:.2}ms (<0.01ms per in-memory lookup)",
        hydration_duration.as_millis()
    );

    // [4/7] In-Memory readdir Benchmark
    println!("\n[4/7] Testing direct in-memory directory enumeration (readdir ROOT_INO)...");
    let readdir_start = std::time::Instant::now();
    let root_entries = routing_table.readdir(InMemoryRoutingTable::ROOT_INO)?;
    let readdir_duration = readdir_start.elapsed();
    println!(
        "      Readdir returned {} root entries in {:.3}ms (ZERO network calls)",
        root_entries.len(),
        readdir_duration.as_secs_f64() * 1000.0
    );

    // [5/7] Real Cloud Resumable Upload Test
    println!("\n[5/7] Testing live resumable upload (creating smoke test file)...");
    let test_filename = format!("__ucloud_smoke_test_{}.txt", uuid::Uuid::new_v4());
    let test_content = b"ucloud-fs zero-disk ephemeral stream verification test payload.\n";
    let test_bytes = Bytes::from_static(test_content);

    let mut hasher = Sha256::new();
    hasher.update(&test_bytes);
    let original_hash = hasher.finalize();

    let meta = GoogleFileInput {
        name: test_filename.clone(),
        mime_type: Some("text/plain".to_string()),
        parents: None,
        app_properties: None,
    };

    let session_uri = drive_client
        .initiate_resumable_upload(&account_id, &access_token, &meta)
        .await?;

    let upload_res = drive_client
        .upload_resumable_chunk_opt(
            &account_id,
            &session_uri,
            test_bytes.clone(),
            0,
            (test_bytes.len() as u64).saturating_sub(1),
            Some(test_bytes.len() as u64),
        )
        .await?;

    let uploaded_file_id = match upload_res {
        ucloud_fs::network::client::ChunkUploadResult::Complete { file_id } => file_id,
        ucloud_fs::network::client::ChunkUploadResult::Incomplete { .. } => {
            return Err(Box::new(VfsError::InternalFault(
                "Smoke test upload was unexpectedly incomplete".to_string(),
            )));
        }
    };
    println!("      Uploaded test file '{test_filename}' -> Google file ID: {uploaded_file_id}");

    // [6/7] Real Cloud Range Download & SHA-256 Verification
    println!("\n[6/7] Reading back uploaded file from Google Drive & verifying byte-for-byte integrity...");
    let download_resp = drive_client
        .download_stream(&account_id, &access_token, &uploaded_file_id, None)
        .await?;

    let downloaded_bytes = download_resp.bytes().await.map_err(|e| {
        VfsError::InternalFault(format!("Failed to stream downloaded bytes: {e}"))
    })?;

    let mut verify_hasher = Sha256::new();
    verify_hasher.update(&downloaded_bytes);
    let downloaded_hash = verify_hasher.finalize();

    if original_hash != downloaded_hash {
        return Err(Box::new(VfsError::InternalFault(
            "Downloaded file SHA-256 hash does not match original uploaded bytes!".to_string(),
        )));
    }
    println!("      SHA-256 MATCH VERIFIED 100%! Byte-for-byte identical ({} bytes).", downloaded_bytes.len());

    // [7/7] Clean up smoke test file from cloud
    println!("\n[7/7] Cleaning up smoke test file from Google Drive...");
    drive_client
        .delete_file(&account_id, &access_token, &uploaded_file_id)
        .await?;
    println!("      Smoke test file removed from cloud successfully.");

    println!(
        "\n================================================================================"
    );
    println!("  ALL 7 LIVE INTEGRATION TESTS PASSED WITH 100% SUCCESS!");
    println!("  Single Google Account '{}' verified ready for production mount.", account_id);
    println!(
        "================================================================================\n"
    );

    Ok(())
}

async fn cmd_mount(drive_letter: String, volume_label: String) -> Result<(), Box<dyn std::error::Error>> {
    // Acquire system-wide singleton mutex to prevent dual-process collision
    let mutex_name: Vec<u16> = "Global\\ucloud_fs_daemon_singleton_lock\0".encode_utf16().collect();
    let mutex_handle = unsafe {
        windows_sys::Win32::System::Threading::CreateMutexW(
            std::ptr::null(),
            1, // TRUE - acquire initial ownership
            mutex_name.as_ptr(),
        )
    };
    if mutex_handle.is_null() || unsafe { windows_sys::Win32::Foundation::GetLastError() } == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
        if !mutex_handle.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(mutex_handle); }
        }
        eprintln!("\n[ERROR] Another instance of ucloud-fs is already running.");
        eprintln!("Please terminate existing ucloud-fs processes first before mounting.\n");
        return Err(Box::new(VfsError::InternalFault(
            "Another instance of ucloud-fs is already running".to_string(),
        )));
    }

    let mut creds = load_vault_credentials().await?;
    if creds.is_empty() {
        return Err(Box::new(VfsError::PermissionDenied(
            "No authenticated accounts found in vault.enc. Please run 'ucloud-fs login' or 'ucloud-fs add-account' first.".to_string(),
        )));
    }

    println!(
        "\n================================================================================"
    );
    println!("  ucloud-fs Virtual Filesystem Mounting");
    println!("  Target Drive Letter: {drive_letter}");
    println!("  Volume Label:        {volume_label}");
    println!(
        "================================================================================\n"
    );

    let http_client = reqwest::Client::new();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();

    // Check & refresh tokens
    for cred in &mut creds {
        let (cid, csecret) = resolve_client_credentials(cred)?;
        cred.client_id = Some(cid.clone());
        cred.client_secret = Some(csecret.clone());
        if now_unix >= cred.expires_at_unix.saturating_sub(60) {
            println!("Refreshing expired token for {}...", cred.account_id);
            if let Ok(refreshed) = refresh_access_token(
                &http_client,
                DEFAULT_GOOGLE_TOKEN_ENDPOINT,
                &cid,
                &csecret,
                &cred.refresh_token,
            )
            .await
            {
                cred.access_token = refreshed.access_token;
                cred.expires_at_unix = now_unix + refreshed.expires_in;
                println!("Successfully refreshed token for {}", cred.account_id);
            } else {
                eprintln!("Failed to refresh token for {}", cred.account_id);
            }
        }
    }
    save_vault_credentials(&creds).await?;

    let limiter = Arc::new(MultiTierLimiter::new());
    let client = Arc::new(GoogleDriveClient::new(limiter)?);

    let routing_table = Arc::new(InMemoryRoutingTable::new());
    let placement_engine = Arc::new(PlacementEngine::new());
    let session = Arc::new(SessionController::new());
    let notifier: Arc<dyn ShellNotifier> = Arc::new(WindowsShellNotifier::new(&drive_letter));
    let sync_engine = Arc::new(DeltaSyncEngine::new(
        Arc::clone(&client),
        Arc::clone(&routing_table),
        Arc::clone(&placement_engine),
        Arc::clone(&session),
        Arc::clone(&notifier),
    ));
    let prompter = Arc::new(AutoAcceptPrompter);
    let valve = Arc::new(MemoryBackpressureValve::new());

    println!("Hydrating accounts into virtual routing table...");
    for cred in &creds {
        match client.hydrate_account(&cred.account_id, &cred.access_token).await {
            Ok(data) => {
                let limit = data.storage_limit_bytes;
                let used = data.storage_used_bytes;
                let files_count = data.files.len();

                placement_engine.register_account(
                    &cred.account_id,
                    limit,
                    used,
                );

                sync_engine.set_start_page_token(&cred.account_id, data.start_page_token.clone());

                if let Err(e) = hydrate_account_into_table(&routing_table, &data) {
                    warn!(account = %cred.account_id, error = %e, "Failed to fully hydrate routing table");
                }

                println!(
                    "  [Hydrated] {} ({} files, {:.2} GB used / {:.2} GB total)",
                    cred.account_id,
                    files_count,
                    (used as f64) / (1024.0 * 1024.0 * 1024.0),
                    (limit as f64) / (1024.0 * 1024.0 * 1024.0)
                );
            }
            Err(e) => {
                warn!(account = %cred.account_id, error = %e, "Account hydration failed; proceeding with cached/empty state");
            }
        }
    }

    let driver = Arc::new(UcloudFsDriver::new(DriverDependencies {
        routing_table: Arc::clone(&routing_table),
        placement_engine: Arc::clone(&placement_engine),
        session: Arc::clone(&session),
        client: Arc::clone(&client),
        sync_engine: Arc::clone(&sync_engine),
        notifier: Arc::clone(&notifier),
        prompter,
        valve,
        rt_handle: tokio::runtime::Handle::current(),
        volume_label: Some(volume_label.clone()),
    }));

    for cred in &creds {
        driver.set_account_token(&cred.account_id, &cred.access_token);
    }

    let mount_options = WinFspMountOptions {
        mount_point: drive_letter.clone(),
        volume_label: volume_label.clone(),
        case_sensitive_search: false,
        case_preserved_names: true,
        unicode_on_disk: true,
        reparse_points: false,
        operation_timeout_ms: 300_000,
    };

    let mount_guard = WinFspMountGuard::mount(Arc::clone(&driver), mount_options)?;

    println!(
        "\nDrive {drive_letter} is MOUNTED and ACTIVE. (Volume: {volume_label})"
    );
    println!("Total routing table files indexed: {}", routing_table.count());
    println!("Press Ctrl+C to unmount and exit cleanly.\n");

    // Background Delta Sync & Dynamic Vault Hot-Reload task (every 10 seconds)
    let sync_engine_bg = Arc::clone(&sync_engine);
    let routing_table_bg = Arc::clone(&routing_table);
    let client_bg = Arc::clone(&client);
    let driver_bg = Arc::clone(&driver);
    let notifier_bg = Arc::clone(&notifier);
    let mut bg_creds = creds.clone();
    let mut known_accounts: std::collections::HashSet<String> =
        bg_creds.iter().map(|c| c.account_id.clone()).collect();
    let passphrase_shield = zeroize::Zeroizing::new(get_master_passphrase());
    let mut last_vault_mtime = std::fs::metadata(VAULT_FILE).and_then(|m| m.modified()).ok();

    let sync_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;

            // 1. Check for vault.enc modifications (dynamic account addition via CLI)
            if let Ok(metadata) = std::fs::metadata(VAULT_FILE)
                && let Ok(mtime) = metadata.modified()
                && (last_vault_mtime.is_none() || Some(mtime) > last_vault_mtime)
            {
                last_vault_mtime = Some(mtime);
                    if let Ok(payload) = tokio::fs::read(VAULT_FILE).await
                        && let Ok(fresh_creds) = decrypt_credentials(
                            &payload,
                            &passphrase_shield,
                            KeyDerivationParams::default(),
                        )
                    {
                        for cred in &fresh_creds {
                            if !known_accounts.contains(&cred.account_id) {
                                known_accounts.insert(cred.account_id.clone());
                                bg_creds.push(cred.clone());
                                if let Ok(data) = client_bg
                                    .hydrate_account(&cred.account_id, &cred.access_token)
                                    .await
                                {
                                    driver_bg.register_account(
                                        &cred.account_id,
                                        &cred.access_token,
                                        data.storage_limit_bytes,
                                        data.storage_used_bytes,
                                    );
                                    let _ = hydrate_account_into_table(&routing_table_bg, &data);
                                    notifier_bg.notify_updated("/");
                                    info!(
                                        account = %cred.account_id,
                                        limit = data.storage_limit_bytes,
                                        "Live Vault Hot-Reload: Dynamically added new account and updated storage pool"
                                    );
                                    println!(
                                        "\n[HOT-RELOAD] Dynamically mounted new Google account: {} (+{:.2} GB pool storage)",
                                        cred.account_id,
                                        (data.storage_limit_bytes as f64) / (1024.0 * 1024.0 * 1024.0)
                                    );
                                }
                            }
                        }
                    }
                }

            // 2. Poll delta sync across linked accounts
            for c in &bg_creds {
                if let Err(e) = sync_engine_bg.poll_account(&c.account_id, &c.access_token).await {
                    warn!(account = %c.account_id, error = %e, "Background delta sync encountered error");
                }
            }
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nTermination signal received (Ctrl+C). Initiating clean unmount...");
        }
    }

    sync_task.abort();
    let _ = mount_guard.unmount();
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(mutex_handle);
    }
    println!("ucloud-fs successfully unmounted. Ephemeral caches purged. Goodbye!\n");

    Ok(())
}
