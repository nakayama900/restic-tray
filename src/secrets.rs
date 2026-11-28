//! DPAPI-protected credential store for Cloudflare R2 keys and Restic repository password.
//!
//! Binds decryption to the current Windows user via `CryptProtectData` / `CryptUnprotectData`
//! with application-scoped secondary entropy, wipes plaintext buffers via `zeroize::Zeroize`,
//! and writes atomically.

#[cfg(windows)]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
#[cfg(windows)]
use tracing::{info, warn};
use zeroize::Zeroize;
#[cfg(windows)]
use zeroize::Zeroizing;

pub const CREDENTIALS_FILENAME: &str = "credentials.dat";

#[cfg(windows)]
/// Secondary DPAPI entropy scoping encrypted blobs to Restic Tray.
/// Kept unchanged across the rename from "R2 Restic Tray" so existing `credentials.dat`
/// blobs remain decryptable without re-entering secrets.
const ENTROPY: &[u8] = b"r2-restic-tray-credentials-v1";

pub type Result<T> = std::result::Result<T, SecretsError>;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("DPAPI is only available on Windows")]
    UnsupportedPlatform,

    #[error("DPAPI CryptProtectData failed (win32 error: {0})")]
    Encrypt(u32),

    #[error(
        "DPAPI CryptUnprotectData failed (win32 error: {0}); credentials.dat may belong to another Windows user or be corrupted"
    )]
    Decrypt(u32),

    #[cfg(not(windows))]
    #[error("OS keychain error: {0}")]
    Keyring(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("credentials.dat JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Sensitive credentials encrypted at rest in `credentials.dat` and wiped on drop.
#[derive(Debug, Default, Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct StoredSecrets {
    #[serde(default)]
    pub r2_access_key_id: Option<String>,
    #[serde(default)]
    pub r2_secret_access_key: Option<String>,
    #[serde(default)]
    pub restic_password: Option<String>,
}

/// Non-sensitive summary safe to expose to the GUI and MCP `get_status` tool.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsReadiness {
    pub has_access_key_id: bool,
    pub has_secret_access_key: bool,
    pub has_restic_password: bool,
    /// True when `credentials.dat` exists on disk but could not be decrypted or parsed
    /// (for example, copied from another Windows user or corrupted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub vault_unreadable: bool,
}

impl SecretsReadiness {
    pub fn is_complete(&self) -> bool {
        !self.vault_unreadable
            && self.has_access_key_id
            && self.has_secret_access_key
            && self.has_restic_password
    }

    /// Completeness for the active storage backend: the rclone `serve s3` adapter needs no R2
    /// credentials (rclone keeps its own in `rclone.conf`), only the Restic password.
    pub fn is_complete_for(&self, backend: crate::config::StorageBackend) -> bool {
        match backend {
            crate::config::StorageBackend::Rclone => {
                !self.vault_unreadable && self.has_restic_password
            }
            _ => self.is_complete(),
        }
    }
}

fn has_non_empty(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|s| !s.trim().is_empty())
}

impl StoredSecrets {
    pub fn is_empty(&self) -> bool {
        !has_non_empty(&self.r2_access_key_id)
            && !has_non_empty(&self.r2_secret_access_key)
            && !has_non_empty(&self.restic_password)
    }

    pub fn readiness(&self) -> SecretsReadiness {
        SecretsReadiness {
            has_access_key_id: has_non_empty(&self.r2_access_key_id),
            has_secret_access_key: has_non_empty(&self.r2_secret_access_key),
            has_restic_password: has_non_empty(&self.restic_password),
            vault_unreadable: false,
        }
    }
}

/// Windows DPAPI always works; macOS Keychain always works; on Linux the Secret Service needs a
/// running session D-Bus (absent in headless CI), so support is detected per call.
pub fn is_supported() -> bool {
    if cfg!(windows) || cfg!(target_os = "macos") {
        return true;
    }
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

pub fn credentials_path<P: AsRef<Path>>(base_dir: P) -> PathBuf {
    base_dir.as_ref().join("config").join(CREDENTIALS_FILENAME)
}

#[cfg(windows)]
pub fn save_secrets<P: AsRef<Path>>(base_dir: P, secrets: &StoredSecrets) -> Result<()> {
    if !is_supported() {
        return Err(SecretsError::UnsupportedPlatform);
    }
    let path = credentials_path(&base_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let plaintext = Zeroizing::new(serde_json::to_vec(secrets)?);
    let ciphertext = dpapi::protect(&plaintext)?;
    write_atomic(&path, &ciphertext)?;
    info!(
        "Credentials saved ({} bytes encrypted) at {}",
        ciphertext.len(),
        path.display()
    );
    Ok(())
}

/// Loads `credentials.dat`.
///
/// Returns `Ok(None)` ONLY when the vault file does not exist (or DPAPI is unsupported).
/// Decryption failures and invalid decrypted JSON return `Err(...)` so callers can distinguish
/// an absent vault from an unreadable one and never overwrite the original bytes on a partial save.
#[cfg(windows)]
pub fn load_secrets<P: AsRef<Path>>(base_dir: P) -> Result<Option<StoredSecrets>> {
    if !is_supported() {
        return Ok(None);
    }
    let path = credentials_path(&base_dir);
    let ciphertext = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(SecretsError::Io(err)),
    };
    let plaintext = match dpapi::unprotect(&ciphertext) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("Failed to decrypt credentials.dat (corrupt or another Windows user): {err}");
            return Err(err);
        }
    };
    let creds = match serde_json::from_slice::<StoredSecrets>(&plaintext) {
        Ok(creds) => creds,
        Err(err) => {
            warn!("credentials.dat has unexpected JSON structure: {err}");
            return Err(SecretsError::Json(err));
        }
    };
    Ok(Some(creds))
}

#[cfg(windows)]
pub fn clear_secrets<P: AsRef<Path>>(base_dir: P) -> Result<()> {
    let path = credentials_path(&base_dir);
    if !path.exists() {
        return Ok(());
    }
    let size = fs::metadata(&path)?.len() as usize;
    if size > 0 {
        use rand::Rng;
        let mut buf = vec![0u8; size];
        rand::rng().fill_bytes(&mut buf);
        fs::write(&path, &buf)?;
    }
    fs::remove_file(&path)?;
    info!("Credentials wiped and removed at {}", path.display());
    Ok(())
}

#[cfg(windows)]
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CREDENTIALS_FILENAME);
    let tmp = path.with_file_name(format!("{file_name}.{}.{seq}.tmp", std::process::id()));
    if let Err(err) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// OS keychain credential store for macOS (Keychain) and Linux (Secret Service via D-Bus).
///
/// Secrets never touch `config.json` or `credentials.dat` on these platforms; each field is one
/// keyring entry under this service name, wiped via `Zeroize` after use.
#[cfg(not(windows))]
mod keyring_store {
    use super::{Result, SecretsError, StoredSecrets};

    const SERVICE: &str = "io.github.nayuengin.restic-tray";
    const ENTRY_ACCESS_KEY_ID: &str = "r2-access-key-id";
    const ENTRY_SECRET_ACCESS_KEY: &str = "r2-secret-access-key";
    const ENTRY_RESTIC_PASSWORD: &str = "restic-repository-password";

    fn entry(user: &str) -> std::result::Result<keyring::Entry, SecretsError> {
        keyring::Entry::new(SERVICE, user).map_err(|err| SecretsError::Keyring(err.to_string()))
    }

    fn set(user: &str, value: Option<&str>) -> Result<()> {
        let entry = entry(user)?;
        match value.filter(|v| !v.is_empty()) {
            Some(v) => entry
                .set_password(v)
                .map_err(|err| SecretsError::Keyring(err.to_string())),
            None => entry.delete_credential().or_else(ignore_no_entry),
        }
    }

    fn get(user: &str) -> Result<Option<String>> {
        match entry(user)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(SecretsError::Keyring(err.to_string())),
        }
    }

    fn ignore_no_entry(err: keyring::Error) -> Result<()> {
        match err {
            keyring::Error::NoEntry => Ok(()),
            other => Err(SecretsError::Keyring(other.to_string())),
        }
    }

    pub fn save(secrets: &StoredSecrets) -> Result<()> {
        set(
            ENTRY_ACCESS_KEY_ID,
            secrets
                .r2_access_key_id
                .as_deref()
                .filter(|s| !s.is_empty()),
        )?;
        set(
            ENTRY_SECRET_ACCESS_KEY,
            secrets
                .r2_secret_access_key
                .as_deref()
                .filter(|s| !s.is_empty()),
        )?;
        set(
            ENTRY_RESTIC_PASSWORD,
            secrets.restic_password.as_deref().filter(|s| !s.is_empty()),
        )?;
        tracing::info!("Credentials saved to the OS keychain ({SERVICE})");
        Ok(())
    }

    pub fn load() -> Result<Option<StoredSecrets>> {
        let access_key = get(ENTRY_ACCESS_KEY_ID)?;
        let secret_key = get(ENTRY_SECRET_ACCESS_KEY)?;
        let password = get(ENTRY_RESTIC_PASSWORD)?;
        if access_key.is_none() && secret_key.is_none() && password.is_none() {
            return Ok(None);
        }
        Ok(Some(StoredSecrets {
            r2_access_key_id: access_key,
            r2_secret_access_key: secret_key,
            restic_password: password,
        }))
    }

    pub fn clear() -> Result<()> {
        for user in [
            ENTRY_ACCESS_KEY_ID,
            ENTRY_SECRET_ACCESS_KEY,
            ENTRY_RESTIC_PASSWORD,
        ] {
            entry(user)?.delete_credential().or_else(ignore_no_entry)?;
        }
        tracing::info!("Credentials removed from the OS keychain ({SERVICE})");
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn save_secrets<P: AsRef<Path>>(_base_dir: P, secrets: &StoredSecrets) -> Result<()> {
    if !is_supported() {
        return Err(SecretsError::UnsupportedPlatform);
    }
    keyring_store::save(secrets)
}

#[cfg(not(windows))]
pub fn load_secrets<P: AsRef<Path>>(_base_dir: P) -> Result<Option<StoredSecrets>> {
    if !is_supported() {
        return Ok(None);
    }
    keyring_store::load()
}

#[cfg(not(windows))]
pub fn clear_secrets<P: AsRef<Path>>(_base_dir: P) -> Result<()> {
    keyring_store::clear()
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod dpapi {
    use super::{ENTROPY, SecretsError};
    use std::ptr;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };
    use zeroize::{Zeroize, Zeroizing};

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    fn read_blob(blob: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if blob.cbData == 0 || blob.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: `blob` was populated by Windows DPAPI (`CryptProtectData` /
        // `CryptUnprotectData`), which guarantees `cbData` readable bytes at `pbData`.
        unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec() }
    }

    pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let input = blob(plaintext);
        let entropy = blob(ENTROPY);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: `input` and `entropy` point to valid slices for the duration of the FFI
        // call, and `output.pbData` is freed via `LocalFree` before returning.
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                0,
                &mut output,
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` is safe to call immediately after a failed Win32 API.
            return Err(SecretsError::Encrypt(unsafe { GetLastError() }));
        }
        let result = read_blob(&output);
        if !output.pbData.is_null() {
            // SAFETY: `output.pbData` was allocated by `CryptProtectData` using `LocalAlloc`.
            unsafe {
                LocalFree(output.pbData as _);
            }
        }
        Ok(result)
    }

    pub fn unprotect(ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
        let input = blob(ciphertext);
        let entropy = blob(ENTROPY);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: `input` and `entropy` point to valid slices for the duration of the FFI
        // call, and `output.pbData` is wiped and freed via `LocalFree` before returning.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                0,
                &mut output,
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` is safe to call immediately after a failed Win32 API.
            return Err(SecretsError::Decrypt(unsafe { GetLastError() }));
        }
        let result = Zeroizing::new(read_blob(&output));
        if !output.pbData.is_null() {
            if output.cbData > 0 {
                // SAFETY: `output.pbData` was allocated by `CryptUnprotectData` with
                // `output.cbData` writable bytes and is exclusively owned until `LocalFree`.
                unsafe {
                    std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize).zeroize();
                }
            }
            // SAFETY: `output.pbData` was allocated by `CryptUnprotectData` using `LocalAlloc`.
            unsafe {
                LocalFree(output.pbData as _);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_dpapi_secrets() {
        if !is_supported() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = StoredSecrets {
            r2_access_key_id: Some("r2-access-123".to_owned()),
            r2_secret_access_key: Some("r2-secret-xyz".to_owned()),
            restic_password: Some("repo-pass-999".to_owned()),
        };
        assert!(secrets.readiness().is_complete());

        save_secrets(dir.path(), &secrets).expect("save secrets");
        let loaded = load_secrets(dir.path())
            .expect("load secrets")
            .expect("present");
        assert_eq!(loaded.r2_access_key_id.as_deref(), Some("r2-access-123"));
        assert_eq!(
            loaded.r2_secret_access_key.as_deref(),
            Some("r2-secret-xyz")
        );
        assert_eq!(loaded.restic_password.as_deref(), Some("repo-pass-999"));

        clear_secrets(dir.path()).expect("clear secrets");
        assert!(
            load_secrets(dir.path())
                .expect("load after clear")
                .is_none()
        );
    }

    #[test]
    fn legacy_secrets_with_gdrive_fields_deserialize_and_drop_on_resave() {
        let legacy = r#"{
            "r2_access_key_id": "ak",
            "r2_secret_access_key": "sk",
            "restic_password": "pw",
            "gdrive_oauth_token": "{\"access_token\":\"ya29.test\",\"refresh_token\":\"1//ref\"}",
            "gdrive_client_secret": "csec-abc"
        }"#;
        let parsed: StoredSecrets = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.r2_access_key_id.as_deref(), Some("ak"));
        assert_eq!(parsed.r2_secret_access_key.as_deref(), Some("sk"));
        assert_eq!(parsed.restic_password.as_deref(), Some("pw"));
        assert!(parsed.readiness().is_complete());

        let resaved = serde_json::to_string(&parsed).unwrap();
        assert!(!resaved.contains("gdrive_oauth_token"));
        assert!(!resaved.contains("gdrive_client_secret"));

        let readiness_json = serde_json::to_string(&parsed.readiness()).unwrap();
        assert!(!readiness_json.contains("hasGdrive"));

        // Drive-only legacy JSON has no R2 keys and is incomplete.
        let drive_only: StoredSecrets = serde_json::from_str(
            r#"{"restic_password":"pw","gdrive_oauth_token":"{\"access_token\":\"a\"}"}"#,
        )
        .unwrap();
        assert!(!drive_only.readiness().is_complete());
        assert!(drive_only.readiness().has_restic_password);
    }

    #[cfg(windows)]
    #[test]
    fn corrupted_ciphertext_and_invalid_json_return_distinct_errors() {
        if !is_supported() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        // Missing file -> Ok(None)
        assert!(load_secrets(dir.path()).expect("absent").is_none());

        let path = credentials_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"corrupt-dpapi-blob").unwrap();
        assert!(matches!(
            load_secrets(dir.path()),
            Err(SecretsError::Decrypt(_))
        ));

        // Valid DPAPI wrapper around invalid JSON -> Err(SecretsError::Json(_))
        let bad_json_blob = dpapi::protect(b"{not-valid-json").expect("protect bad json");
        fs::write(&path, &bad_json_blob).unwrap();
        assert!(matches!(
            load_secrets(dir.path()),
            Err(SecretsError::Json(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_entropy_is_preserved_across_app_id_change() {
        // Must never change so existing credentials.dat files remain decryptable after folder copy.
        assert_eq!(ENTROPY, b"r2-restic-tray-credentials-v1");
        let path = credentials_path(crate::config::default_app_data_dir());
        assert!(
            path.ends_with(
                Path::new("io.github.nayuengin.restic-tray")
                    .join("config")
                    .join(CREDENTIALS_FILENAME)
            )
        );
    }
}
