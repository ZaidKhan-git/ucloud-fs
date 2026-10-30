use crate::error::VfsError;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Memory-locked secret buffer that locks RAM pages on Windows to prevent OS paging to swap.
struct MemoryLockGuard {
    #[cfg(windows)]
    ptr: *mut std::ffi::c_void,
    #[cfg(windows)]
    size: usize,
}

impl MemoryLockGuard {
    fn lock(slice: &[u8]) -> Self {
        #[cfg(windows)]
        {
            let ptr = slice.as_ptr() as *mut std::ffi::c_void;
            let size = slice.len();
            // SAFETY: `slice` is a valid, contiguous byte slice in heap or stack memory
            // allocated by this process. `VirtualLock` informs the Windows kernel to lock
            // this memory range into the physical working set, preventing paging to pagefile.sys.
            // If the OS call fails (e.g. working set quota exceeded), the memory is still safe to use.
            unsafe {
                windows_sys::Win32::System::Memory::VirtualLock(ptr, size);
            }
            Self { ptr, size }
        }
        #[cfg(not(windows))]
        {
            let _ = slice;
            Self {}
        }
    }
}

impl Drop for MemoryLockGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: `self.ptr` and `self.size` correspond directly to the valid memory
        // region previously locked via `VirtualLock` in `MemoryLockGuard::lock`.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualUnlock(self.ptr, self.size);
        }
    }
}

/// Secure container for a linked Google account's OAuth credentials.
/// Implements `Zeroize` and `ZeroizeOnDrop` so tokens are cleared to 0x00 upon leaving scope.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop, PartialEq, Eq)]
pub struct AccountCredentials {
    pub account_id: String,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_at_unix: u64,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl fmt::Debug for AccountCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountCredentials")
            .field("account_id", &self.account_id)
            .field("refresh_token", &"[REDACTED]")
            .field("access_token", &"[REDACTED]")
            .field("expires_at_unix", &self.expires_at_unix)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

/// Parameters for Argon2id key derivation.
#[derive(Clone, Copy)]
pub struct KeyDerivationParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KeyDerivationParams {
    fn default() -> Self {
        // Standard Argon2id configuration (64 MB memory cost, 3 iterations)
        Self {
            memory_kib: 64 * 1024,
            iterations: 3,
            parallelism: 1,
        }
    }
}

impl KeyDerivationParams {
    /// Fast parameters for tests to avoid multi-second hashing overhead during test runs
    #[must_use]
    pub const fn fast_test() -> Self {
        Self {
            memory_kib: 1024,
            iterations: 1,
            parallelism: 1,
        }
    }
}

/// Encrypts account credentials into a single encrypted binary payload.
/// Format: `[16-byte salt][12-byte nonce][ciphertext + 16-byte Poly1305 tag]`.
pub fn encrypt_credentials(
    credentials: &[AccountCredentials],
    passphrase: &str,
    params: KeyDerivationParams,
) -> Result<Vec<u8>, VfsError> {
    if passphrase.is_empty() {
        return Err(VfsError::PermissionDenied(
            "Master passphrase cannot be empty".to_string(),
        ));
    }

    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);

    let mut derived_key = [0u8; KEY_LEN];
    let _lock_guard = MemoryLockGuard::lock(&derived_key);

    let argon_params = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|e| VfsError::InternalFault(format!("Invalid Argon2 parameters: {}", e)))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    argon2
        .hash_password_into(passphrase.as_bytes(), &salt, &mut derived_key)
        .map_err(|e| VfsError::InternalFault(format!("Key derivation failed: {}", e)))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let mut plaintext_json = serde_json::to_vec(credentials)
        .map_err(|e| VfsError::InternalFault(format!("Failed to serialize credentials: {}", e)))?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&derived_key));
    let ciphertext = cipher
        .encrypt(nonce, plaintext_json.as_ref())
        .map_err(|_| VfsError::InternalFault("AEAD encryption failed".to_string()))?;

    // Zeroize intermediate plaintext bytes immediately
    plaintext_json.zeroize();
    derived_key.zeroize();

    let mut payload = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&salt);
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&ciphertext);

    Ok(payload)
}

/// Decrypts a binary payload into a list of account credentials.
pub fn decrypt_credentials(
    payload: &[u8],
    passphrase: &str,
    params: KeyDerivationParams,
) -> Result<Vec<AccountCredentials>, VfsError> {
    if payload.len() < (SALT_LEN + NONCE_LEN + 16) {
        return Err(VfsError::PermissionDenied(
            "Encrypted vault payload is corrupted or truncated".to_string(),
        ));
    }

    let salt = &payload[..SALT_LEN];
    let nonce_bytes = &payload[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &payload[SALT_LEN + NONCE_LEN..];

    let mut derived_key = [0u8; KEY_LEN];
    let _lock_guard = MemoryLockGuard::lock(&derived_key);

    let argon_params = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|e| VfsError::InternalFault(format!("Invalid Argon2 parameters: {}", e)))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut derived_key)
        .map_err(|_| VfsError::PermissionDenied("Authentication failed".to_string()))?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&derived_key));
    let nonce = Nonce::from_slice(nonce_bytes);

    let mut decrypted = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| VfsError::PermissionDenied("Authentication failed or corrupted ciphertext".to_string()))?;

    derived_key.zeroize();

    let credentials: Vec<AccountCredentials> = serde_json::from_slice(&decrypted)
        .map_err(|_| VfsError::PermissionDenied("Failed to parse decrypted credentials".to_string()))?;

    decrypted.zeroize();

    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_roundtrip() {
        let creds = vec![
            AccountCredentials {
                account_id: "user1@gmail.com".to_string(),
                refresh_token: "1//refresh_token_alpha".to_string(),
                access_token: "ya29.access_token_alpha".to_string(),
                expires_at_unix: 1800000000,
                client_id: None,
                client_secret: None,
            },
            AccountCredentials {
                account_id: "user2@gmail.com".to_string(),
                refresh_token: "1//refresh_token_beta".to_string(),
                access_token: "ya29.access_token_beta".to_string(),
                expires_at_unix: 1800000000,
                client_id: None,
                client_secret: None,
            },
        ];

        let passphrase = "correct_horse_battery_staple";
        let params = KeyDerivationParams::fast_test();

        let encrypted = encrypt_credentials(&creds, passphrase, params).unwrap();
        assert!(!encrypted.is_empty());

        let decrypted = decrypt_credentials(&encrypted, passphrase, params).unwrap();
        assert_eq!(creds, decrypted);
    }

    #[test]
    fn test_vault_wrong_passphrase() {
        let creds = vec![AccountCredentials {
            account_id: "test@gmail.com".to_string(),
            refresh_token: "token".to_string(),
            access_token: "access".to_string(),
            expires_at_unix: 100,
            client_id: None,
            client_secret: None,
        }];
        let params = KeyDerivationParams::fast_test();

        let encrypted = encrypt_credentials(&creds, "passphrase_1", params).unwrap();
        let result = decrypt_credentials(&encrypted, "wrong_passphrase", params);

        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_vault_ciphertext_tamper_fuzzing() {
        let creds = vec![AccountCredentials {
            account_id: "victim@gmail.com".to_string(),
            refresh_token: "secret_refresh".to_string(),
            access_token: "secret_access".to_string(),
            expires_at_unix: 5000,
            client_id: None,
            client_secret: None,
        }];
        let passphrase = "fuzz_master_password";
        let params = KeyDerivationParams::fast_test();

        let encrypted = encrypt_credentials(&creds, passphrase, params).unwrap();

        // Flip random bits across different positions in the payload
        let mut rng = rand::thread_rng();
        for _ in 0..100 {
            let mut tampered = encrypted.clone();
            let byte_idx = (rng.next_u32() as usize) % tampered.len();
            let bit_idx = (rng.next_u32() % 8) as u8;
            tampered[byte_idx] ^= 1 << bit_idx;

            let res = decrypt_credentials(&tampered, passphrase, params);
            assert!(res.is_err(), "Tampered ciphertext must never decrypt successfully");
        }
    }
}
