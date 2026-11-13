use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use maya_core::security::sanitize_filename;
use maya_core::{DownloadEngine, DownloadRequest, DownloadState, MayaError, Result, StateStore};
use maya_protocols::{HttpProtocolClient, MediaExtractor, SelectedStreams, TorrentClient};
use maya_storage::{part_path_for, state_path_for, SparseDiskWriter, StateJournal};
use tokio::sync::{mpsc, watch};

use crate::progress::ProgressRenderer;

/// [`StateStore`] implementation backed by [`StateJournal`] atomic `.maya.state` files.
#[derive(Debug, Default, Clone, Copy)]
pub struct JournalStateStore;

impl StateStore for JournalStateStore {
    fn load<'life0, 'life1, 'async_trait>(
        &'life0 self,
        target_path: &'life1 Path,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DownloadState>>> + Send + 'async_trait>>
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
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
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
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
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

/// Discover `ffmpeg` executable on WinGet links, PATH, or well-known locations.
pub fn find_ffmpeg() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let p = PathBuf::from(local_app_data).join(r"Microsoft\WinGet\Links\ffmpeg.exe");
            if p.exists() {
                return Some(p);
            }
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = if cfg!(windows) {
                dir.join("ffmpeg.exe")
            } else {
                dir.join("ffmpeg")
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    #[cfg(windows)]
    {
        let common = [
            r"C:\ffmpeg\bin\ffmpeg.exe",
            r"C:\Program Files\ffmpeg\bin\ffmpeg.exe",
        ];
        for p in common {
            let path = PathBuf::from(p);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    // Fallback to bare "ffmpeg" / "ffmpeg.exe"
    Some(PathBuf::from(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }))
}

fn apply_media_headers(
    req: &mut DownloadRequest,
    info: &maya_protocols::MediaStreamInfo,
    stream: &maya_protocols::MediaFormatStream,
) {
    for (k, v) in &info.http_headers {
        if !req.headers.iter().any(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
            req.headers.push((k.clone(), v.clone()));
        }
    }
    for (k, v) in &stream.http_headers {
        if !req.headers.iter().any(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
            req.headers.push((k.clone(), v.clone()));
        }
    }
    if let Some(ua) = &info.user_agent {
        if !req
            .headers
            .iter()
            .any(|(hk, _)| hk.eq_ignore_ascii_case("user-agent"))
        {
            req.headers.push(("User-Agent".to_string(), ua.clone()));
        }
    }
    if let Some(ref_url) = &info.referer {
        if !req
            .headers
            .iter()
            .any(|(hk, _)| hk.eq_ignore_ascii_case("referer"))
        {
            req.headers.push(("Referer".to_string(), ref_url.clone()));
        }
    }
    if let Some(cookie) = &info.cookies {
        if !req
            .headers
            .iter()
            .any(|(hk, _)| hk.eq_ignore_ascii_case("cookie"))
        {
            req.headers.push(("Cookie".to_string(), cookie.clone()));
        }
    }
}

/// Download an HTTP resource with Maya's high-speed segmented engine.
pub async fn run_engine_download(
    req: DownloadRequest,
    json_mode: bool,
    cancel_rx: watch::Receiver<bool>,
) -> Result<PathBuf> {
    let (events_tx, events_rx) = mpsc::channel(256);
    let render_task = tokio::spawn(ProgressRenderer::run(events_rx, json_mode));

    let engine = DownloadEngine::new(Arc::new(HttpProtocolClient::new()), |p| {
        Ok(Arc::new(SparseDiskWriter::new(p.to_path_buf())))
    })
    .with_state_store(Arc::new(JournalStateStore))
    .with_cancel(cancel_rx);

    let res = engine.download(&req, Some(events_tx)).await;
    let _ = render_task.await;
    res
}

/// Handle YouTube & streaming platform URL extraction, downloading, and optional ffmpeg muxing.
pub async fn run_media_download(
    req: DownloadRequest,
    json_mode: bool,
    cancel_rx: watch::Receiver<bool>,
) -> Result<PathBuf> {
    if !json_mode {
        println!("[maya] Detected media/streaming URL, extracting streams via yt-dlp...");
    }

    let extractor = MediaExtractor::new();
    let info = extractor.extract_info(&req.source).await?;

    let sanitized_title = sanitize_filename(&info.title);
    let base_name = if sanitized_title.trim().is_empty() {
        "media".to_string()
    } else {
        sanitized_title
    };

    match &info.streams {
        SelectedStreams::Muxed(stream) => {
            let mut direct_req = req.clone();
            direct_req.source = stream.url.clone();
            let ext = stream.ext.as_deref().unwrap_or(if info.ext.is_empty() {
                "mp4"
            } else {
                &info.ext
            });
            direct_req.filename = Some(
                req.filename
                    .clone()
                    .unwrap_or_else(|| format!("{base_name}.{ext}")),
            );
            apply_media_headers(&mut direct_req, &info, stream);
            run_engine_download(direct_req, json_mode, cancel_rx).await
        }
        SelectedStreams::Separate { video, audio } => {
            if let Some(audio_stream) = audio {
                let ffmpeg = find_ffmpeg().ok_or_else(|| {
                    MayaError::Unsupported(
                        "ffmpeg is required to mux separate video and audio streams, but was not found on PATH or WinGet links".to_string(),
                    )
                })?;

                let final_filename = req
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("{base_name}.mp4"));
                let final_output = if req.security.enforce_safe_paths {
                    maya_core::security::safe_target_path(&req.output_dir, &final_filename)?
                } else {
                    req.output_dir.join(&final_filename)
                };

                let video_ext = video.ext.as_deref().unwrap_or("mp4");
                let audio_ext = audio_stream.ext.as_deref().unwrap_or("m4a");
                let video_track_name = format!("{base_name}.video_track.{video_ext}");
                let audio_track_name = format!("{base_name}.audio_track.{audio_ext}");

                let mut video_req = req.clone();
                video_req.source = video.url.clone();
                video_req.filename = Some(video_track_name);
                apply_media_headers(&mut video_req, &info, video);

                let mut audio_req = req.clone();
                audio_req.source = audio_stream.url.clone();
                audio_req.filename = Some(audio_track_name);
                apply_media_headers(&mut audio_req, &info, audio_stream);

                if !json_mode {
                    println!("[maya] Downloading video stream...");
                }
                let video_path =
                    run_engine_download(video_req, json_mode, cancel_rx.clone()).await?;

                if !json_mode {
                    println!("[maya] Downloading audio stream...");
                }
                let audio_path =
                    run_engine_download(audio_req, json_mode, cancel_rx.clone()).await?;

                if !json_mode {
                    println!("[maya] Muxing video and audio streams with ffmpeg...");
                }

                let mut cmd = tokio::process::Command::new(&ffmpeg);
                cmd.args(["-y", "-i"])
                    .arg(&video_path)
                    .arg("-i")
                    .arg(&audio_path)
                    .args(["-c", "copy"])
                    .arg(&final_output);

                let status = cmd
                    .status()
                    .await
                    .map_err(|e| MayaError::Protocol(format!("failed to execute ffmpeg: {e}")))?;

                if !status.success() {
                    return Err(MayaError::Protocol(format!(
                        "ffmpeg muxing failed with status: {status}"
                    )));
                }

                // Clean up intermediate tracks
                let _ = tokio::fs::remove_file(&video_path).await;
                let _ = tokio::fs::remove_file(&audio_path).await;

                if !json_mode {
                    println!(
                        "[maya] Successfully muxed media to {}",
                        final_output.display()
                    );
                }

                Ok(final_output)
            } else {
                let mut direct_req = req.clone();
                direct_req.source = video.url.clone();
                let ext = video.ext.as_deref().unwrap_or(if info.ext.is_empty() {
                    "mp4"
                } else {
                    &info.ext
                });
                direct_req.filename = Some(
                    req.filename
                        .clone()
                        .unwrap_or_else(|| format!("{base_name}.{ext}")),
                );
                apply_media_headers(&mut direct_req, &info, video);
                run_engine_download(direct_req, json_mode, cancel_rx).await
            }
        }
    }
}

/// Execute a download request with Ctrl+C cancellation wiring and progress rendering.
#[allow(dead_code)]
pub async fn run_download(
    req: DownloadRequest,
    seed_torrent: bool,
    json_mode: bool,
) -> Result<PathBuf> {
    run_download_ext(req, seed_torrent, json_mode, false).await
}

/// Execute a download request with optional media extraction flag.
pub async fn run_download_ext(
    req: DownloadRequest,
    seed_torrent: bool,
    json_mode: bool,
    extract_media: bool,
) -> Result<PathBuf> {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel_tx.send(true);
        }
    });

    let result =
        run_download_with_cancel_ext(req, seed_torrent, json_mode, extract_media, cancel_rx).await;
    signal_task.abort();
    result
}

/// Execute a download request using an explicit cancellation receiver.
#[allow(dead_code)]
pub async fn run_download_with_cancel(
    req: DownloadRequest,
    seed_torrent: bool,
    json_mode: bool,
    cancel_rx: watch::Receiver<bool>,
) -> Result<PathBuf> {
    run_download_with_cancel_ext(req, seed_torrent, json_mode, false, cancel_rx).await
}

/// Execute a download request with explicit cancellation receiver and media extraction flag.
pub async fn run_download_with_cancel_ext(
    req: DownloadRequest,
    seed_torrent: bool,
    json_mode: bool,
    extract_media: bool,
    cancel_rx: watch::Receiver<bool>,
) -> Result<PathBuf> {
    if extract_media || MediaExtractor::is_media_url(&req.source) {
        return run_media_download(req, json_mode, cancel_rx).await;
    }

    let torrent_client = TorrentClient::new().with_seeding(seed_torrent);
    if torrent_client.supports(&req.source) {
        let (events_tx, events_rx) = mpsc::channel(256);
        let render_task = tokio::spawn(ProgressRenderer::run(events_rx, json_mode));
        let res = torrent_client.download(&req, events_tx, cancel_rx).await;
        let _ = render_task.await;
        res
    } else {
        run_engine_download(req, json_mode, cancel_rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maya_core::{Segment, SegmentState, STATE_VERSION};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "maya-cli-runner-{prefix}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn journal_state_store_roundtrips_and_removes() {
        let dir = TempDirGuard::new("state-store");
        let target = dir.path.join("file.bin");
        let store = JournalStateStore;

        assert_eq!(store.load(&target).await.expect("load missing"), None);

        let state = DownloadState {
            version: STATE_VERSION,
            source: "https://example.com/file.bin".to_string(),
            resolved_url: "https://example.com/file.bin".to_string(),
            target_path: target.clone(),
            part_path: maya_storage::part_path_for(&target),
            total_size: Some(1024),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
            segments: vec![Segment {
                index: 0,
                start: 0,
                end: Some(1024),
                downloaded: 256,
                state: SegmentState::Active,
            }],
        };

        store.save(&target, &state).await.expect("save state");
        assert!(state_path_for(&target).exists());
        std::fs::write(part_path_for(&target), vec![0u8; 1024]).expect("create part file");
        assert_eq!(store.load(&target).await.expect("load saved"), Some(state));

        store.remove(&target).await.expect("remove state");
        assert!(!state_path_for(&target).exists());
        assert_eq!(store.load(&target).await.expect("load removed"), None);
    }

    #[tokio::test]
    async fn journal_state_store_rejects_resume_when_part_file_missing() {
        let dir = TempDirGuard::new("missing-part");
        let target = dir.path.join("file.bin");
        let store = JournalStateStore;

        let state = DownloadState {
            version: STATE_VERSION,
            source: "https://example.com/file.bin".to_string(),
            resolved_url: "https://example.com/file.bin".to_string(),
            target_path: target.clone(),
            part_path: part_path_for(&target),
            total_size: Some(1024),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
            segments: vec![Segment {
                index: 0,
                start: 0,
                end: Some(1024),
                downloaded: 256,
                state: SegmentState::Active,
            }],
        };

        store.save(&target, &state).await.expect("save state");
        assert!(!part_path_for(&target).exists());

        let err = store
            .load(&target)
            .await
            .expect_err("missing .maya.part with downloaded > 0 must fail");
        assert!(matches!(err, MayaError::ResumeMismatch(_)));
    }

    #[tokio::test]
    async fn run_download_respects_pre_triggered_cancellation() {
        let dir = TempDirGuard::new("cancel");
        let req = DownloadRequest::new("https://example.com/file.bin", &dir.path);
        let (tx, rx) = watch::channel(true);
        let _keep_tx = tx;

        let err = run_download_with_cancel(req, false, false, rx)
            .await
            .expect_err("cancelled download must fail");
        assert!(matches!(err, MayaError::Cancelled));
    }

    #[test]
    fn test_ffmpeg_discovery() {
        let ffmpeg = find_ffmpeg();
        assert!(ffmpeg.is_some());
    }

    #[test]
    fn test_media_url_detection() {
        assert!(MediaExtractor::is_media_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(MediaExtractor::is_media_url("https://youtu.be/dQw4w9WgXcQ"));
        assert!(MediaExtractor::is_media_url(
            "https://example.com/stream.m3u8"
        ));
        assert!(!MediaExtractor::is_media_url(
            "https://example.com/file.zip"
        ));
    }
}
