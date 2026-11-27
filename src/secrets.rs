//! DPAPI-protected credential store for Cloudflare R2 keys and Restic repository password.
//!
//! Binds decryption to the current Windows user via `CryptProtectData` / `CryptUnprotectData`
//! with application-scoped secondary entropy, wipes plaintext buffers via `zeroize::Zeroize`,
//! and writes atomically.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use zeroize::{Zeroize, Zeroizing};

pub const CREDENTIALS_FILENAME: &str = "credentials.dat";

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

pub const fn is_supported() -> bool {
    cfg!(target_os = "windows")
}

pub fn credentials_path<P: AsRef<Path>>(base_dir: P) -> PathBuf {
    base_dir.as_ref().join("config").join(CREDENTIALS_FILENAME)
}

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

#[cfg(not(target_os = "windows"))]
mod dpapi {
    use super::SecretsError;
    use zeroize::Zeroizing;
    pub fn protect(_: &[u8]) -> Result<Vec<u8>, SecretsError> {
        Err(SecretsError::UnsupportedPlatform)
    }
    pub fn unprotect(_: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
        Err(SecretsError::UnsupportedPlatform)
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
