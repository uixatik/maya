use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinSet;

use crate::error::{MayaError, Result};
use crate::limiter::TokenBucketLimiter;
use crate::scheduler::{SegmentScheduler, WorkAssignment};
use crate::security::{derive_filename_from_url, safe_target_path, sanitize_filename};
use crate::types::{
    DownloadRequest, DownloadState, ProbeResult, ProgressEvent, ProtocolClient, StorageWriter,
};

/// Current schema version for persisted [`DownloadState`] checkpoints.
pub const STATE_VERSION: u32 = 1;

const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_BASE_RETRY_DELAY: Duration = Duration::from_millis(200);
const DEFAULT_CHECKPOINT_INTERVAL_BYTES: u64 = 512 * 1024;

/// Factory closure producing a [`StorageWriter`] once the target output path is resolved.
pub type WriterFactory = Arc<dyn Fn(&Path) -> Result<Arc<dyn StorageWriter>> + Send + Sync>;

/// Optional persistence hook for loading, saving, and clearing `.maya.state` checkpoints.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Load any existing checkpoint associated with `target_path`.
    async fn load(&self, target_path: &Path) -> Result<Option<DownloadState>>;

    /// Atomically persist a checkpoint for `target_path`.
    async fn save(&self, target_path: &Path, state: &DownloadState) -> Result<()>;

    /// Remove the checkpoint associated with `target_path`.
    async fn remove(&self, target_path: &Path) -> Result<()>;
}

/// Core segmented, resumable, rate-limited download orchestrator.
pub struct DownloadEngine {
    protocol: Arc<dyn ProtocolClient>,
    writer_factory: WriterFactory,
    state_store: Option<Arc<dyn StateStore>>,
    cancel: Option<watch::Receiver<bool>>,
    limiter: TokenBucketLimiter,
    max_retries: usize,
    base_retry_delay: Duration,
    checkpoint_interval_bytes: u64,
}

impl DownloadEngine {
    /// Construct a new [`DownloadEngine`] with a [`ProtocolClient`] and [`StorageWriter`] factory.
    pub fn new<F>(protocol: Arc<dyn ProtocolClient>, writer_factory: F) -> Self
    where
        F: Fn(&Path) -> Result<Arc<dyn StorageWriter>> + Send + Sync + 'static,
    {
        Self {
            protocol,
            writer_factory: Arc::new(writer_factory),
            state_store: None,
            cancel: None,
            limiter: TokenBucketLimiter::unlimited(),
            max_retries: DEFAULT_MAX_RETRIES,
            base_retry_delay: DEFAULT_BASE_RETRY_DELAY,
            checkpoint_interval_bytes: DEFAULT_CHECKPOINT_INTERVAL_BYTES,
        }
    }

    /// Attach a [`StateStore`] for resumable `.maya.state` checkpointing.
    pub fn with_state_store(mut self, store: Arc<dyn StateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Attach a cancellation watch channel (`true` cancels the active download).
    pub fn with_cancel(mut self, cancel: watch::Receiver<bool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Attach an existing [`TokenBucketLimiter`] (e.g., shared across downloads).
    pub fn with_limiter(mut self, limiter: TokenBucketLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// Configure maximum retry attempts per segment on transient errors.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Configure the initial exponential backoff delay for segment retries.
    pub fn with_base_retry_delay(mut self, delay: Duration) -> Self {
        self.base_retry_delay = delay;
        self
    }

    /// Configure how many bytes are written between periodic state checkpoints.
    pub fn with_checkpoint_interval_bytes(mut self, bytes: u64) -> Self {
        self.checkpoint_interval_bytes = bytes.max(1);
        self
    }

    /// Access the engine's bandwidth limiter (e.g., for dynamic rate adjustment).
    pub fn limiter(&self) -> &TokenBucketLimiter {
        &self.limiter
    }

    /// Execute `request` from probe through segmented transfer, verification, and finalization.
    pub async fn download(
        &self,
        request: &DownloadRequest,
        events: Option<mpsc::Sender<ProgressEvent>>,
    ) -> Result<PathBuf> {
        let result = self.download_inner(request, events.clone()).await;
        if let Err(ref err) = result {
            emit_event(
                &events,
                ProgressEvent::Failed {
                    error: err.to_string(),
                },
            )
            .await;
        }
        result
    }

    async fn download_inner(
        &self,
        request: &DownloadRequest,
        events: Option<mpsc::Sender<ProgressEvent>>,
    ) -> Result<PathBuf> {
        if !self.protocol.supports(&request.source) {
            return Err(MayaError::UnsupportedScheme(request.source.clone()));
        }
        if request.connections == 0 {
            return Err(MayaError::InvalidConfig(
                "connections must be at least 1".to_string(),
            ));
        }
        if is_cancelled(&self.cancel) {
            return Err(MayaError::Cancelled);
        }
        if request.max_bytes_per_sec.is_some() {
            self.limiter.set_rate(request.max_bytes_per_sec);
        }

        let probe = self.protocol.probe(request).await?;

        if let (Some(max_bytes), Some(total)) =
            (request.security.max_download_bytes, probe.total_size)
        {
            if total > max_bytes {
                return Err(MayaError::SecurityViolation(format!(
                    "remote content length {total} exceeds max_download_bytes {max_bytes}"
                )));
            }
        }

        let (filename, target_path) = resolve_target_path(request, &probe)?;
        if target_path.exists() && !request.overwrite {
            return Err(MayaError::Storage(format!(
                "destination file already exists: {}",
                target_path.display()
            )));
        }

        emit_event(
            &events,
            ProgressEvent::Probed {
                resolved_url: probe.resolved_url.clone(),
                total_size: probe.total_size,
                supports_ranges: probe.supports_ranges,
                filename: Some(filename),
            },
        )
        .await;

        if is_cancelled(&self.cancel) {
            return Err(MayaError::Cancelled);
        }

        let loaded_state = if request.resume {
            if let Some(store) = &self.state_store {
                store.load(&target_path).await?
            } else {
                None
            }
        } else {
            None
        };

        let scheduler = match loaded_state {
            Some(ref saved) => {
                validate_resume_state(saved, &probe)?;
                Arc::new(SegmentScheduler::from_state(
                    saved,
                    probe.supports_ranges,
                    request.connections,
                    request.min_segment_size,
                )?)
            }
            None => Arc::new(SegmentScheduler::new(
                probe.total_size,
                probe.supports_ranges,
                request.connections,
                request.min_segment_size,
            )),
        };

        let writer = (self.writer_factory)(&target_path)?;
        writer.prepare(probe.total_size).await?;

        emit_event(
            &events,
            ProgressEvent::Started {
                total_size: probe.total_size,
                segment_count: scheduler.segment_count(),
            },
        )
        .await;

        let checkpointer = Arc::new(Checkpointer {
            store: self.state_store.clone(),
            source: request.source.clone(),
            resolved_url: probe.resolved_url.clone(),
            target_path: target_path.clone(),
            part_path: part_path_hint(&target_path),
            total_size: probe.total_size,
            etag: probe.etag.clone(),
            last_modified: probe.last_modified.clone(),
            interval_bytes: self.checkpoint_interval_bytes,
            pending_bytes: AtomicU64::new(0),
            save_lock: Mutex::new(()),
        });

        if !scheduler.is_finished() {
            checkpointer.save(&scheduler).await?;

            let worker_count = if probe.supports_ranges && probe.total_size.is_some() {
                request.connections.max(1)
            } else {
                1
            };

            let (abort_tx, _) = watch::channel(false);
            let mut join_set: JoinSet<Result<()>> = JoinSet::new();

            for _ in 0..worker_count {
                let ctx = WorkerContext {
                    protocol: Arc::clone(&self.protocol),
                    writer: Arc::clone(&writer),
                    scheduler: Arc::clone(&scheduler),
                    limiter: self.limiter.clone(),
                    checkpointer: Arc::clone(&checkpointer),
                    request: request.clone(),
                    supports_ranges: probe.supports_ranges,
                    max_download_bytes: request.security.max_download_bytes,
                    max_retries: self.max_retries,
                    base_retry_delay: self.base_retry_delay,
                    events: events.clone(),
                    external_cancel: self.cancel.clone(),
                    internal_abort: abort_tx.subscribe(),
                };
                join_set.spawn(async move { run_worker(ctx).await });
            }

            let mut first_err: Option<MayaError> = None;
            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                            let _ = abort_tx.send(true);
                        }
                    }
                    Err(join_err) => {
                        if first_err.is_none() {
                            first_err = Some(MayaError::Protocol(format!(
                                "download worker task failed: {join_err}"
                            )));
                            let _ = abort_tx.send(true);
                        }
                    }
                }
            }

            if let Some(err) = first_err {
                let should_preserve_resume = request.resume
                    && !matches!(
                        err,
                        MayaError::SecurityViolation(_) | MayaError::ResumeMismatch(_)
                    );

                if should_preserve_resume {
                    let _ = writer.flush().await;
                    let _ = checkpointer.save(&scheduler).await;
                } else {
                    let _ = writer.abort().await;
                    if let Some(store) = &self.state_store {
                        let _ = store.remove(&target_path).await;
                    }
                }
                return Err(err);
            }

            if !scheduler.is_finished() || scheduler.has_failed() {
                let err = MayaError::Protocol("download finished with incomplete segments".into());
                if request.resume {
                    let _ = writer.flush().await;
                    let _ = checkpointer.save(&scheduler).await;
                } else {
                    let _ = writer.abort().await;
                }
                return Err(err);
            }
        }

        writer.flush().await?;

        if let Some(spec) = &request.checksum {
            emit_event(
                &events,
                ProgressEvent::Verifying {
                    algorithm: spec.algorithm,
                },
            )
            .await;
        }

        let output_path = writer.finalize(request.checksum.as_ref()).await?;
        if let Some(store) = &self.state_store {
            let _ = store.remove(&target_path).await;
        }

        let total_bytes = scheduler.total_downloaded();
        emit_event(
            &events,
            ProgressEvent::Completed {
                output_path: output_path.clone(),
                total_bytes,
            },
        )
        .await;

        Ok(output_path)
    }
}

struct WorkerContext {
    protocol: Arc<dyn ProtocolClient>,
    writer: Arc<dyn StorageWriter>,
    scheduler: Arc<SegmentScheduler>,
    limiter: TokenBucketLimiter,
    checkpointer: Arc<Checkpointer>,
    request: DownloadRequest,
    supports_ranges: bool,
    max_download_bytes: Option<u64>,
    max_retries: usize,
    base_retry_delay: Duration,
    events: Option<mpsc::Sender<ProgressEvent>>,
    external_cancel: Option<watch::Receiver<bool>>,
    internal_abort: watch::Receiver<bool>,
}

struct Checkpointer {
    store: Option<Arc<dyn StateStore>>,
    source: String,
    resolved_url: String,
    target_path: PathBuf,
    part_path: PathBuf,
    total_size: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    interval_bytes: u64,
    pending_bytes: AtomicU64,
    save_lock: Mutex<()>,
}

impl Checkpointer {
    async fn maybe_save(&self, scheduler: &SegmentScheduler, delta: u64) -> Result<()> {
        if self.store.is_none() {
            return Ok(());
        }
        let prev = self.pending_bytes.fetch_add(delta, Ordering::Relaxed);
        if prev + delta >= self.interval_bytes {
            self.pending_bytes.store(0, Ordering::Relaxed);
            self.save(scheduler).await?;
        }
        Ok(())
    }

    async fn save(&self, scheduler: &SegmentScheduler) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let _guard = self.save_lock.lock().await;
        let state = DownloadState {
            version: STATE_VERSION,
            source: self.source.clone(),
            resolved_url: self.resolved_url.clone(),
            target_path: self.target_path.clone(),
            part_path: self.part_path.clone(),
            total_size: self.total_size,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            segments: scheduler.segments(),
        };
        store.save(&self.target_path, &state).await
    }
}

async fn run_worker(mut ctx: WorkerContext) -> Result<()> {
    while let Some(mut assignment) = ctx.scheduler.next_work() {
        let mut attempt = 0usize;
        loop {
            if check_aborted(&ctx.external_cancel, &ctx.internal_abort)? {
                return Err(MayaError::Cancelled);
            }

            let step = execute_segment(&mut ctx, assignment).await;
            match step {
                Ok(()) => {
                    let _ = ctx.checkpointer.save(&ctx.scheduler).await;
                    break;
                }
                Err(err) => {
                    if matches!(err, MayaError::Cancelled) {
                        return Err(MayaError::Cancelled);
                    }
                    if !is_retryable(&err) || attempt >= ctx.max_retries {
                        ctx.scheduler.mark_failed(assignment.segment_index);
                        return Err(err);
                    }

                    let delay = retry_backoff(ctx.base_retry_delay, attempt);
                    attempt += 1;
                    sleep_or_cancel(delay, &mut ctx.external_cancel, &mut ctx.internal_abort)
                        .await?;

                    match ctx.scheduler.prepare_retry(assignment.segment_index) {
                        Some(next_assignment) => {
                            assignment = next_assignment;
                        }
                        None => {
                            // Remaining bytes were already completed/stolen while waiting.
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn execute_segment(ctx: &mut WorkerContext, assignment: WorkAssignment) -> Result<()> {
    let mut stream = tokio::select! {
        res = ctx.protocol.fetch_range(&ctx.request, assignment.start_offset, assignment.end) => res?,
        _ = wait_aborted(&mut ctx.external_cancel, &mut ctx.internal_abort) => {
            return Err(MayaError::Cancelled);
        }
    };

    loop {
        let next_item = tokio::select! {
            item = stream.next() => item,
            _ = wait_aborted(&mut ctx.external_cancel, &mut ctx.internal_abort) => {
                return Err(MayaError::Cancelled);
            }
        };

        let Some(chunk_res) = next_item else {
            break;
        };
        let chunk = chunk_res?;
        if chunk.is_empty() {
            continue;
        }

        let outcome = ctx
            .scheduler
            .record_write(assignment.segment_index, chunk.len() as u64)?;

        if outcome.accepted_bytes == 0 {
            break;
        }

        if let Some(max_bytes) = ctx.max_download_bytes {
            if outcome.total_downloaded > max_bytes {
                ctx.scheduler
                    .rollback_write(assignment.segment_index, outcome.accepted_bytes);
                return Err(MayaError::SecurityViolation(format!(
                    "downloaded bytes {} exceeded max_download_bytes {}",
                    outcome.total_downloaded, max_bytes
                )));
            }
        }

        tokio::select! {
            biased;
            _ = wait_aborted(&mut ctx.external_cancel, &mut ctx.internal_abort) => {
                ctx.scheduler
                    .rollback_write(assignment.segment_index, outcome.accepted_bytes);
                return Err(MayaError::Cancelled);
            }
            _ = ctx.limiter.acquire(outcome.accepted_bytes) => {}
        }

        if check_aborted(&ctx.external_cancel, &ctx.internal_abort)? {
            ctx.scheduler
                .rollback_write(assignment.segment_index, outcome.accepted_bytes);
            return Err(MayaError::Cancelled);
        }

        let slice = if (outcome.accepted_bytes as usize) < chunk.len() {
            chunk.slice(..outcome.accepted_bytes as usize)
        } else {
            chunk
        };

        if let Err(err) = ctx.writer.write_at(outcome.write_offset, slice).await {
            ctx.scheduler
                .rollback_write(assignment.segment_index, outcome.accepted_bytes);
            return Err(err);
        }

        emit_event(
            &ctx.events,
            ProgressEvent::SegmentProgress {
                segment_index: assignment.segment_index,
                bytes_delta: outcome.accepted_bytes,
                total_downloaded: outcome.total_downloaded,
                total_size: ctx.scheduler.total_size(),
            },
        )
        .await;

        ctx.checkpointer
            .maybe_save(&ctx.scheduler, outcome.accepted_bytes)
            .await?;

        if outcome.segment_completed {
            emit_event(
                &ctx.events,
                ProgressEvent::SegmentCompleted {
                    segment_index: assignment.segment_index,
                },
            )
            .await;
            return Ok(());
        }
    }

    // Stream reached EOF.
    if assignment.end.is_none() {
        ctx.scheduler.mark_completed(assignment.segment_index);
        emit_event(
            &ctx.events,
            ProgressEvent::SegmentCompleted {
                segment_index: assignment.segment_index,
            },
        )
        .await;
        return Ok(());
    }

    if ctx.scheduler.is_segment_complete(assignment.segment_index) {
        emit_event(
            &ctx.events,
            ProgressEvent::SegmentCompleted {
                segment_index: assignment.segment_index,
            },
        )
        .await;
        return Ok(());
    }

    let _ = ctx.supports_ranges;
    Err(MayaError::Network(format!(
        "stream ended prematurely for segment {}",
        assignment.segment_index
    )))
}

fn resolve_target_path(
    request: &DownloadRequest,
    probe: &ProbeResult,
) -> Result<(String, PathBuf)> {
    let filename = if let Some(explicit) = &request.filename {
        if request.security.enforce_safe_paths {
            let path = safe_target_path(&request.output_dir, explicit)?;
            return Ok((explicit.clone(), path));
        }
        return Ok((explicit.clone(), request.output_dir.join(explicit)));
    } else if let Some(suggested) = &probe.suggested_filename {
        sanitize_filename(suggested)
    } else if let Ok(url) = url::Url::parse(&probe.resolved_url) {
        derive_filename_from_url(&url)
    } else if let Ok(url) = url::Url::parse(&request.source) {
        derive_filename_from_url(&url)
    } else {
        "download".to_string()
    };

    let target_path = if request.security.enforce_safe_paths {
        safe_target_path(&request.output_dir, &filename)?
    } else {
        request.output_dir.join(&filename)
    };

    Ok((filename, target_path))
}

fn validate_resume_state(saved: &DownloadState, probe: &ProbeResult) -> Result<()> {
    if saved.version != STATE_VERSION {
        return Err(MayaError::ResumeMismatch(format!(
            "unsupported state version {} (expected {STATE_VERSION})",
            saved.version
        )));
    }

    if !probe.supports_ranges {
        return Err(MayaError::ResumeMismatch(
            "remote server does not support range requests for resume".to_string(),
        ));
    }

    if probe.total_size.is_none() || saved.total_size != probe.total_size {
        return Err(MayaError::ResumeMismatch(format!(
            "remote size mismatch (saved: {:?}, remote: {:?})",
            saved.total_size, probe.total_size
        )));
    }

    if saved.etag.is_some() || probe.etag.is_some() {
        if saved.etag != probe.etag {
            return Err(MayaError::ResumeMismatch(format!(
                "ETag mismatch (saved: {:?}, remote: {:?})",
                saved.etag, probe.etag
            )));
        }
    } else if (saved.last_modified.is_some() || probe.last_modified.is_some())
        && saved.last_modified != probe.last_modified
    {
        return Err(MayaError::ResumeMismatch(format!(
            "Last-Modified mismatch (saved: {:?}, remote: {:?})",
            saved.last_modified, probe.last_modified
        )));
    }

    let total = probe.total_size.unwrap_or(0);
    for seg in &saved.segments {
        match seg.end {
            Some(end) if end >= seg.start && end <= total => {}
            _ => {
                return Err(MayaError::ResumeMismatch(
                    "saved segment bounds exceed remote resource size".to_string(),
                ));
            }
        }
    }

    Ok(())
}

fn part_path_hint(target_path: &Path) -> PathBuf {
    let mut os = target_path.as_os_str().to_os_string();
    os.push(".maya.part");
    PathBuf::from(os)
}

fn is_retryable(err: &MayaError) -> bool {
    match err {
        MayaError::Network(_) | MayaError::Timeout(_) | MayaError::Io(_) => true,
        MayaError::HttpStatus { status, .. } => *status == 408 || *status == 429 || *status >= 500,
        _ => false,
    }
}

fn retry_backoff(base: Duration, attempt: usize) -> Duration {
    let multiplier = 1u32.checked_shl(attempt.min(10) as u32).unwrap_or(1024);
    base.saturating_mul(multiplier).min(Duration::from_secs(30))
}

fn is_cancelled(cancel: &Option<watch::Receiver<bool>>) -> bool {
    cancel.as_ref().is_some_and(|rx| *rx.borrow())
}

fn check_aborted(
    external_cancel: &Option<watch::Receiver<bool>>,
    internal_abort: &watch::Receiver<bool>,
) -> Result<bool> {
    if *internal_abort.borrow() || is_cancelled(external_cancel) {
        return Ok(true);
    }
    Ok(false)
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

async fn wait_aborted(
    external_cancel: &mut Option<watch::Receiver<bool>>,
    internal_abort: &mut watch::Receiver<bool>,
) {
    match external_cancel {
        Some(ext) => {
            tokio::select! {
                _ = wait_for_true(ext) => {}
                _ = wait_for_true(internal_abort) => {}
            }
        }
        None => {
            wait_for_true(internal_abort).await;
        }
    }
}

async fn sleep_or_cancel(
    delay: Duration,
    external_cancel: &mut Option<watch::Receiver<bool>>,
    internal_abort: &mut watch::Receiver<bool>,
) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = wait_aborted(external_cancel, internal_abort) => Err(MayaError::Cancelled),
    }
}

async fn emit_event(tx: &Option<mpsc::Sender<ProgressEvent>>, event: ProgressEvent) {
    if let Some(sender) = tx {
        let _ = sender.send(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    use bytes::Bytes;
    use futures::stream;

    use crate::types::{ByteStream, ChecksumAlgorithm, ChecksumSpec, ProtocolKind, SecurityPolicy};

    struct MemoryStorageWriter {
        target_path: PathBuf,
        buffer: StdMutex<Vec<u8>>,
        prepared_size: StdMutex<Option<u64>>,
        aborted: AtomicBool,
        finalized: AtomicBool,
    }

    impl MemoryStorageWriter {
        fn new(target_path: PathBuf) -> Self {
            Self {
                target_path,
                buffer: StdMutex::new(Vec::new()),
                prepared_size: StdMutex::new(None),
                aborted: AtomicBool::new(false),
                finalized: AtomicBool::new(false),
            }
        }

        fn data(&self) -> Vec<u8> {
            self.buffer.lock().unwrap().clone()
        }

        fn was_aborted(&self) -> bool {
            self.aborted.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl StorageWriter for MemoryStorageWriter {
        async fn prepare(&self, total_size: Option<u64>) -> Result<()> {
            *self.prepared_size.lock().unwrap() = total_size;
            if let Some(len) = total_size {
                let mut buf = self.buffer.lock().unwrap();
                if buf.len() < len as usize {
                    buf.resize(len as usize, 0);
                }
            }
            Ok(())
        }

        async fn write_at(&self, offset: u64, data: Bytes) -> Result<()> {
            let mut buf = self.buffer.lock().unwrap();
            let end = offset as usize + data.len();
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[offset as usize..end].copy_from_slice(&data);
            Ok(())
        }

        async fn flush(&self) -> Result<()> {
            Ok(())
        }

        async fn finalize(&self, checksum: Option<&ChecksumSpec>) -> Result<PathBuf> {
            if let Some(spec) = checksum {
                let buf = self.buffer.lock().unwrap();
                let actual = match spec.algorithm {
                    ChecksumAlgorithm::Sha256 => {
                        use sha2::Digest;
                        let digest = sha2::Sha256::digest(&*buf);
                        digest
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    }
                    ChecksumAlgorithm::Sha512 => {
                        use sha2::Digest;
                        let digest = sha2::Sha512::digest(&*buf);
                        digest
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    }
                    ChecksumAlgorithm::Blake3 => blake3::hash(&buf).to_hex().to_string(),
                    _ => unimplemented!("algorithm not handled in memory writer test mock"),
                };
                if actual != spec.expected_hex.to_ascii_lowercase() {
                    return Err(MayaError::ChecksumMismatch {
                        expected: spec.expected_hex.clone(),
                        actual,
                    });
                }
            }
            self.finalized.store(true, Ordering::SeqCst);
            Ok(self.target_path.clone())
        }

        async fn abort(&self) -> Result<()> {
            self.aborted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryStateStore {
        states: StdMutex<HashMap<PathBuf, DownloadState>>,
    }

    #[async_trait]
    impl StateStore for MemoryStateStore {
        async fn load(&self, target_path: &Path) -> Result<Option<DownloadState>> {
            Ok(self.states.lock().unwrap().get(target_path).cloned())
        }

        async fn save(&self, target_path: &Path, state: &DownloadState) -> Result<()> {
            self.states
                .lock()
                .unwrap()
                .insert(target_path.to_path_buf(), state.clone());
            Ok(())
        }

        async fn remove(&self, target_path: &Path) -> Result<()> {
            self.states.lock().unwrap().remove(target_path);
            Ok(())
        }
    }

    struct MockProtocolClient {
        payload: Vec<u8>,
        supports_ranges: bool,
        unknown_length_on_probe: bool,
        etag: Option<String>,
        chunk_size: usize,
        fail_first_n_fetches: AtomicU64,
        fail_mid_stream_once: AtomicBool,
        delay_per_chunk: Duration,
    }

    impl MockProtocolClient {
        fn new(payload: Vec<u8>, supports_ranges: bool) -> Self {
            Self {
                payload,
                supports_ranges,
                unknown_length_on_probe: false,
                etag: Some("\"v1\"".to_string()),
                chunk_size: 64,
                fail_first_n_fetches: AtomicU64::new(0),
                fail_mid_stream_once: AtomicBool::new(false),
                delay_per_chunk: Duration::ZERO,
            }
        }
    }

    #[async_trait]
    impl ProtocolClient for MockProtocolClient {
        fn supports(&self, source: &str) -> bool {
            source.starts_with("https://") || source.starts_with("http://")
        }

        async fn probe(&self, request: &DownloadRequest) -> Result<ProbeResult> {
            Ok(ProbeResult {
                protocol: ProtocolKind::Https,
                resolved_url: request.source.clone(),
                total_size: if self.unknown_length_on_probe {
                    None
                } else {
                    Some(self.payload.len() as u64)
                },
                supports_ranges: self.supports_ranges,
                suggested_filename: Some("test-artifact.bin".to_string()),
                etag: self.etag.clone(),
                last_modified: None,
                content_type: Some("application/octet-stream".to_string()),
            })
        }

        async fn fetch_range(
            &self,
            _request: &DownloadRequest,
            start: u64,
            end: Option<u64>,
        ) -> Result<ByteStream> {
            let remaining_fails = self.fail_first_n_fetches.load(Ordering::SeqCst);
            if remaining_fails > 0 {
                self.fail_first_n_fetches.fetch_sub(1, Ordering::SeqCst);
                return Err(MayaError::Network("simulated transient fetch error".into()));
            }

            let start_idx = start as usize;
            let end_idx = end
                .map(|e| (e as usize).min(self.payload.len()))
                .unwrap_or(self.payload.len());

            let slice = self.payload[start_idx..end_idx].to_vec();
            let chunk_size = self.chunk_size.max(1);
            let delay = self.delay_per_chunk;
            let fail_mid = self.fail_mid_stream_once.swap(false, Ordering::SeqCst);

            let mut chunks: Vec<Result<Bytes>> = slice
                .chunks(chunk_size)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();

            if fail_mid && chunks.len() > 1 {
                chunks.truncate(1);
                chunks.push(Err(MayaError::Network("mid-stream disconnect".into())));
            }

            let s = stream::iter(chunks).then(move |item| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                item
            });
            Ok(Box::pin(s))
        }
    }

    fn test_payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("maya_engine_test_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn segmented_download_reassembles_and_verifies_checksum() {
        let dir = temp_dir("segmented");
        let payload = test_payload(4096);
        let expected_blake3 = blake3::hash(&payload).to_hex().to_string();

        let client = Arc::new(MockProtocolClient::new(payload.clone(), true));
        let writer_holder: Arc<StdMutex<Option<Arc<MemoryStorageWriter>>>> =
            Arc::new(StdMutex::new(None));
        let holder_clone = Arc::clone(&writer_holder);

        let state_store = Arc::new(MemoryStateStore::default());
        let engine = DownloadEngine::new(client, move |path| {
            let w = Arc::new(MemoryStorageWriter::new(path.to_path_buf()));
            *holder_clone.lock().unwrap() = Some(Arc::clone(&w));
            Ok(w as Arc<dyn StorageWriter>)
        })
        .with_state_store(state_store.clone())
        .with_checkpoint_interval_bytes(512);

        let mut req = DownloadRequest::new("https://example.com/file.bin", &dir);
        req.connections = 4;
        req.min_segment_size = 256;
        req.checksum = Some(ChecksumSpec {
            algorithm: ChecksumAlgorithm::Blake3,
            expected_hex: expected_blake3,
        });

        let (tx, mut rx) = mpsc::channel(256);
        let out_path = engine.download(&req, Some(tx)).await.unwrap();
        assert_eq!(out_path, dir.join("test-artifact.bin"));

        let writer = writer_holder.lock().unwrap().clone().unwrap();
        assert_eq!(writer.data(), payload);
        assert!(state_store.load(&out_path).await.unwrap().is_none());

        let mut saw_probed = false;
        let mut saw_started = false;
        let mut saw_verifying = false;
        let mut saw_completed = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ProgressEvent::Probed { .. } => saw_probed = true,
                ProgressEvent::Started { segment_count, .. } => {
                    saw_started = true;
                    assert_eq!(segment_count, 4);
                }
                ProgressEvent::Verifying {
                    algorithm: ChecksumAlgorithm::Blake3,
                } => saw_verifying = true,
                ProgressEvent::Completed { total_bytes, .. } => {
                    saw_completed = true;
                    assert_eq!(total_bytes, 4096);
                }
                _ => {}
            }
        }
        assert!(saw_probed && saw_started && saw_verifying && saw_completed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retries_with_backoff_and_resets_to_zero_when_no_ranges() {
        let dir = temp_dir("retry_no_range");
        let payload = test_payload(512);

        let client = Arc::new(MockProtocolClient::new(payload.clone(), false));
        client.fail_mid_stream_once.store(true, Ordering::SeqCst);

        let writer_holder: Arc<StdMutex<Option<Arc<MemoryStorageWriter>>>> =
            Arc::new(StdMutex::new(None));
        let holder_clone = Arc::clone(&writer_holder);

        let engine = DownloadEngine::new(client, move |path| {
            let w = Arc::new(MemoryStorageWriter::new(path.to_path_buf()));
            *holder_clone.lock().unwrap() = Some(Arc::clone(&w));
            Ok(w as Arc<dyn StorageWriter>)
        })
        .with_max_retries(2)
        .with_base_retry_delay(Duration::from_millis(5));

        let mut req = DownloadRequest::new("https://example.com/stream.bin", &dir);
        req.connections = 4;

        let _ = engine.download(&req, None).await.unwrap();
        let writer = writer_holder.lock().unwrap().clone().unwrap();
        assert_eq!(writer.data(), payload);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_validates_etag_and_rejects_mismatch() {
        let dir = temp_dir("resume_etag");
        let payload = test_payload(1000);
        let target = dir.join("test-artifact.bin");

        let state_store = Arc::new(MemoryStateStore::default());
        state_store
            .save(
                &target,
                &DownloadState {
                    version: STATE_VERSION,
                    source: "https://example.com/file.bin".to_string(),
                    resolved_url: "https://example.com/file.bin".to_string(),
                    target_path: target.clone(),
                    part_path: part_path_hint(&target),
                    total_size: Some(1000),
                    etag: Some("\"old-etag\"".to_string()),
                    last_modified: None,
                    segments: vec![crate::types::Segment {
                        index: 0,
                        start: 0,
                        end: Some(1000),
                        downloaded: 200,
                        state: crate::types::SegmentState::Active,
                    }],
                },
            )
            .await
            .unwrap();

        let client = Arc::new(MockProtocolClient::new(payload, true));
        let engine = DownloadEngine::new(client, |path| {
            Ok(Arc::new(MemoryStorageWriter::new(path.to_path_buf())) as Arc<dyn StorageWriter>)
        })
        .with_state_store(state_store);

        let req = DownloadRequest::new("https://example.com/file.bin", &dir);
        let err = engine.download(&req, None).await.unwrap_err();
        assert!(matches!(err, MayaError::ResumeMismatch(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enforces_max_download_bytes_on_unknown_length_stream() {
        let dir = temp_dir("max_bytes_stream");
        let payload = test_payload(1024);
        let mut client = MockProtocolClient::new(payload, false);
        client.unknown_length_on_probe = true;
        client.chunk_size = 128;

        let writer_holder: Arc<StdMutex<Option<Arc<MemoryStorageWriter>>>> =
            Arc::new(StdMutex::new(None));
        let holder_clone = Arc::clone(&writer_holder);

        let engine = DownloadEngine::new(Arc::new(client), move |path| {
            let w = Arc::new(MemoryStorageWriter::new(path.to_path_buf()));
            *holder_clone.lock().unwrap() = Some(Arc::clone(&w));
            Ok(w as Arc<dyn StorageWriter>)
        });

        let mut req = DownloadRequest::new("https://example.com/unknown.bin", &dir);
        req.security = SecurityPolicy {
            max_download_bytes: Some(500),
            ..SecurityPolicy::default()
        };

        let err = engine.download(&req, None).await.unwrap_err();
        assert!(matches!(err, MayaError::SecurityViolation(_)));
        assert!(writer_holder
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .was_aborted());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancellation_stops_active_download() {
        let dir = temp_dir("cancel");
        let payload = test_payload(2048);
        let mut client = MockProtocolClient::new(payload, true);
        client.chunk_size = 64;
        client.delay_per_chunk = Duration::from_millis(20);

        let (cancel_tx, cancel_rx) = watch::channel(false);

        let engine = DownloadEngine::new(Arc::new(client), |path| {
            Ok(Arc::new(MemoryStorageWriter::new(path.to_path_buf())) as Arc<dyn StorageWriter>)
        })
        .with_cancel(cancel_rx);

        let req = DownloadRequest::new("https://example.com/slow.bin", &dir);
        let handle = tokio::spawn(async move { engine.download(&req, None).await });

        tokio::time::sleep(Duration::from_millis(35)).await;
        let _ = cancel_tx.send(true);

        let res = handle.await.unwrap();
        assert!(matches!(res, Err(MayaError::Cancelled)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_from_partial_state_downloads_remaining_bytes() {
        let dir = temp_dir("resume_ok");
        let payload = test_payload(1000);
        let target = dir.join("test-artifact.bin");

        // Pre-populate state with first 400 bytes already downloaded.
        let state_store = Arc::new(MemoryStateStore::default());
        state_store
            .save(
                &target,
                &DownloadState {
                    version: STATE_VERSION,
                    source: "https://example.com/file.bin".to_string(),
                    resolved_url: "https://example.com/file.bin".to_string(),
                    target_path: target.clone(),
                    part_path: part_path_hint(&target),
                    total_size: Some(1000),
                    etag: Some("\"v1\"".to_string()),
                    last_modified: None,
                    segments: vec![
                        crate::types::Segment {
                            index: 0,
                            start: 0,
                            end: Some(500),
                            downloaded: 400,
                            state: crate::types::SegmentState::Active,
                        },
                        crate::types::Segment {
                            index: 1,
                            start: 500,
                            end: Some(1000),
                            downloaded: 0,
                            state: crate::types::SegmentState::Failed,
                        },
                    ],
                },
            )
            .await
            .unwrap();

        let pre_filled = payload[..400].to_vec();
        let writer_holder: Arc<StdMutex<Option<Arc<MemoryStorageWriter>>>> =
            Arc::new(StdMutex::new(None));
        let holder_clone = Arc::clone(&writer_holder);

        let client = Arc::new(MockProtocolClient::new(payload.clone(), true));
        let engine = DownloadEngine::new(client, move |path| {
            let w = Arc::new(MemoryStorageWriter::new(path.to_path_buf()));
            w.buffer.lock().unwrap().extend_from_slice(&pre_filled);
            *holder_clone.lock().unwrap() = Some(Arc::clone(&w));
            Ok(w as Arc<dyn StorageWriter>)
        })
        .with_state_store(state_store.clone());

        let req = DownloadRequest::new("https://example.com/file.bin", &dir);
        let out = engine.download(&req, None).await.unwrap();
        assert_eq!(out, target);
        assert_eq!(
            writer_holder.lock().unwrap().as_ref().unwrap().data(),
            payload
        );
        assert!(state_store.load(&target).await.unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancellation_during_rate_limit_sleep_is_immediate() {
        let dir = temp_dir("cancel_throttled");
        let payload = test_payload(1024);
        let mut client = MockProtocolClient::new(payload, false);
        client.chunk_size = 512;

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let engine = DownloadEngine::new(Arc::new(client), |path| {
            Ok(Arc::new(MemoryStorageWriter::new(path.to_path_buf())) as Arc<dyn StorageWriter>)
        })
        .with_cancel(cancel_rx);

        let mut req = DownloadRequest::new("https://example.com/throttled.bin", &dir);
        req.connections = 1;
        // 10 B/s: chunk 1 consumes initial 10B capacity and sleeps ~50s without cancellation.
        req.max_bytes_per_sec = Some(10);

        let start = std::time::Instant::now();
        let handle = tokio::spawn(async move { engine.download(&req, None).await });

        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = cancel_tx.send(true);

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cancellation must interrupt limiter sleep promptly")
            .unwrap();
        assert!(matches!(res, Err(MayaError::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(1));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
