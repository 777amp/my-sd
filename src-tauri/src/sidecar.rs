// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Headless Node.js sidecar manager for `@signalapp/libsignal-client` protocol operations.
//!
//! Speaks newline-delimited JSON-RPC 2.0 over `stdin`/`stdout` with low-RAM V8 flags
//! tuned for Raspberry Pi 4B (`--max-old-space-size=256 --optimize-for-size`).
//! As protocol modules are migrated into pure Rust, calls move out of this sidecar
//! without changing the frontend IPC contract.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tracing::{debug, error, info, warn};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcInbound {
    /// Present for responses to Rust -> Sidecar requests
    id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcError>,
    /// Present for asynchronous Sidecar -> UI events (e.g. incoming messages, QR code)
    event: Option<String>,
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
    #[serde(default)]
    code: Option<i64>,
}

pub type EventCallback = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct SidecarManager {
    script_path: PathBuf,
    user_data_dir: PathBuf,
    next_id: AtomicU64,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    child: Arc<AsyncMutex<Option<Child>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    on_event: Arc<Mutex<Option<EventCallback>>>,
}

impl SidecarManager {
    pub fn new(script_path: PathBuf, user_data_dir: PathBuf) -> Self {
        Self {
            script_path,
            user_data_dir,
            next_id: AtomicU64::new(1),
            stdin: Arc::new(AsyncMutex::new(None)),
            child: Arc::new(AsyncMutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            on_event: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_event_callback(&self, cb: EventCallback) {
        *self.on_event.lock() = Some(cb);
    }

    pub async fn is_running(&self) -> bool {
        let mut guard = self.child.lock().await;
        if let Some(ref mut c) = *guard {
            matches!(c.try_wait(), Ok(None))
        } else {
            false
        }
    }

    /// Spawns the headless Node.js sidecar if not already running.
    pub async fn start(&self) -> Result<()> {
        if self.is_running().await {
            return Ok(());
        }

        if !self.script_path.exists() {
            return Err(anyhow!(
                "Sidecar script not found at {}",
                self.script_path.display()
            ));
        }

        let node_bin = std::env::var("SIGNAL_NODE_BIN").unwrap_or_else(|_| "node".into());
        let max_heap = std::env::var("SIGNAL_SIDECAR_MAX_OLD_SPACE_MB")
            .unwrap_or_else(|_| "256".into());

        info!(
            "Starting Signal protocol sidecar: {} {}",
            node_bin,
            self.script_path.display()
        );

        let mut cmd = Command::new(&node_bin);
        cmd.arg(format!("--max-old-space-size={max_heap}"))
            .arg("--optimize-for-size")
            .arg(&self.script_path)
            .env("SIGNAL_USER_DATA_DIR", &self.user_data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn sidecar ({node_bin})"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("No sidecar stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("No sidecar stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("No sidecar stderr"))?;

        *self.stdin.lock().await = Some(stdin);
        *self.child.lock().await = Some(child);

        // Spawn stdout JSON-RPC reader task
        let pending = Arc::clone(&self.pending);
        let on_event = Arc::clone(&self.on_event);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<RpcInbound>(&line) {
                    Ok(msg) => {
                        if let Some(id) = msg.id {
                            if let Some(tx) = pending.lock().remove(&id) {
                                let res = if let Some(err) = msg.error {
                                    Err(anyhow!(
                                        "Sidecar RPC error ({:?}): {}",
                                        err.code,
                                        err.message
                                    ))
                                } else {
                                    Ok(msg.result.unwrap_or(Value::Null))
                                };
                                let _ = tx.send(res);
                            }
                        } else if let Some(event) = msg.event {
                            debug!("Sidecar event: {event}");
                            if let Some(cb) = on_event.lock().as_ref() {
                                cb(&event, msg.payload.unwrap_or(Value::Null));
                            }
                        }
                    }
                    Err(err) => {
                        warn!("Ignoring non-JSON line from sidecar stdout: {err} ({line})");
                    }
                }
            }

            // Fail any in-flight requests if the sidecar exited
            let mut map = pending.lock();
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(anyhow!("Sidecar process exited unexpectedly")));
            }
        });

        // Forward sidecar stderr to tracing
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!("[sidecar] {line}");
            }
        });

        Ok(())
    }

    /// Invokes a JSON-RPC method on the headless Node.js sidecar.
    pub async fn invoke(&self, method: &str, params: Value) -> Result<Value> {
        if !self.is_running().await {
            self.start().await?;
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);

        let req = RpcRequest { id, method, params };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        {
            let mut stdin_guard = self.stdin.lock().await;
            let stdin = stdin_guard
                .as_mut()
                .ok_or_else(|| anyhow!("Sidecar stdin is closed"))?;
            if let Err(err) = stdin.write_all(line.as_bytes()).await {
                self.pending.lock().remove(&id);
                return Err(err.into());
            }
            stdin.flush().await?;
        }

        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(anyhow!("Sidecar response channel dropped")),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(anyhow!("Sidecar RPC '{method}' timed out"))
            }
        }
    }

    /// Gracefully stops the sidecar process.
    pub async fn shutdown(&self) {
        if self.is_running().await {
            let _ = self.invoke("shutdown", json!({})).await;
        }
        *self.stdin.lock().await = None;
        if let Some(mut child) = self.child.lock().await.take() {
            if let Err(err) = child.kill().await {
                error!("Error terminating sidecar: {err}");
            }
        }
    }
}

/// Locates `sidecar/signal-sidecar.mjs` relative to the Tauri resource dir or workspace root.
pub fn resolve_sidecar_script(resource_dir: Option<&Path>) -> PathBuf {
    let candidates = [
        resource_dir.map(|d| d.join("sidecar").join("signal-sidecar.mjs")),
        resource_dir.map(|d| d.join("signal-sidecar.mjs")),
        Some(PathBuf::from("sidecar/signal-sidecar.mjs")),
        Some(PathBuf::from("../sidecar/signal-sidecar.mjs")),
    ];

    for path in candidates.into_iter().flatten() {
        if path.exists() {
            return path;
        }
    }

    PathBuf::from("sidecar/signal-sidecar.mjs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_sidecar_ping_roundtrip() {
        let script = resolve_sidecar_script(None);
        if !script.exists() {
            // Skip if sidecar script has not been created yet
            return;
        }
        let tmp = TempDir::new().unwrap();
        let mgr = SidecarManager::new(script, tmp.path().to_path_buf());
        let pong = mgr.invoke("ping", json!({ "echo": "rpi4" })).await.unwrap();
        assert_eq!(pong["ok"], true);
        assert_eq!(pong["echo"], "rpi4");

        let link = mgr
            .invoke("linking.start", json!({ "offlinePreview": true }))
            .await
            .unwrap();
        assert_eq!(link["started"], true);
        assert!(link["qrUrl"]
            .as_str()
            .unwrap()
            .starts_with("sgnl://linkdevice?uuid="));

        let msg_status = mgr.invoke("messaging.status", json!({})).await.unwrap();
        assert_eq!(msg_status["state"], "disconnected");

        mgr.invoke("linking.cancel", json!({})).await.unwrap();
        mgr.shutdown().await;
    }
}
