use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use maya_ipc::{DaemonInfo, IpcClient, JobInfo, JobOptions, RpcEvent};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

/// Shared application state managed by Tauri.
pub struct AppState {
    pub client: Arc<Mutex<Option<IpcClient>>>,
    pub endpoint: String,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            endpoint: maya_ipc::default_ipc_endpoint(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Locate the `mayad` executable across process directory, PATH, target folder, or cargo bin.
fn find_mayad_executable() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
            if candidate.is_file() {
                return candidate;
            }
            // Check relative build targets when debugging
            let target_rel = parent
                .join("../../../target/release")
                .join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
            if target_rel.is_file() {
                return target_rel;
            }
            let target_dbg = parent.join("../../../target/debug").join(if cfg!(windows) {
                "mayad.exe"
            } else {
                "mayad"
            });
            if target_dbg.is_file() {
                return target_dbg;
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        let cargo_bin =
            home.join(".cargo/bin")
                .join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
        if cargo_bin.is_file() {
            return cargo_bin;
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from(if cfg!(windows) { "mayad.exe" } else { "mayad" })
}

/// Attempt to auto-spawn `mayad.exe --background` if not already running.
fn try_spawn_daemon() -> bool {
    let exe = find_mayad_executable();
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--background");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    cmd.spawn().is_ok()
}

/// Retrieve an existing IPC client or establish a new connection to the Maya daemon.
async fn get_or_connect_client(
    client_holder: &Arc<Mutex<Option<IpcClient>>>,
    endpoint: &str,
) -> Result<IpcClient, String> {
    let mut guard = client_holder.lock().await;

    // Check if current cached client can query status
    if let Some(client) = guard.as_ref() {
        if client.daemon_status().await.is_ok() {
            return Ok(client.clone());
        }
        // Client connection stale or terminated
        *guard = None;
    }

    // Try connecting to existing endpoint
    match IpcClient::connect(endpoint).await {
        Ok(client) => {
            *guard = Some(client.clone());
            Ok(client)
        }
        Err(err) => {
            // Daemon is offline or pipe unavailable: try auto-spawning mayad
            if try_spawn_daemon() {
                // Wait briefly for daemon named pipe creation
                tokio::time::sleep(Duration::from_millis(350)).await;
                match IpcClient::connect_with_timeout(endpoint, Duration::from_secs(3)).await {
                    Ok(client) => {
                        *guard = Some(client.clone());
                        return Ok(client);
                    }
                    Err(spawn_err) => {
                        return Err(format!(
                            "Daemon auto-spawned but connection failed: {spawn_err}"
                        ));
                    }
                }
            }
            Err(format!(
                "Maya daemon is offline at `{endpoint}`: {err}. Please start `mayad`."
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri IPC Commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn add_job(
    state: State<'_, AppState>,
    url: String,
    options: serde_json::Value,
) -> Result<JobInfo, String> {
    let job_options: JobOptions = serde_json::from_value(options).unwrap_or_default();
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client
        .add_job(url, job_options)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_jobs(state: State<'_, AppState>) -> Result<Vec<JobInfo>, String> {
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client.list_jobs().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn pause_job(state: State<'_, AppState>, id: String) -> Result<JobInfo, String> {
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client.pause_job(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn resume_job(state: State<'_, AppState>, id: String) -> Result<JobInfo, String> {
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client.resume_job(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn cancel_job(state: State<'_, AppState>, id: String) -> Result<JobInfo, String> {
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client.cancel_job(id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_daemon_status(state: State<'_, AppState>) -> Result<DaemonInfo, String> {
    let client = get_or_connect_client(&state.client, &state.endpoint).await?;
    client.daemon_status().await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Background IPC Listener & Event Relay
// ---------------------------------------------------------------------------

async fn background_event_loop(
    app: AppHandle,
    client_holder: Arc<Mutex<Option<IpcClient>>>,
    endpoint: String,
) {
    loop {
        // Attempt to obtain or subscribe to client
        let maybe_client = {
            let guard = client_holder.lock().await;
            guard.clone()
        };

        let client = match maybe_client {
            Some(c) => c,
            None => match IpcClient::connect(&endpoint).await {
                Ok(c) => {
                    let mut guard = client_holder.lock().await;
                    *guard = Some(c.clone());
                    c
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
            },
        };

        let mut rx = client.subscribe_events();

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(RpcEvent::JobProgress { id, downloaded, total, speed_bps, state }) => {
                            let payload = serde_json::json!({
                                "id": id,
                                "downloaded": downloaded,
                                "total": total,
                                "speed_bps": speed_bps,
                                "state": state,
                            });
                            let _ = app.emit("download-progress", payload);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Missed some progress events; continue receiving
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Channel closed, connection likely dropped
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Periodic heartbeat check
                    if client.daemon_status().await.is_err() {
                        break;
                    }
                }
            }
        }

        // Invalidate cached client and wait before reconnecting
        {
            let mut guard = client_holder.lock().await;
            *guard = None;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

// ---------------------------------------------------------------------------
// Application Entry Point
// ---------------------------------------------------------------------------

pub fn run() {
    let app_state = AppState::new();
    let client_holder = app_state.client.clone();
    let endpoint = app_state.endpoint.clone();

    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            add_job,
            list_jobs,
            pause_job,
            resume_job,
            cancel_job,
            get_daemon_status
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                background_event_loop(app_handle, client_holder, endpoint).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
