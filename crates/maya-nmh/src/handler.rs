//! Message handler dispatching extension commands to `mayad` via `maya-ipc`.

use maya_ipc::{IpcClient, IpcError, JobOptions, RpcRequest};
use serde_json::{json, Value};
use std::time::Duration;

/// Dispatch an incoming extension message and generate a structured JSON response.
pub async fn handle_message(msg: &Value) -> Value {
    match dispatch_inner(msg).await {
        Ok(data) => json!({
            "status": "ok",
            "success": true,
            "data": data,
        }),
        Err(err) => json!({
            "status": "error",
            "success": false,
            "error": err,
        }),
    }
}

async fn get_client() -> Result<IpcClient, String> {
    match IpcClient::connect_with_timeout(&maya_ipc::default_ipc_endpoint(), Duration::from_millis(500)).await {
        Ok(client) => Ok(client),
        Err(IpcError::Io(err)) => Err(format!(
            "Maya daemon is not running (I/O error: {err}). Please start mayad or run 'maya daemon start'."
        )),
        Err(IpcError::Timeout) => Err(
            "Timed out connecting to Maya daemon. Please ensure mayad is running.".to_string(),
        ),
        Err(err) => Err(format!("IPC connection error: {err}")),
    }
}

async fn dispatch_inner(msg: &Value) -> Result<Value, String> {
    // If the message is a raw JSON-RPC request with `method` and `params`
    if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
        if let Ok(mut rpc_req) = serde_json::from_value::<RpcRequest>(msg.clone()) {
            if let RpcRequest::JobAdd {
                ref mut options, ..
            } = rpc_req
            {
                // Security (MAYA-SEC-03): Disallow targeting private or loopback IP ranges from browser NMH
                options.allow_private_ips = Some(false);
            }
            let client = get_client().await?;
            let res = client.call(rpc_req).await.map_err(|e| e.to_string())?;
            return Ok(res);
        }

        // Handle method string directly if deserialization wasn't exact
        return dispatch_by_action(method, msg).await;
    }

    // Otherwise check for `action`, `command`, or `type`
    let action = msg
        .get("action")
        .or_else(|| msg.get("command"))
        .or_else(|| msg.get("type"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing required 'action' or 'method' field in message".to_string())?;

    dispatch_by_action(action, msg).await
}

pub(crate) fn extract_job_options(msg: &Value) -> JobOptions {
    let mut headers: Vec<(String, String)> = Vec::new();

    // Extract explicit headers array: [["Header-Name", "value"], ...]
    if let Some(arr) = msg.get("headers").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(pair) = item.as_array() {
                if pair.len() == 2 {
                    if let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str()) {
                        headers.push((k.to_string(), v.to_string()));
                    }
                }
            } else if let Some(map) = item.as_object() {
                for (k, v) in map {
                    if let Some(val_str) = v.as_str() {
                        headers.push((k.to_string(), val_str.to_string()));
                    }
                }
            }
        }
    } else if let Some(map) = msg.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(val_str) = v.as_str() {
                headers.push((k.to_string(), val_str.to_string()));
            }
        }
    }

    // Extract referrer if provided
    if let Some(referrer) = msg.get("referrer").and_then(|v| v.as_str()) {
        if !referrer.trim().is_empty()
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("Referer"))
        {
            headers.push(("Referer".to_string(), referrer.to_string()));
        }
    }

    // Extract cookie/cookies if provided
    if let Some(cookies) = msg
        .get("cookies")
        .or_else(|| msg.get("cookie"))
        .and_then(|v| v.as_str())
    {
        if !cookies.trim().is_empty()
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("Cookie"))
        {
            headers.push(("Cookie".to_string(), cookies.to_string()));
        }
    }

    let filename = msg
        .get("filename")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let output_dir = msg
        .get("output_dir")
        .or_else(|| msg.get("dest"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let connections = msg
        .get("connections")
        .and_then(|v| v.as_u64())
        .map(|c| c as usize);

    let max_bytes_per_sec = msg
        .get("max_bytes_per_sec")
        .or_else(|| msg.get("speed_limit"))
        .and_then(|v| v.as_u64());

    let checksum = msg
        .get("checksum")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let overwrite = msg.get("overwrite").and_then(|v| v.as_bool());
    let resume = msg.get("resume").and_then(|v| v.as_bool());

    // Security (MAYA-SEC-03): Disallow targeting private or loopback IP ranges from browser NMH messages
    let allow_private_ips = Some(false);

    JobOptions {
        output_dir,
        filename,
        connections,
        max_bytes_per_sec,
        headers,
        checksum,
        overwrite,
        resume,
        allow_private_ips,
    }
}

async fn dispatch_by_action(action: &str, msg: &Value) -> Result<Value, String> {
    match action.to_ascii_lowercase().as_str() {
        "ping" => {
            let daemon_status = match get_client().await {
                Ok(client) => match client.daemon_status().await {
                    Ok(info) => json!({
                        "daemon_connected": true,
                        "daemon": info,
                    }),
                    Err(e) => json!({
                        "daemon_connected": false,
                        "error": e.to_string(),
                    }),
                },
                Err(e) => json!({
                    "daemon_connected": false,
                    "error": e,
                }),
            };

            Ok(json!({
                "service": "maya-nmh",
                "version": env!("CARGO_PKG_VERSION"),
                "status": "ready",
                "daemon": daemon_status,
            }))
        }

        "download" | "add" | "job_add" | "jobadd" => {
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required 'url' parameter for download".to_string())?;

            let options = extract_job_options(msg);

            let client = get_client().await?;
            let info = client
                .add_job(url, options)
                .await
                .map_err(|e| e.to_string())?;

            Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
        }

        "list" | "job_list" | "joblist" | "jobs" => {
            let client = get_client().await?;
            let jobs = client.list_jobs().await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(jobs).map_err(|e| e.to_string())?)
        }

        "status" | "job_status" | "jobstatus" => {
            let client = get_client().await?;
            if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                let info = client.job_status(id).await.map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
            } else {
                let daemon = client.daemon_status().await.map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(daemon).map_err(|e| e.to_string())?)
            }
        }

        "daemon_status" | "daemonstatus" => {
            let client = get_client().await?;
            let daemon = client.daemon_status().await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(daemon).map_err(|e| e.to_string())?)
        }

        "pause" | "job_pause" | "jobpause" => {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required 'id' parameter for pause".to_string())?;

            let client = get_client().await?;
            let info = client.pause_job(id).await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
        }

        "resume" | "job_resume" | "jobresume" => {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required 'id' parameter for resume".to_string())?;

            let client = get_client().await?;
            let info = client.resume_job(id).await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
        }

        "cancel" | "job_cancel" | "jobcancel" => {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required 'id' parameter for cancel".to_string())?;

            let client = get_client().await?;
            let info = client.cancel_job(id).await.map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(info).map_err(|e| e.to_string())?)
        }

        other => Err(format!("Unknown or unsupported action '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ping_message() {
        let msg = json!({ "action": "ping" });
        let resp = handle_message(&msg).await;
        assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(true));
        assert!(resp.get("data").is_some());
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let msg = json!({ "action": "nonexistent_action_xyz" });
        let resp = handle_message(&msg).await;
        assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(false));
    }

    #[tokio::test]
    async fn test_download_missing_url() {
        let msg = json!({ "action": "download" });
        let resp = handle_message(&msg).await;
        assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn test_extract_job_options_disallows_private_ips() {
        // Explicitly trying to enable private IPs must be overridden to Some(false)
        let msg_with_private_ips = json!({
            "url": "http://192.168.1.1/firmware.bin",
            "allow_private_ips": true
        });
        let options = extract_job_options(&msg_with_private_ips);
        assert_eq!(options.allow_private_ips, Some(false));

        // When omitted, must also be Some(false) for browser security
        let msg_default = json!({
            "url": "https://example.com/file.zip"
        });
        let options_default = extract_job_options(&msg_default);
        assert_eq!(options_default.allow_private_ips, Some(false));
    }
}
