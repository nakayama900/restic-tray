//! Resident app side of the AI-agent named-pipe bridge.
//!
//! Listens on `\\.\pipe\restic-tray-agent-<hex>`, authenticates requests using
//! mutual CSPRNG tokens from `agent-bridge.json`, and executes operations against
//! [`SharedController`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::agent::protocol::{
    BridgeRequest, BridgeResponse, ConnectionInfo, MAX_REQUEST_BYTES, OPERATIONS,
    connection_file_path, token_matches,
};
use crate::controller::SharedController;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the background named-pipe server thread for the resident tray app.
#[cfg(windows)]
pub fn spawn_agent_bridge_thread(controller: Arc<SharedController>) {
    std::thread::Builder::new()
        .name("restic-tray-agent-bridge".to_owned())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for agent bridge");

            rt.block_on(async move {
                if let Err(err) = serve_bridge_loop(controller).await {
                    tracing::warn!("agent bridge stopped: {err}");
                }
            });
        })
        .expect("failed to spawn agent bridge thread");
}

#[cfg(not(windows))]
pub fn spawn_agent_bridge_thread(_controller: Arc<SharedController>) {}

#[cfg(windows)]
async fn serve_bridge_loop(controller: Arc<SharedController>) -> std::io::Result<()> {
    let conn_path = connection_file_path(Some(&controller.base_dir));
    if let Some(parent) = conn_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let info = ConnectionInfo::generate();
    let mut server = create_pipe_instance(&info.pipe_name, true)?;
    write_connection_file(&conn_path, &info)?;
    tracing::info!(pipe = %info.pipe_name, "MCP named-pipe bridge listening");

    loop {
        if controller.is_quit_requested() {
            break;
        }
        if server.connect().await.is_err() {
            server = create_pipe_instance(&info.pipe_name, false)?;
            continue;
        }

        let connected = server;
        server = match create_pipe_instance(&info.pipe_name, false) {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!("could not create next pipe instance: {err}");
                break;
            }
        };

        let ctrl = Arc::clone(&controller);
        let conn_info = info.clone();
        tokio::spawn(async move {
            handle_client(connected, ctrl, conn_info).await;
        });
    }

    let _ = std::fs::remove_file(&conn_path);
    Ok(())
}

#[cfg(windows)]
fn create_pipe_instance(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first)
        .reject_remote_clients(true)
        .in_buffer_size(64 * 1024)
        .out_buffer_size(64 * 1024);
    opts.create(name)
}

#[cfg(windows)]
fn write_connection_file(path: &Path, info: &ConnectionInfo) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(info)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(windows)]
async fn handle_client(
    pipe: NamedPipeServer,
    controller: Arc<SharedController>,
    info: ConnectionInfo,
) {
    let (reader, mut writer) = tokio::io::split(pipe);
    let mut lines = BufReader::new(reader);
    let mut authenticated = false;

    loop {
        let mut buf = Vec::new();
        let read_fut = async {
            let mut limited = (&mut lines).take((MAX_REQUEST_BYTES + 1) as u64);
            limited.read_until(b'\n', &mut buf).await
        };

        let read_res = if authenticated {
            read_fut.await
        } else {
            match tokio::time::timeout(AUTH_TIMEOUT, read_fut).await {
                Ok(r) => r,
                Err(_) => return,
            }
        };

        match read_res {
            Ok(0) | Err(_) => return,
            Ok(_) if buf.len() > MAX_REQUEST_BYTES => {
                let _ = send_line(
                    &mut writer,
                    &BridgeResponse::failure(0, "request too large"),
                )
                .await;
                return;
            }
            Ok(_) => {}
        }

        let req: BridgeRequest = match serde_json::from_slice(&buf) {
            Ok(r) => r,
            Err(err) => {
                let _ = send_line(
                    &mut writer,
                    &BridgeResponse::failure(0, format!("invalid JSON request: {err}")),
                )
                .await;
                return;
            }
        };

        if !token_matches(&info.token, &req.token) {
            let _ = send_line(
                &mut writer,
                &BridgeResponse::failure(req.id, "unauthorized bridge token"),
            )
            .await;
            return;
        }
        authenticated = true;

        if !controller.get_config().agent.enabled {
            let resp = BridgeResponse::failure(
                req.id,
                "MCP agent integration is disabled in settings (agent.enabled = false)",
            )
            .from_server(&info.server_token);
            let _ = send_line(&mut writer, &resp).await;
            continue;
        }

        if !OPERATIONS.contains(&req.op.as_str()) {
            let resp = BridgeResponse::failure(req.id, format!("unknown operation: {}", req.op))
                .from_server(&info.server_token);
            let _ = send_line(&mut writer, &resp).await;
            continue;
        }

        let ctrl = Arc::clone(&controller);
        let op = req.op.clone();
        let args = req.args.clone();
        let exec_res =
            tokio::task::spawn_blocking(move || ctrl.handle_operation("mcp", &op, &args)).await;

        let resp = match exec_res {
            Ok(Ok(val)) => BridgeResponse::success(req.id, val).from_server(&info.server_token),
            Ok(Err(err)) => {
                BridgeResponse::failure(req.id, err.to_string()).from_server(&info.server_token)
            }
            Err(join_err) => {
                BridgeResponse::failure(req.id, format!("worker panicked: {join_err}"))
                    .from_server(&info.server_token)
            }
        };

        if send_line(&mut writer, &resp).await.is_err() {
            return;
        }
    }
}

#[cfg(windows)]
async fn send_line<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &BridgeResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(resp)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}
