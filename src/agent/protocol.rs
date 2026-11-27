//! Wire format shared by the resident tray app's agent bridge (named-pipe server)
//! and the `--mcp` stdio server process (pipe client).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::default_app_data_dir;

pub const CONNECTION_FILE: &str = "agent-bridge.json";
pub const PIPE_PREFIX: &str = r"\\.\pipe\restic-tray-agent-";
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;

pub const OPERATIONS: &[&str] = &[
    "get_status",
    "get_config",
    "get_backup_history",
    "list_snapshots",
    "list_snapshot_files",
    "diff_snapshots",
    "get_size_breakdown",
    "run_backup",
    "cancel_backup",
    "init_repository",
    "check_repository",
    "update_storage_config",
    "add_backup_target",
    "remove_backup_target",
    "update_schedule",
    "update_retention_policy",
    "restore_snapshot",
    "forget_prune",
    "delete_r2_bucket",
    "get_project_sync_status",
    "release_project",
    "takeover_project",
];

#[cfg(windows)]
pub const ERROR_PIPE_BUSY: i32 = 231;

#[cfg(windows)]
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

#[cfg(windows)]
pub fn open_pipe(pipe_name: &str) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(pipe_name)
}

pub fn connection_file_path(base_dir: Option<&Path>) -> PathBuf {
    base_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_app_data_dir)
        .join(CONNECTION_FILE)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionInfo {
    pub pipe_name: String,
    pub token: String,
    pub server_token: String,
}

impl ConnectionInfo {
    pub fn generate() -> Self {
        Self {
            pipe_name: format!("{PIPE_PREFIX}{}", random_hex()),
            token: format!("{}{}", random_hex(), random_hex()),
            server_token: format!("{}{}", random_hex(), random_hex()),
        }
    }

    pub fn is_well_formed(&self) -> bool {
        self.pipe_name
            .strip_prefix(PIPE_PREFIX)
            .is_some_and(|suffix| suffix.len() == 32 && is_lower_hex(suffix))
            && [&self.token, &self.server_token]
                .iter()
                .all(|t| t.len() == 64 && is_lower_hex(t))
    }
}

fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Constant-time token comparison.
pub fn token_matches(expected: &str, received: &str) -> bool {
    expected.len() == received.len()
        && expected
            .bytes()
            .zip(received.bytes())
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeRequest {
    pub token: String,
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl BridgeResponse {
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
            server: None,
        }
    }

    pub fn failure(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
            server: None,
        }
    }

    #[must_use]
    pub fn from_server(self, server_token: &str) -> Self {
        Self {
            server: Some(server_token.to_owned()),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_info_is_well_formed_and_rejects_tampering() {
        let info = ConnectionInfo::generate();
        assert!(info.is_well_formed());
        assert!(token_matches(&info.token, &info.token));
        assert!(!token_matches(&info.token, &info.server_token));

        let bad = ConnectionInfo {
            pipe_name: r"\\.\pipe\other".to_owned(),
            ..info
        };
        assert!(!bad.is_well_formed());
    }

    #[test]
    fn default_connection_file_path_resolves_under_app_identifier() {
        let path = connection_file_path(None);
        assert!(
            path.ends_with(Path::new(crate::config::APP_IDENTIFIER).join(CONNECTION_FILE)),
            "unexpected bridge connection path: {}",
            path.display()
        );
    }
}
