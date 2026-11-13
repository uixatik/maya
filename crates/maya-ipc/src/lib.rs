//! Maya IPC protocol definitions, JSON-RPC framing, and cross-platform client/server transports.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{
    split, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    ReadHalf, WriteHalf,
};
use tokio::sync::{broadcast, oneshot, watch, Mutex};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

/// IPC protocol version.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Default Windows named pipe path for the Maya daemon.
pub const DEFAULT_WINDOWS_PIPE_NAME: &str = r"\\.\pipe\maya-daemon";

/// Default Unix domain socket filename for the Maya daemon.
pub const DEFAULT_UNIX_SOCKET_NAME: &str = "maya.sock";

/// Default fallback Unix domain socket path when neither `XDG_RUNTIME_DIR` nor `HOME` is set.
pub const DEFAULT_UNIX_SOCKET_PATH: &str = "/tmp/maya-root/maya.sock";

/// Maximum allowed size for a single incoming IPC message frame (1 MiB).
pub const MAX_IPC_MESSAGE_SIZE: usize = 1024 * 1024;

fn default_jsonrpc() -> String {
    JSONRPC_VERSION.to_string()
}

/// Errors produced by IPC framing, transport, or remote daemon responses.
#[derive(Debug, Error)]
pub enum IpcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("daemon returned error: {0}")]
    Remote(String),

    #[error("IPC connection closed")]
    Closed,

    #[error("timed out waiting for IPC connection or response")]
    Timeout,
}

/// Unified result alias for `maya-ipc`.
pub type Result<T> = std::result::Result<T, IpcError>;

/// Resolve the Unix domain socket path from explicit `XDG_RUNTIME_DIR` and `HOME` values.
pub fn resolve_unix_socket_path_from(xdg_runtime_dir: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(runtime_dir) = xdg_runtime_dir.filter(|s| !s.trim().is_empty()) {
        return Path::new(runtime_dir).join(DEFAULT_UNIX_SOCKET_NAME);
    }
    if let Some(home_dir) = home.filter(|s| !s.trim().is_empty()) {
        return Path::new(home_dir)
            .join(".maya")
            .join(DEFAULT_UNIX_SOCKET_NAME);
    }
    PathBuf::from(DEFAULT_UNIX_SOCKET_PATH)
}

/// Return the default cross-platform IPC endpoint for the Maya daemon.
///
/// - Windows: `\\.\pipe\maya-daemon`
/// - Unix: `$XDG_RUNTIME_DIR/maya.sock` or `~/.maya/maya.sock` (falling back to `/tmp/maya-root/maya.sock`)
pub fn default_ipc_endpoint() -> String {
    #[cfg(windows)]
    {
        DEFAULT_WINDOWS_PIPE_NAME.to_string()
    }
    #[cfg(unix)]
    {
        let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let home = std::env::var("HOME").ok();
        resolve_unix_socket_path_from(xdg.as_deref(), home.as_deref())
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(any(windows, unix)))]
    {
        DEFAULT_UNIX_SOCKET_PATH.to_string()
    }
}

/// Lifecycle state of a managed download job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    /// Return `true` if the job has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Return the canonical lowercase snake_case string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = IpcError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "queued" => Ok(Self::Queued),
            "downloading" => Ok(Self::Downloading),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            other => Err(IpcError::Protocol(format!("unknown job state `{other}`"))),
        }
    }
}

/// Optional configuration parameters passed with [`RpcRequest::JobAdd`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobOptions {
    pub output_dir: Option<String>,
    pub filename: Option<String>,
    pub connections: Option<usize>,
    pub max_bytes_per_sec: Option<u64>,
    pub headers: Vec<(String, String)>,
    pub checksum: Option<String>,
    pub overwrite: Option<bool>,
    pub resume: Option<bool>,
    pub allow_private_ips: Option<bool>,
}

/// Snapshot of a queued, active, or finished download job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub url: String,
    pub dest: String,
    pub status: JobState,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Daemon health and queue summary returned by [`RpcRequest::DaemonStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub active_jobs: usize,
    pub queued_jobs: usize,
    pub total_jobs: usize,
}

/// JSON-RPC request methods supported by `mayad`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum RpcRequest {
    JobAdd {
        url: String,
        #[serde(default)]
        options: JobOptions,
    },
    JobPause {
        id: String,
    },
    JobResume {
        id: String,
    },
    JobCancel {
        id: String,
    },
    JobList,
    JobStatus {
        id: String,
    },
    DaemonStatus,
    DaemonShutdown,
}

/// Wire envelope for a JSON-RPC request sent from client to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub id: String,
    #[serde(flatten)]
    pub request: RpcRequest,
}

impl IpcRequest {
    /// Wrap a [`RpcRequest`] in a JSON-RPC envelope with a fresh UUID v4 correlation id.
    pub fn new(request: RpcRequest) -> Self {
        Self {
            jsonrpc: default_jsonrpc(),
            id: Uuid::new_v4().to_string(),
            request,
        }
    }
}

/// JSON-RPC response payload returned by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcResponse {
    Ok(serde_json::Value),
    Err(String),
}

impl RpcResponse {
    /// Construct an `Ok` response from a serializable value.
    pub fn ok<T: Serialize>(value: T) -> Result<Self> {
        Ok(Self::Ok(serde_json::to_value(value)?))
    }

    /// Construct an `Err` response from an error message.
    pub fn err(message: impl Into<String>) -> Self {
        Self::Err(message.into())
    }

    /// Convert this response into a typed `Result<T>`.
    pub fn into_result<T: DeserializeOwned>(self) -> Result<T> {
        match self {
            Self::Ok(val) => Ok(serde_json::from_value(val)?),
            Self::Err(msg) => Err(IpcError::Remote(msg)),
        }
    }
}

/// Wire envelope for a JSON-RPC response paired with its request correlation `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcResponse {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub id: String,
    pub result: RpcResponse,
}

impl IpcResponse {
    /// Construct a response envelope for the given request `id`.
    pub fn new(id: impl Into<String>, result: RpcResponse) -> Self {
        Self {
            jsonrpc: default_jsonrpc(),
            id: id.into(),
            result,
        }
    }
}

/// Real-time server notification events streamed to connected clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "params")]
pub enum RpcEvent {
    JobProgress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: u64,
        state: JobState,
    },
}

/// Tagged server-to-client message frame carrying either a response or a pushed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Response(IpcResponse),
    Event(RpcEvent),
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

type BoxedStream = Box<dyn AsyncStream>;

async fn connect_raw(endpoint: &str, timeout: Option<Duration>) -> Result<BoxedStream> {
    let deadline = timeout.map(|t| Instant::now() + t);

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        const ERROR_PIPE_BUSY: i32 = 231;

        let mut busy_attempts = 0u32;
        loop {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => return Ok(Box::new(client)),
                Err(err) => {
                    let is_busy = err.raw_os_error() == Some(ERROR_PIPE_BUSY);
                    let is_not_found = err.kind() == std::io::ErrorKind::NotFound;

                    if let Some(limit) = deadline {
                        if Instant::now() >= limit {
                            return Err(if is_busy || is_not_found {
                                IpcError::Timeout
                            } else {
                                IpcError::Io(err)
                            });
                        }
                        if is_busy || is_not_found {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                        return Err(IpcError::Io(err));
                    }

                    if is_busy && busy_attempts < 100 {
                        busy_attempts += 1;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                    return Err(IpcError::Io(err));
                }
            }
        }
    }

    #[cfg(unix)]
    {
        use tokio::net::UnixStream;

        loop {
            match UnixStream::connect(endpoint).await {
                Ok(stream) => return Ok(Box::new(stream)),
                Err(err) => {
                    let retryable = matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    );
                    if let Some(limit) = deadline {
                        if Instant::now() >= limit {
                            return Err(if retryable {
                                IpcError::Timeout
                            } else {
                                IpcError::Io(err)
                            });
                        }
                        if retryable {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                    }
                    return Err(IpcError::Io(err));
                }
            }
        }
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = (endpoint, deadline);
        Err(IpcError::Protocol(
            "IPC transport is only supported on Windows and Unix".to_string(),
        ))
    }
}

struct IpcClientInner {
    writer: Mutex<WriteHalf<BoxedStream>>,
    pending: Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>,
    events: broadcast::Sender<RpcEvent>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for IpcClientInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.reader_task.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

/// Typed asynchronous IPC client supporting RPC requests and streaming [`RpcEvent`] notifications.
#[derive(Clone)]
pub struct IpcClient {
    inner: Arc<IpcClientInner>,
}

impl IpcClient {
    /// Connect to the default platform IPC endpoint.
    pub async fn connect_default() -> Result<Self> {
        Self::connect(&default_ipc_endpoint()).await
    }

    /// Connect to an explicit IPC endpoint (named pipe on Windows, socket path on Unix).
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let stream = connect_raw(endpoint, None).await?;
        Ok(Self::from_stream(stream))
    }

    /// Connect to an IPC endpoint, retrying `NotFound` / busy errors up to `timeout`.
    pub async fn connect_with_timeout(endpoint: &str, timeout: Duration) -> Result<Self> {
        let stream = connect_raw(endpoint, Some(timeout)).await?;
        Ok(Self::from_stream(stream))
    }

    fn from_stream(stream: BoxedStream) -> Self {
        let (read_half, write_half) = split(stream);
        let (events_tx, _) = broadcast::channel(256);

        let inner = Arc::new(IpcClientInner {
            writer: Mutex::new(write_half),
            pending: Mutex::new(HashMap::new()),
            events: events_tx,
            reader_task: Mutex::new(None),
        });

        let reader_inner = Arc::downgrade(&inner);
        let handle = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut reader)
                    .take(MAX_IPC_MESSAGE_SIZE as u64 + 1)
                    .read_line(&mut line)
                    .await
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line.len() > MAX_IPC_MESSAGE_SIZE
                            || (line.len() >= MAX_IPC_MESSAGE_SIZE && !line.ends_with('\n'))
                        {
                            break;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(msg) = serde_json::from_str::<ServerMessage>(trimmed) else {
                            continue;
                        };
                        let Some(strong) = reader_inner.upgrade() else {
                            break;
                        };
                        match msg {
                            ServerMessage::Response(resp) => {
                                let tx = strong.pending.lock().await.remove(&resp.id);
                                if let Some(tx) = tx {
                                    let _ = tx.send(resp.result);
                                }
                            }
                            ServerMessage::Event(ev) => {
                                let _ = strong.events.send(ev);
                            }
                        }
                    }
                }
            }

            if let Some(strong) = reader_inner.upgrade() {
                let mut pending = strong.pending.lock().await;
                pending.clear();
            }
        });

        if let Ok(mut guard) = inner.reader_task.try_lock() {
            *guard = Some(handle);
        }

        Self { inner }
    }

    /// Subscribe to real-time [`RpcEvent`] notifications pushed by the daemon.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RpcEvent> {
        self.inner.events.subscribe()
    }

    /// Send a raw [`RpcRequest`] and await its [`RpcResponse`].
    pub async fn request(&self, request: RpcRequest) -> Result<RpcResponse> {
        let envelope = IpcRequest::new(request);
        let id = envelope.id.clone();
        let mut payload = serde_json::to_vec(&envelope)?;
        payload.push(b'\n');

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.clone(), tx);

        {
            let mut writer = self.inner.writer.lock().await;
            if let Err(err) = writer.write_all(&payload).await {
                self.inner.pending.lock().await.remove(&id);
                return Err(IpcError::Io(err));
            }
            if let Err(err) = writer.flush().await {
                self.inner.pending.lock().await.remove(&id);
                return Err(IpcError::Io(err));
            }
        }

        rx.await.map_err(|_| IpcError::Closed)
    }

    /// Send a [`RpcRequest`] and unwrap `RpcResponse::Ok(Value)` or fail on `RpcResponse::Err`.
    pub async fn call(&self, request: RpcRequest) -> Result<serde_json::Value> {
        match self.request(request).await? {
            RpcResponse::Ok(val) => Ok(val),
            RpcResponse::Err(msg) => Err(IpcError::Remote(msg)),
        }
    }

    /// Add a download job to the daemon queue.
    pub async fn add_job(&self, url: impl Into<String>, options: JobOptions) -> Result<JobInfo> {
        let resp = self
            .request(RpcRequest::JobAdd {
                url: url.into(),
                options,
            })
            .await?;
        resp.into_result()
    }

    /// Pause an active or queued download job.
    pub async fn pause_job(&self, id: impl Into<String>) -> Result<JobInfo> {
        let resp = self.request(RpcRequest::JobPause { id: id.into() }).await?;
        resp.into_result()
    }

    /// Resume a paused or failed download job.
    pub async fn resume_job(&self, id: impl Into<String>) -> Result<JobInfo> {
        let resp = self
            .request(RpcRequest::JobResume { id: id.into() })
            .await?;
        resp.into_result()
    }

    /// Cancel a download job.
    pub async fn cancel_job(&self, id: impl Into<String>) -> Result<JobInfo> {
        let resp = self
            .request(RpcRequest::JobCancel { id: id.into() })
            .await?;
        resp.into_result()
    }

    /// List all jobs tracked by the daemon.
    pub async fn list_jobs(&self) -> Result<Vec<JobInfo>> {
        let resp = self.request(RpcRequest::JobList).await?;
        resp.into_result()
    }

    /// Query status for a single job by `id`.
    pub async fn job_status(&self, id: impl Into<String>) -> Result<JobInfo> {
        let resp = self
            .request(RpcRequest::JobStatus { id: id.into() })
            .await?;
        resp.into_result()
    }

    /// Query daemon status and queue summary.
    pub async fn daemon_status(&self) -> Result<DaemonInfo> {
        let resp = self.request(RpcRequest::DaemonStatus).await?;
        resp.into_result()
    }

    /// Request graceful daemon shutdown.
    pub async fn daemon_shutdown(&self) -> Result<()> {
        let _ = self.call(RpcRequest::DaemonShutdown).await?;
        Ok(())
    }
}

/// Cloneable writer handle for sending responses and streaming events on an accepted connection.
#[derive(Clone)]
pub struct IpcConnectionSender {
    writer: Arc<Mutex<WriteHalf<BoxedStream>>>,
}

impl IpcConnectionSender {
    /// Send a [`ServerMessage`] frame to the connected client.
    pub async fn send_message(&self, message: &ServerMessage) -> Result<()> {
        let mut payload = serde_json::to_vec(message)?;
        payload.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&payload).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Send a JSON-RPC response correlated to `id`.
    pub async fn send_response(&self, id: impl Into<String>, response: RpcResponse) -> Result<()> {
        self.send_message(&ServerMessage::Response(IpcResponse::new(id, response)))
            .await
    }

    /// Stream an asynchronous [`RpcEvent`] notification to the connected client.
    pub async fn send_event(&self, event: RpcEvent) -> Result<()> {
        self.send_message(&ServerMessage::Event(event)).await
    }
}

/// Accepted bidirectional client connection on an [`IpcServer`].
pub struct IpcConnection {
    reader: BufReader<ReadHalf<BoxedStream>>,
    sender: IpcConnectionSender,
}

impl IpcConnection {
    fn new(stream: BoxedStream) -> Self {
        let (read_half, write_half) = split(stream);
        Self {
            reader: BufReader::new(read_half),
            sender: IpcConnectionSender {
                writer: Arc::new(Mutex::new(write_half)),
            },
        }
    }

    /// Return a cloneable handle for sending responses and events concurrently.
    pub fn sender(&self) -> IpcConnectionSender {
        self.sender.clone()
    }

    /// Receive the next [`IpcRequest`] from the client, or `Ok(None)` on EOF.
    pub async fn recv_request(&mut self) -> Result<Option<IpcRequest>> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = (&mut self.reader)
                .take(MAX_IPC_MESSAGE_SIZE as u64 + 1)
                .read_line(&mut line)
                .await?;
            if read == 0 {
                return Ok(None);
            }
            if line.len() > MAX_IPC_MESSAGE_SIZE
                || (line.len() >= MAX_IPC_MESSAGE_SIZE && !line.ends_with('\n'))
            {
                return Err(IpcError::Protocol(
                    "IPC message frame exceeded 1 MiB limit".to_string(),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let req = serde_json::from_str::<IpcRequest>(trimmed)?;
            return Ok(Some(req));
        }
    }

    /// Send a JSON-RPC response for `id`.
    pub async fn send_response(&self, id: impl Into<String>, response: RpcResponse) -> Result<()> {
        self.sender.send_response(id, response).await
    }

    /// Send an [`RpcEvent`] notification to this client.
    pub async fn send_event(&self, event: RpcEvent) -> Result<()> {
        self.sender.send_event(event).await
    }
}

/// Cross-platform IPC server (Windows Named Pipe or Unix Domain Socket).
pub struct IpcServer {
    endpoint: String,
    #[cfg(windows)]
    listener: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
}

impl IpcServer {
    /// Bind to the default platform IPC endpoint.
    pub async fn bind_default() -> Result<Self> {
        Self::bind(default_ipc_endpoint()).await
    }

    /// Bind an IPC server to `endpoint`.
    ///
    /// On Windows, creates the initial pipe instance with `first_pipe_instance(true)`.
    /// On Unix, cleans up stale non-listening socket files before binding.
    pub async fn bind(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            let listener = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&endpoint)?;
            Ok(Self { endpoint, listener })
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use tokio::net::{UnixListener, UnixStream};

            let path = Path::new(&endpoint);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await?;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            if path.exists() {
                if UnixStream::connect(path).await.is_ok() {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("IPC endpoint already in use: {endpoint}"),
                    )));
                }
                let _ = tokio::fs::remove_file(path).await;
            }
            let listener = UnixListener::bind(path)?;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            Ok(Self { endpoint, listener })
        }

        #[cfg(not(any(windows, unix)))]
        {
            let _ = endpoint;
            Err(IpcError::Protocol(
                "IPC transport is only supported on Windows and Unix".to_string(),
            ))
        }
    }

    /// Return the endpoint string this server is bound to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Accept the next incoming client connection.
    pub async fn accept(&mut self) -> Result<IpcConnection> {
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            self.listener.connect().await?;
            let next = ServerOptions::new().create(&self.endpoint)?;
            let connected = std::mem::replace(&mut self.listener, next);
            Ok(IpcConnection::new(Box::new(connected)))
        }

        #[cfg(unix)]
        {
            let (stream, _) = self.listener.accept().await?;
            Ok(IpcConnection::new(Box::new(stream)))
        }

        #[cfg(not(any(windows, unix)))]
        {
            Err(IpcError::Protocol(
                "IPC transport is only supported on Windows and Unix".to_string(),
            ))
        }
    }

    /// Serve incoming connections concurrently, dispatching RPC requests and streaming events
    /// until `shutdown` becomes `true`.
    pub async fn serve<F, Fut>(
        mut self,
        events: broadcast::Sender<RpcEvent>,
        mut shutdown: watch::Receiver<bool>,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(RpcRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = RpcResponse> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let mut tasks = JoinSet::new();

        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = wait_for_true(&mut shutdown) => {
                    break;
                }
                accepted = self.accept() => {
                    let conn = accepted?;
                    let handler = Arc::clone(&handler);
                    let events_rx = events.subscribe();
                    let conn_shutdown = shutdown.clone();
                    tasks.spawn(async move {
                        handle_connection(conn, events_rx, conn_shutdown, handler).await;
                    });
                }
            }
        }

        let drain_deadline = tokio::time::sleep(Duration::from_millis(250));
        tokio::pin!(drain_deadline);
        loop {
            tokio::select! {
                _ = &mut drain_deadline => break,
                joined = tasks.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
            }
        }
        tasks.abort_all();
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

async fn handle_connection<F, Fut>(
    mut conn: IpcConnection,
    mut events_rx: broadcast::Receiver<RpcEvent>,
    mut shutdown: watch::Receiver<bool>,
    handler: Arc<F>,
) where
    F: Fn(RpcRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = RpcResponse> + Send + 'static,
{
    let sender = conn.sender();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = wait_for_true(&mut shutdown) => {
                break;
            }
            req = conn.recv_request() => {
                match req {
                    Ok(Some(ipc_req)) => {
                        let req_id = ipc_req.id;
                        let is_shutdown = matches!(ipc_req.request, RpcRequest::DaemonShutdown);
                        let response = handler(ipc_req.request).await;
                        if sender.send_response(req_id, response).await.is_err() {
                            break;
                        }
                        if is_shutdown {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            ev = events_rx.recv() => {
                match ev {
                    Ok(event) => {
                        if sender.send_event(event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn wait_for_true(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_endpoint(tag: &str) -> String {
        let unique = Uuid::new_v4();
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\maya-ipc-test-{tag}-{unique}")
        }
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(format!("maya-ipc-test-{tag}-{unique}.sock"))
                .to_string_lossy()
                .into_owned()
        }
    }

    #[test]
    fn resolves_unix_socket_with_xdg_and_home_fallbacks() {
        assert_eq!(
            resolve_unix_socket_path_from(Some("/run/user/1000"), Some("/home/alice")),
            PathBuf::from("/run/user/1000").join("maya.sock")
        );
        assert_eq!(
            resolve_unix_socket_path_from(None, Some("/home/alice")),
            PathBuf::from("/home/alice").join(".maya").join("maya.sock")
        );
        assert_eq!(
            resolve_unix_socket_path_from(Some("  "), None),
            PathBuf::from("/tmp/maya-root/maya.sock")
        );
    }

    #[test]
    fn default_endpoint_matches_platform() {
        let ep = default_ipc_endpoint();
        #[cfg(windows)]
        assert_eq!(ep, DEFAULT_WINDOWS_PIPE_NAME);
        #[cfg(unix)]
        assert!(ep.ends_with("maya.sock"));
    }

    #[test]
    fn jsonrpc_requests_responses_and_events_roundtrip() {
        let requests = vec![
            RpcRequest::JobAdd {
                url: "https://example.com/archive.tar.gz".to_string(),
                options: JobOptions {
                    output_dir: Some("/downloads".to_string()),
                    filename: Some("archive.tar.gz".to_string()),
                    connections: Some(8),
                    max_bytes_per_sec: Some(1_000_000),
                    headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
                    checksum: Some("sha256:abcd".to_string()),
                    overwrite: Some(true),
                    resume: Some(true),
                    allow_private_ips: Some(false),
                },
            },
            RpcRequest::JobPause {
                id: "job-1".to_string(),
            },
            RpcRequest::JobResume {
                id: "job-1".to_string(),
            },
            RpcRequest::JobCancel {
                id: "job-1".to_string(),
            },
            RpcRequest::JobList,
            RpcRequest::JobStatus {
                id: "job-1".to_string(),
            },
            RpcRequest::DaemonStatus,
            RpcRequest::DaemonShutdown,
        ];

        for req in requests {
            let env = IpcRequest::new(req.clone());
            let raw = serde_json::to_string(&env).expect("serialize request");
            let decoded: IpcRequest = serde_json::from_str(&raw).expect("deserialize request");
            assert_eq!(decoded, env);
            assert_eq!(decoded.request, req);
        }

        let ok_msg = ServerMessage::Response(IpcResponse::new(
            "req-1",
            RpcResponse::ok(serde_json::json!({ "ok": true })).expect("wrap ok"),
        ));
        let raw_ok = serde_json::to_string(&ok_msg).expect("serialize ok");
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&raw_ok).expect("deserialize ok"),
            ok_msg
        );

        let err_msg =
            ServerMessage::Response(IpcResponse::new("req-2", RpcResponse::err("not found")));
        let raw_err = serde_json::to_string(&err_msg).expect("serialize err");
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&raw_err).expect("deserialize err"),
            err_msg
        );

        let event_msg = ServerMessage::Event(RpcEvent::JobProgress {
            id: "job-1".to_string(),
            downloaded: 512,
            total: Some(1024),
            speed_bps: 2048,
            state: JobState::Downloading,
        });
        let raw_event = serde_json::to_string(&event_msg).expect("serialize event");
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&raw_event).expect("deserialize event"),
            event_msg
        );
    }

    #[tokio::test]
    async fn ipc_client_server_handshake_and_event_streaming() {
        let endpoint = test_endpoint("handshake");
        let server = IpcServer::bind(&endpoint).await.expect("bind ipc server");
        let (events_tx, _) = broadcast::channel(32);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let srv_events = events_tx.clone();
        let srv_shutdown = shutdown_tx.clone();
        let server_task = tokio::spawn(async move {
            server
                .serve(srv_events, shutdown_rx, move |req| {
                    let tx = srv_shutdown.clone();
                    async move {
                        match req {
                            RpcRequest::DaemonStatus => RpcResponse::ok(DaemonInfo {
                                version: "0.0.1".to_string(),
                                protocol_version: IPC_PROTOCOL_VERSION,
                                pid: std::process::id(),
                                uptime_secs: 7,
                                active_jobs: 1,
                                queued_jobs: 2,
                                total_jobs: 3,
                            })
                            .expect("serialize daemon info"),
                            RpcRequest::JobPause { id } => {
                                RpcResponse::err(format!("job `{id}` not found"))
                            }
                            RpcRequest::DaemonShutdown => {
                                let _ = tx.send(true);
                                RpcResponse::ok(serde_json::json!({ "shutting_down": true }))
                                    .expect("serialize shutdown")
                            }
                            _ => RpcResponse::err("unsupported in test"),
                        }
                    }
                })
                .await
                .expect("serve ipc");
        });

        let client = IpcClient::connect_with_timeout(&endpoint, Duration::from_secs(2))
            .await
            .expect("connect ipc client");
        let mut ev_rx = client.subscribe_events();

        let status = client.daemon_status().await.expect("daemon_status");
        assert_eq!(status.protocol_version, IPC_PROTOCOL_VERSION);
        assert_eq!(status.total_jobs, 3);

        let err = client
            .pause_job("missing-id")
            .await
            .expect_err("missing job should return Remote error");
        assert!(matches!(err, IpcError::Remote(msg) if msg.contains("missing-id")));

        // Verify pushed event streaming over the same connection.
        let pushed = RpcEvent::JobProgress {
            id: "job-42".to_string(),
            downloaded: 4096,
            total: Some(8192),
            speed_bps: 1024,
            state: JobState::Downloading,
        };
        events_tx.send(pushed.clone()).expect("broadcast event");
        let received = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("event timeout")
            .expect("event recv");
        assert_eq!(received, pushed);

        // Verify a second concurrent client works (tests next pipe instance creation on Windows).
        let client2 = IpcClient::connect_with_timeout(&endpoint, Duration::from_secs(2))
            .await
            .expect("second client connects");
        let status2 = client2.daemon_status().await.expect("second client status");
        assert_eq!(status2.pid, status.pid);

        client2.daemon_shutdown().await.expect("daemon shutdown");
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server join timeout")
            .expect("server task clean");
    }

    #[test]
    fn max_ipc_message_size_constant_is_1_mib() {
        assert_eq!(MAX_IPC_MESSAGE_SIZE, 1024 * 1024);
    }

    #[test]
    fn unix_socket_path_fallback_uses_maya_root() {
        assert_eq!(
            resolve_unix_socket_path_from(None, None),
            PathBuf::from("/tmp/maya-root/maya.sock")
        );
        assert_eq!(
            resolve_unix_socket_path_from(Some(""), Some("")),
            PathBuf::from("/tmp/maya-root/maya.sock")
        );
    }

    #[test]
    fn unix_socket_path_xdg_priority_over_home() {
        let p = resolve_unix_socket_path_from(Some("/custom/xdg"), Some("/custom/home"));
        assert_eq!(p, PathBuf::from("/custom/xdg/maya.sock"));
    }

    #[test]
    fn unix_socket_path_home_when_xdg_whitespace() {
        let p = resolve_unix_socket_path_from(Some(" \t "), Some("/custom/home"));
        assert_eq!(p, PathBuf::from("/custom/home/.maya/maya.sock"));
    }

    #[test]
    fn unix_socket_path_fallback_when_both_whitespace() {
        let p = resolve_unix_socket_path_from(Some("   "), Some(" \n "));
        assert_eq!(p, PathBuf::from("/tmp/maya-root/maya.sock"));
    }

    #[tokio::test]
    async fn ipc_connection_rejects_frame_exceeding_1_mib() {
        let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let mut conn = IpcConnection::new(Box::new(server));

        let writer = tokio::spawn(async move {
            let mut payload = vec![b'a'; MAX_IPC_MESSAGE_SIZE + 10];
            payload.push(b'\n');
            let _ = client.write_all(&payload).await;
        });

        let res = conn.recv_request().await;
        let _ = writer.await;

        match res {
            Err(IpcError::Protocol(msg)) => {
                assert_eq!(msg, "IPC message frame exceeded 1 MiB limit");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_connection_rejects_frame_reaching_limit_without_newline() {
        let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let mut conn = IpcConnection::new(Box::new(server));

        let writer = tokio::spawn(async move {
            let payload = vec![b'x'; MAX_IPC_MESSAGE_SIZE];
            let _ = client.write_all(&payload).await;
            drop(client);
        });

        let res = conn.recv_request().await;
        let _ = writer.await;

        match res {
            Err(IpcError::Protocol(msg)) => {
                assert_eq!(msg, "IPC message frame exceeded 1 MiB limit");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ipc_connection_accepts_valid_request_under_limit() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut conn = IpcConnection::new(Box::new(server));

        let req = IpcRequest::new(RpcRequest::JobList);
        let mut data = serde_json::to_vec(&req).unwrap();
        data.push(b'\n');

        let writer = tokio::spawn(async move {
            client.write_all(&data).await.unwrap();
        });

        let received = conn.recv_request().await.unwrap();
        writer.await.unwrap();

        assert_eq!(received, Some(req));
    }

    #[tokio::test]
    async fn ipc_connection_skips_empty_lines() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut conn = IpcConnection::new(Box::new(server));

        let req = IpcRequest::new(RpcRequest::DaemonStatus);
        let mut data = Vec::new();
        data.extend_from_slice(b"\n  \n\r\n");
        let mut req_bytes = serde_json::to_vec(&req).unwrap();
        req_bytes.push(b'\n');
        data.extend_from_slice(&req_bytes);

        let writer = tokio::spawn(async move {
            client.write_all(&data).await.unwrap();
        });

        let received = conn.recv_request().await.unwrap();
        writer.await.unwrap();

        assert_eq!(received, Some(req));
    }

    #[tokio::test]
    async fn ipc_connection_returns_none_on_eof() {
        let (client, server) = tokio::io::duplex(1024);
        let mut conn = IpcConnection::new(Box::new(server));

        drop(client);
        let received = conn.recv_request().await.unwrap();
        assert_eq!(received, None);
    }

    #[test]
    fn job_state_parsing_and_terminal_states() {
        assert_eq!(JobState::from_str("queued").unwrap(), JobState::Queued);
        assert_eq!(
            JobState::from_str("downloading").unwrap(),
            JobState::Downloading
        );
        assert_eq!(JobState::from_str("paused").unwrap(), JobState::Paused);
        assert_eq!(
            JobState::from_str("completed").unwrap(),
            JobState::Completed
        );
        assert_eq!(JobState::from_str("failed").unwrap(), JobState::Failed);
        assert_eq!(
            JobState::from_str("cancelled").unwrap(),
            JobState::Cancelled
        );
        assert_eq!(JobState::from_str("canceled").unwrap(), JobState::Cancelled);

        assert!(!JobState::Queued.is_terminal());
        assert!(!JobState::Downloading.is_terminal());
        assert!(!JobState::Paused.is_terminal());
        assert!(JobState::Completed.is_terminal());
        assert!(JobState::Failed.is_terminal());
        assert!(JobState::Cancelled.is_terminal());
    }

    #[test]
    fn job_state_from_str_invalid_returns_protocol_error() {
        let err = JobState::from_str("bogus_state").unwrap_err();
        assert!(matches!(err, IpcError::Protocol(msg) if msg.contains("bogus_state")));
    }

    #[test]
    fn job_state_display_matches_as_str() {
        for state in [
            JobState::Queued,
            JobState::Downloading,
            JobState::Paused,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert_eq!(format!("{state}"), state.as_str());
        }
    }

    #[test]
    fn job_options_defaults() {
        let opts = JobOptions::default();
        assert!(opts.output_dir.is_none());
        assert!(opts.filename.is_none());
        assert!(opts.connections.is_none());
        assert!(opts.max_bytes_per_sec.is_none());
        assert!(opts.headers.is_empty());
        assert!(opts.checksum.is_none());
        assert!(opts.overwrite.is_none());
        assert!(opts.resume.is_none());
        assert!(opts.allow_private_ips.is_none());
    }

    #[test]
    fn rpc_response_constructors_and_conversions() {
        let ok_resp = RpcResponse::ok("success_data").unwrap();
        let val: String = ok_resp.into_result().unwrap();
        assert_eq!(val, "success_data");

        let err_resp = RpcResponse::err("failure_message");
        let res: Result<String> = err_resp.into_result();
        match res {
            Err(IpcError::Remote(msg)) => assert_eq!(msg, "failure_message"),
            other => panic!("expected Remote error, got {other:?}"),
        }
    }

    #[test]
    fn ipc_request_new_generates_unique_ids() {
        let r1 = IpcRequest::new(RpcRequest::JobList);
        let r2 = IpcRequest::new(RpcRequest::JobList);
        assert_ne!(r1.id, r2.id);
        assert_eq!(r1.jsonrpc, JSONRPC_VERSION);
        assert_eq!(r2.jsonrpc, JSONRPC_VERSION);
    }

    #[test]
    fn daemon_info_serialization_roundtrip() {
        let info = DaemonInfo {
            version: "0.0.1".to_string(),
            protocol_version: IPC_PROTOCOL_VERSION,
            pid: 1234,
            uptime_secs: 5678,
            active_jobs: 2,
            queued_jobs: 5,
            total_jobs: 7,
        };
        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: DaemonInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(info, deserialized);
    }

    #[test]
    fn job_info_serialization_roundtrip() {
        let info = JobInfo {
            id: "uuid-test".to_string(),
            url: "https://example.com/file".to_string(),
            dest: "/tmp/file".to_string(),
            status: JobState::Downloading,
            downloaded_bytes: 1024,
            total_bytes: Some(2048),
            error: None,
            created_at: 1000,
            updated_at: 2000,
        };
        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: JobInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(info, deserialized);
    }

    #[tokio::test]
    async fn ipc_connection_sender_sends_responses_and_events() {
        let (client, server) = tokio::io::duplex(2048);
        let conn = IpcConnection::new(Box::new(server));
        let sender = conn.sender();
        let mut client_reader = BufReader::new(client);

        sender
            .send_response("test-id", RpcResponse::err("err_msg"))
            .await
            .unwrap();

        let mut line = String::new();
        client_reader.read_line(&mut line).await.unwrap();
        let parsed: ServerMessage = serde_json::from_str(&line).unwrap();
        match parsed {
            ServerMessage::Response(resp) => {
                assert_eq!(resp.id, "test-id");
                assert_eq!(resp.result, RpcResponse::Err("err_msg".to_string()));
            }
            _ => panic!("expected response"),
        }

        let ev = RpcEvent::JobProgress {
            id: "job-100".to_string(),
            downloaded: 100,
            total: Some(200),
            speed_bps: 50,
            state: JobState::Downloading,
        };
        sender.send_event(ev.clone()).await.unwrap();

        line.clear();
        client_reader.read_line(&mut line).await.unwrap();
        let parsed_ev: ServerMessage = serde_json::from_str(&line).unwrap();
        match parsed_ev {
            ServerMessage::Event(e) => assert_eq!(e, ev),
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn default_unix_socket_path_constant_is_secured() {
        assert_eq!(DEFAULT_UNIX_SOCKET_PATH, "/tmp/maya-root/maya.sock");
    }
}
