use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maya_core::{
    ChecksumAlgorithm, ChecksumSpec, DownloadEngine, DownloadRequest, DownloadState, MayaError,
    ProgressEvent, Result as CoreResult, StateStore,
};
use maya_ipc::{
    DaemonInfo, JobInfo, JobOptions, JobState, RpcEvent, RpcRequest, RpcResponse,
    IPC_PROTOCOL_VERSION,
};
use maya_protocols::{HttpProtocolClient, TorrentClient};
use maya_storage::{part_path_for, state_path_for, SparseDiskWriter, StateJournal};
use tokio::sync::{broadcast, mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::db::{JobStore, StoredJob};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobAction {
    Pause,
    Cancel,
    Shutdown,
}

struct ActiveJobHandle {
    run_id: u64,
    cancel_tx: watch::Sender<bool>,
    intent: Arc<Mutex<Option<JobAction>>>,
    task: JoinHandle<()>,
}

/// [`StateStore`] implementation backed by [`StateJournal`] atomic `.maya.state` files.
#[derive(Debug, Default, Clone, Copy)]
struct JournalStateStore;

impl StateStore for JournalStateStore {
    fn load<'life0, 'life1, 'async_trait>(
        &'life0 self,
        target_path: &'life1 Path,
    ) -> Pin<Box<dyn Future<Output = CoreResult<Option<DownloadState>>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let state_path = state_path_for(target_path);
        let part_path = part_path_for(target_path);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let loaded = StateJournal::load(&state_path)?;
                if let Some(ref state) = loaded {
                    if state.segments.iter().any(|s| s.downloaded > 0) && !part_path.exists() {
                        return Err(MayaError::ResumeMismatch(
                            "partial file .maya.part is missing".to_string(),
                        ));
                    }
                }
                Ok(loaded)
            })
            .await
            .map_err(|err| MayaError::Storage(format!("state load task failed: {err}")))?
        })
    }

    fn save<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        target_path: &'life1 Path,
        state: &'life2 DownloadState,
    ) -> Pin<Box<dyn Future<Output = CoreResult<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let state_path = state_path_for(target_path);
        let state = state.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || StateJournal::save_atomic(&state_path, &state))
                .await
                .map_err(|err| MayaError::Storage(format!("state save task failed: {err}")))?
        })
    }

    fn remove<'life0, 'life1, 'async_trait>(
        &'life0 self,
        target_path: &'life1 Path,
    ) -> Pin<Box<dyn Future<Output = CoreResult<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let state_path = state_path_for(target_path);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || StateJournal::remove(&state_path))
                .await
                .map_err(|err| MayaError::Storage(format!("state remove task failed: {err}")))?
        })
    }
}

/// Manages persistent download queue state, active download tasks, and IPC event broadcasting.
pub struct QueueManager {
    store: JobStore,
    max_concurrent: usize,
    started_at: Instant,
    events_tx: broadcast::Sender<RpcEvent>,
    active: Mutex<HashMap<String, ActiveJobHandle>>,
    notify: Notify,
    next_run_id: AtomicU64,
}

impl QueueManager {
    /// Construct a new [`QueueManager`] backed by `store`.
    pub fn new(store: JobStore, max_concurrent: usize) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(512);
        Arc::new(Self {
            store,
            max_concurrent: max_concurrent.max(1),
            started_at: Instant::now(),
            events_tx,
            active: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            next_run_id: AtomicU64::new(1),
        })
    }

    /// Access the broadcast sender for [`RpcEvent`] notifications.
    pub fn events_sender(&self) -> broadcast::Sender<RpcEvent> {
        self.events_tx.clone()
    }

    /// Spawn the background queue scheduler loop.
    pub fn start_scheduler(
        self: &Arc<Self>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let _ = mgr.store.recover_interrupted();
            mgr.schedule_pending().await;

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = wait_for_true(&mut shutdown_rx) => {
                        break;
                    }
                    _ = mgr.notify.notified() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                        mgr.schedule_pending().await;
                    }
                }
            }
        })
    }

    /// Trigger the scheduler to fill available concurrency slots with queued jobs.
    pub async fn schedule_pending(self: &Arc<Self>) {
        let mut active = self.active.lock().await;
        if active.len() >= self.max_concurrent {
            return;
        }
        let available = self.max_concurrent - active.len();
        let Ok(queued) = self.store.next_queued(available + active.len()) else {
            return;
        };

        for job in queued {
            if active.len() >= self.max_concurrent {
                break;
            }
            if active.contains_key(&job.info.id) {
                continue;
            }

            let job_id = job.info.id.clone();
            if self
                .store
                .update_status(&job_id, JobState::Downloading, None)
                .is_err()
            {
                continue;
            }

            let run_id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let intent = Arc::new(Mutex::new(None));

            let mgr = Arc::clone(self);
            let intent_clone = Arc::clone(&intent);
            let task = tokio::spawn(async move {
                mgr.run_job(job, run_id, cancel_rx, intent_clone).await;
            });

            active.insert(
                job_id,
                ActiveJobHandle {
                    run_id,
                    cancel_tx,
                    intent,
                    task,
                },
            );
        }
    }

    async fn run_job(
        self: Arc<Self>,
        job: StoredJob,
        run_id: u64,
        cancel_rx: watch::Receiver<bool>,
        intent: Arc<Mutex<Option<JobAction>>>,
    ) {
        let job_id = job.info.id.clone();
        let req = build_download_request(&job);

        let _ = self.events_tx.send(RpcEvent::JobProgress {
            id: job_id.clone(),
            downloaded: job.info.downloaded_bytes,
            total: job.info.total_bytes,
            speed_bps: 0,
            state: JobState::Downloading,
        });

        let (prog_tx, mut prog_rx) = mpsc::channel::<ProgressEvent>(256);
        let store_for_prog = self.store.clone();
        let events_for_prog = self.events_tx.clone();
        let job_id_prog = job_id.clone();
        let output_dir_prog = req.output_dir.clone();
        let initial_downloaded = job.info.downloaded_bytes;
        let initial_total = job.info.total_bytes;

        let progress_task = tokio::spawn(async move {
            let mut downloaded = initial_downloaded;
            let mut total = initial_total;
            let mut last_db_flush = Instant::now() - Duration::from_secs(10);
            let mut last_emit = Instant::now() - Duration::from_secs(10);
            let mut speed_window_start = Instant::now();
            let mut speed_window_bytes = 0u64;
            let mut current_speed_bps = 0u64;

            while let Some(ev) = prog_rx.recv().await {
                match ev {
                    ProgressEvent::Probed {
                        total_size,
                        filename,
                        ..
                    } => {
                        if total_size.is_some() {
                            total = total_size;
                            let _ = store_for_prog.update_progress(&job_id_prog, downloaded, total);
                        }
                        if let Some(name) = filename {
                            let resolved_dest = output_dir_prog.join(name);
                            let _ = store_for_prog
                                .update_dest(&job_id_prog, &resolved_dest.to_string_lossy());
                        }
                    }
                    ProgressEvent::Started { total_size, .. } => {
                        if total_size.is_some() {
                            total = total_size;
                            let _ = store_for_prog.update_progress(&job_id_prog, downloaded, total);
                        }
                    }
                    ProgressEvent::SegmentProgress {
                        bytes_delta,
                        total_downloaded,
                        total_size,
                        ..
                    } => {
                        downloaded = total_downloaded;
                        if total_size.is_some() {
                            total = total_size;
                        }
                        speed_window_bytes = speed_window_bytes.saturating_add(bytes_delta);
                        let elapsed = speed_window_start.elapsed();
                        if elapsed >= Duration::from_millis(250) {
                            current_speed_bps =
                                ((speed_window_bytes as f64) / elapsed.as_secs_f64()) as u64;
                            speed_window_bytes = 0;
                            speed_window_start = Instant::now();
                        }

                        if last_db_flush.elapsed() >= Duration::from_millis(500) {
                            let _ = store_for_prog.update_progress(&job_id_prog, downloaded, total);
                            last_db_flush = Instant::now();
                        }

                        if last_emit.elapsed() >= Duration::from_millis(100) {
                            let _ = events_for_prog.send(RpcEvent::JobProgress {
                                id: job_id_prog.clone(),
                                downloaded,
                                total,
                                speed_bps: current_speed_bps,
                                state: JobState::Downloading,
                            });
                            last_emit = Instant::now();
                        }
                    }
                    ProgressEvent::Completed {
                        output_path,
                        total_bytes,
                    } => {
                        downloaded = total_bytes;
                        total = Some(total_bytes);
                        let _ = store_for_prog
                            .update_dest(&job_id_prog, &output_path.to_string_lossy());
                        let _ = store_for_prog.update_progress(&job_id_prog, downloaded, total);
                    }
                    _ => {}
                }
            }
            (downloaded, total)
        });

        let torrent_client = TorrentClient::new().with_seeding(false);
        let outcome = if torrent_client.supports(&req.source) {
            torrent_client.download(&req, prog_tx, cancel_rx).await
        } else {
            let engine = DownloadEngine::new(Arc::new(HttpProtocolClient::new()), |p| {
                Ok(Arc::new(SparseDiskWriter::new(p.to_path_buf())))
            })
            .with_state_store(Arc::new(JournalStateStore))
            .with_cancel(cancel_rx);
            engine.download(&req, Some(prog_tx)).await
        };

        let (final_downloaded, final_total) = progress_task
            .await
            .unwrap_or((job.info.downloaded_bytes, job.info.total_bytes));

        let requested_action = *intent.lock().await;

        let (final_state, final_err) = match outcome {
            Ok(output_path) => {
                let _ = self
                    .store
                    .update_dest(&job_id, &output_path.to_string_lossy());
                let size = tokio::fs::metadata(&output_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(final_downloaded);
                let _ = self
                    .store
                    .update_progress(&job_id, size, final_total.or(Some(size)));
                (JobState::Completed, None)
            }
            Err(MayaError::Cancelled) => match requested_action {
                Some(JobAction::Cancel) => {
                    cleanup_partial_files(&self.store, &job_id);
                    (JobState::Cancelled, None)
                }
                Some(JobAction::Shutdown) => (JobState::Queued, None),
                Some(JobAction::Pause) | None => (JobState::Paused, None),
            },
            Err(err) => (JobState::Failed, Some(err.to_string())),
        };

        // Do not overwrite if a concurrent `JobResume` already moved the job back to `Queued`.
        let should_update_db = match self.store.get_job(&job_id) {
            Ok(Some(existing)) => {
                !(final_state == JobState::Paused && existing.info.status == JobState::Queued)
            }
            _ => true,
        };

        if should_update_db {
            let _ = self
                .store
                .update_status(&job_id, final_state, final_err.as_deref());
        }

        let latest = self.store.get_job(&job_id).ok().flatten();
        let reported_downloaded = latest
            .as_ref()
            .map(|j| j.info.downloaded_bytes)
            .unwrap_or(final_downloaded);
        let reported_total = latest
            .as_ref()
            .and_then(|j| j.info.total_bytes)
            .or(final_total);
        let reported_state = latest
            .as_ref()
            .map(|j| j.info.status)
            .unwrap_or(final_state);

        let _ = self.events_tx.send(RpcEvent::JobProgress {
            id: job_id.clone(),
            downloaded: reported_downloaded,
            total: reported_total,
            speed_bps: 0,
            state: reported_state,
        });

        {
            let mut active = self.active.lock().await;
            if active.get(&job_id).is_some_and(|h| h.run_id == run_id) {
                active.remove(&job_id);
            }
        }
        self.notify.notify_one();
    }

    /// Dispatch an incoming [`RpcRequest`] and return a [`RpcResponse`].
    pub async fn handle_rpc(
        self: &Arc<Self>,
        request: RpcRequest,
        shutdown_tx: &watch::Sender<bool>,
    ) -> RpcResponse {
        match request {
            RpcRequest::JobAdd { url, options } => self.cmd_job_add(url, options).await,
            RpcRequest::JobPause { id } => self.cmd_job_pause(&id).await,
            RpcRequest::JobResume { id } => self.cmd_job_resume(&id).await,
            RpcRequest::JobCancel { id } => self.cmd_job_cancel(&id).await,
            RpcRequest::JobList => self.cmd_job_list(),
            RpcRequest::JobStatus { id } => self.cmd_job_status(&id),
            RpcRequest::DaemonStatus => self.cmd_daemon_status().await,
            RpcRequest::DaemonShutdown => {
                let _ = shutdown_tx.send(true);
                self.notify.notify_waiters();
                match RpcResponse::ok(serde_json::json!({ "shutting_down": true })) {
                    Ok(resp) => resp,
                    Err(e) => RpcResponse::err(e.to_string()),
                }
            }
        }
    }

    async fn cmd_job_add(self: &Arc<Self>, url: String, options: JobOptions) -> RpcResponse {
        if url.trim().is_empty() {
            return RpcResponse::err("url must not be empty");
        }
        let dest_dir = resolve_output_dir(&options);
        let initial_dest = match &options.filename {
            Some(name) if !name.trim().is_empty() => dest_dir.join(name),
            _ => dest_dir,
        };

        let job = match self
            .store
            .insert_job(&url, &initial_dest.to_string_lossy(), &options)
        {
            Ok(job) => job,
            Err(err) => return RpcResponse::err(format!("failed to persist job: {err}")),
        };

        let _ = self.events_tx.send(RpcEvent::JobProgress {
            id: job.info.id.clone(),
            downloaded: 0,
            total: None,
            speed_bps: 0,
            state: JobState::Queued,
        });

        self.notify.notify_one();

        match RpcResponse::ok(job.info) {
            Ok(resp) => resp,
            Err(err) => RpcResponse::err(err.to_string()),
        }
    }

    async fn cmd_job_pause(&self, id: &str) -> RpcResponse {
        let Some(job) = (match self.store.get_job(id) {
            Ok(j) => j,
            Err(err) => return RpcResponse::err(err.to_string()),
        }) else {
            return RpcResponse::err(format!("job `{id}` not found"));
        };

        match job.info.status {
            JobState::Paused => match RpcResponse::ok(job.info) {
                Ok(resp) => resp,
                Err(err) => RpcResponse::err(err.to_string()),
            },
            JobState::Queued | JobState::Downloading => {
                {
                    let active = self.active.lock().await;
                    if let Some(handle) = active.get(id) {
                        *handle.intent.lock().await = Some(JobAction::Pause);
                        let _ = handle.cancel_tx.send(true);
                    }
                }
                if let Err(err) = self.store.update_status(id, JobState::Paused, None) {
                    return RpcResponse::err(err.to_string());
                }
                let updated = match self.store.get_job(id) {
                    Ok(Some(j)) => j.info,
                    Ok(None) => return RpcResponse::err(format!("job `{id}` not found")),
                    Err(err) => return RpcResponse::err(err.to_string()),
                };
                let _ = self.events_tx.send(RpcEvent::JobProgress {
                    id: updated.id.clone(),
                    downloaded: updated.downloaded_bytes,
                    total: updated.total_bytes,
                    speed_bps: 0,
                    state: JobState::Paused,
                });
                match RpcResponse::ok(updated) {
                    Ok(resp) => resp,
                    Err(err) => RpcResponse::err(err.to_string()),
                }
            }
            other => RpcResponse::err(format!("cannot pause job in `{other}` state")),
        }
    }

    async fn cmd_job_resume(&self, id: &str) -> RpcResponse {
        let Some(job) = (match self.store.get_job(id) {
            Ok(j) => j,
            Err(err) => return RpcResponse::err(err.to_string()),
        }) else {
            return RpcResponse::err(format!("job `{id}` not found"));
        };

        match job.info.status {
            JobState::Queued | JobState::Downloading => match RpcResponse::ok(job.info) {
                Ok(resp) => resp,
                Err(err) => RpcResponse::err(err.to_string()),
            },
            JobState::Paused | JobState::Failed => {
                if let Err(err) = self.store.update_status(id, JobState::Queued, None) {
                    return RpcResponse::err(err.to_string());
                }
                let updated = match self.store.get_job(id) {
                    Ok(Some(j)) => j.info,
                    Ok(None) => return RpcResponse::err(format!("job `{id}` not found")),
                    Err(err) => return RpcResponse::err(err.to_string()),
                };
                let _ = self.events_tx.send(RpcEvent::JobProgress {
                    id: updated.id.clone(),
                    downloaded: updated.downloaded_bytes,
                    total: updated.total_bytes,
                    speed_bps: 0,
                    state: JobState::Queued,
                });
                self.notify.notify_one();
                match RpcResponse::ok(updated) {
                    Ok(resp) => resp,
                    Err(err) => RpcResponse::err(err.to_string()),
                }
            }
            other => RpcResponse::err(format!("cannot resume job in `{other}` state")),
        }
    }

    async fn cmd_job_cancel(&self, id: &str) -> RpcResponse {
        let Some(job) = (match self.store.get_job(id) {
            Ok(j) => j,
            Err(err) => return RpcResponse::err(err.to_string()),
        }) else {
            return RpcResponse::err(format!("job `{id}` not found"));
        };

        match job.info.status {
            JobState::Cancelled => match RpcResponse::ok(job.info) {
                Ok(resp) => resp,
                Err(err) => RpcResponse::err(err.to_string()),
            },
            JobState::Queued | JobState::Downloading | JobState::Paused => {
                {
                    let active = self.active.lock().await;
                    if let Some(handle) = active.get(id) {
                        *handle.intent.lock().await = Some(JobAction::Cancel);
                        let _ = handle.cancel_tx.send(true);
                    }
                }
                if let Err(err) = self.store.update_status(id, JobState::Cancelled, None) {
                    return RpcResponse::err(err.to_string());
                }
                cleanup_partial_files(&self.store, id);
                let updated = match self.store.get_job(id) {
                    Ok(Some(j)) => j.info,
                    Ok(None) => return RpcResponse::err(format!("job `{id}` not found")),
                    Err(err) => return RpcResponse::err(err.to_string()),
                };
                let _ = self.events_tx.send(RpcEvent::JobProgress {
                    id: updated.id.clone(),
                    downloaded: updated.downloaded_bytes,
                    total: updated.total_bytes,
                    speed_bps: 0,
                    state: JobState::Cancelled,
                });
                match RpcResponse::ok(updated) {
                    Ok(resp) => resp,
                    Err(err) => RpcResponse::err(err.to_string()),
                }
            }
            other => RpcResponse::err(format!("cannot cancel job in `{other}` state")),
        }
    }

    fn cmd_job_list(&self) -> RpcResponse {
        match self.store.list_jobs() {
            Ok(jobs) => {
                let infos: Vec<JobInfo> = jobs.into_iter().map(|j| j.info).collect();
                match RpcResponse::ok(infos) {
                    Ok(resp) => resp,
                    Err(err) => RpcResponse::err(err.to_string()),
                }
            }
            Err(err) => RpcResponse::err(err.to_string()),
        }
    }

    fn cmd_job_status(&self, id: &str) -> RpcResponse {
        match self.store.get_job(id) {
            Ok(Some(job)) => match RpcResponse::ok(job.info) {
                Ok(resp) => resp,
                Err(err) => RpcResponse::err(err.to_string()),
            },
            Ok(None) => RpcResponse::err(format!("job `{id}` not found")),
            Err(err) => RpcResponse::err(err.to_string()),
        }
    }

    async fn cmd_daemon_status(&self) -> RpcResponse {
        let active_jobs = self.active.lock().await.len();
        let jobs = match self.store.list_jobs() {
            Ok(j) => j,
            Err(err) => return RpcResponse::err(err.to_string()),
        };
        let queued_jobs = jobs
            .iter()
            .filter(|j| j.info.status == JobState::Queued)
            .count();
        let info = DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: IPC_PROTOCOL_VERSION,
            pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            active_jobs,
            queued_jobs,
            total_jobs: jobs.len(),
        };
        match RpcResponse::ok(info) {
            Ok(resp) => resp,
            Err(err) => RpcResponse::err(err.to_string()),
        }
    }

    /// Cancel active downloads with shutdown intent and await task cleanup.
    pub async fn shutdown_and_drain(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut active = self.active.lock().await;
            let mut tasks = Vec::with_capacity(active.len());
            for (_, handle) in active.drain() {
                *handle.intent.lock().await = Some(JobAction::Shutdown);
                let _ = handle.cancel_tx.send(true);
                tasks.push(handle.task);
            }
            tasks
        };

        for task in handles {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

fn resolve_output_dir(options: &JobOptions) -> PathBuf {
    match &options.output_dir {
        Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from(".")),
    }
}

fn build_download_request(job: &StoredJob) -> DownloadRequest {
    let output_dir = resolve_output_dir(&job.options);
    let mut req = DownloadRequest::new(&job.info.url, output_dir);
    req.filename = job.options.filename.clone();
    if let Some(conns) = job.options.connections {
        req.connections = conns.max(1);
    }
    req.max_bytes_per_sec = job.options.max_bytes_per_sec;
    req.headers = job.options.headers.clone();
    if let Some(resume) = job.options.resume {
        req.resume = resume;
    }
    if let Some(overwrite) = job.options.overwrite {
        req.overwrite = overwrite;
    }
    if let Some(allow_private) = job.options.allow_private_ips {
        req.security.allow_private_ips = allow_private;
    }
    if let Some(ref raw_checksum) = job.options.checksum {
        req.checksum = parse_checksum(raw_checksum);
    }
    req
}

fn parse_checksum(raw: &str) -> Option<ChecksumSpec> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((algo, hex)) = trimmed.split_once(':') {
        let algorithm = match algo.trim().to_ascii_lowercase().as_str() {
            "sha256" | "sha-256" => ChecksumAlgorithm::Sha256,
            "blake3" | "b3" => ChecksumAlgorithm::Blake3,
            _ => return None,
        };
        return Some(ChecksumSpec {
            algorithm,
            expected_hex: hex.trim().to_ascii_lowercase(),
        });
    }
    if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha256,
            expected_hex: trimmed.to_ascii_lowercase(),
        });
    }
    None
}

fn cleanup_partial_files(store: &JobStore, job_id: &str) {
    if let Ok(Some(job)) = store.get_job(job_id) {
        let dest = PathBuf::from(&job.info.dest);
        if dest.file_name().is_some() && !dest.is_dir() {
            let _ = std::fs::remove_file(part_path_for(&dest));
            let _ = std::fs::remove_file(state_path_for(&dest));
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
