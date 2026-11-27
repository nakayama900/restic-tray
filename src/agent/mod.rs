//! AI-agent integration over the Model Context Protocol (MCP).
//!
//! - [`mcp`]: `restic-tray.exe --mcp` stdio JSON-RPC 2.0 server.
//! - [`bridge`]: Windows named-pipe server hosted by the resident tray app.
//! - [`protocol`]: Mutual CSPRNG token authentication and wire format.

pub mod bridge;
pub mod mcp;
pub mod protocol;

use std::path::PathBuf;

/// Entry point for `restic-tray.exe --mcp`.
pub fn run_mcp_stdio(config_override: Option<PathBuf>) -> i32 {
    mcp::run_stdio(config_override)
}
