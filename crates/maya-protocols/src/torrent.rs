use std::path::{Path, PathBuf};

use maya_core::{DownloadRequest, MayaError, ProgressEvent, Result};
use tokio::sync::{mpsc, watch};

/// BitTorrent and Magnet URI download client backed by `librqbit` (when `torrent` feature is enabled).
#[derive(Debug, Clone)]
pub struct TorrentClient {
    seed_after_completion: bool,
}

impl Default for TorrentClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentClient {
    /// Construct a [`TorrentClient`] with post-completion seeding disabled by default.
    pub fn new() -> Self {
        Self {
            seed_after_completion: false,
        }
    }

    /// Configure whether the client should continue seeding after the download completes.
    pub fn with_seeding(mut self, seed_after_completion: bool) -> Self {
        self.seed_after_completion = seed_after_completion;
        self
    }

    /// Return `true` if `source` is a `magnet:?` URI or a `.torrent` file path.
    pub fn supports(&self, source: &str) -> bool {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return false;
        }
        if trimmed
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("magnet:?"))
        {
            return true;
        }
        // Do not claim HTTP/HTTPS URLs that happen to end in `.torrent` — those are fetched via HTTP first.
        if trimmed
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            || trimmed
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        {
            return false;
        }
        Path::new(trimmed)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
    }

    /// Download a magnet URI or `.torrent` file into `request.output_dir`, emitting progress events.
    pub async fn download(
        &self,
        request: &DownloadRequest,
        events: mpsc::Sender<ProgressEvent>,
        cancel: watch::Receiver<bool>,
    ) -> Result<PathBuf> {
        self.download_impl(request, events, cancel).await
    }
}

#[cfg(feature = "torrent")]
mod imp {
    use super::*;
    use std::num::NonZeroU32;
    use std::time::Duration;

    use librqbit::limits::LimitsConfig;
    use librqbit::{
        AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, Session,
        SessionOptions,
    };

    impl TorrentClient {
        pub(super) async fn download_impl(
            &self,
            request: &DownloadRequest,
            events: mpsc::Sender<ProgressEvent>,
            mut cancel: watch::Receiver<bool>,
        ) -> Result<PathBuf> {
            if !self.supports(&request.source) {
                return Err(MayaError::UnsupportedScheme(request.source.clone()));
            }
            if *cancel.borrow() {
                return Err(MayaError::Cancelled);
            }

            tokio::fs::create_dir_all(&request.output_dir).await?;

            let download_bps = request
                .max_bytes_per_sec
                .filter(|&bps| bps > 0)
                .and_then(|bps| NonZeroU32::new(u32::try_from(bps).unwrap_or(u32::MAX)));

            let session_opts = SessionOptions {
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                persistence: None,
                ratelimits: LimitsConfig {
                    download_bps,
                    upload_bps: None,
                },
                ..Default::default()
            };

            let session = Session::new_with_opts(request.output_dir.clone(), session_opts)
                .await
                .map_err(|err| {
                    MayaError::Network(format!("failed to start torrent session: {err}"))
                })?;

            let source = request.source.trim();
            let add_torrent = if source
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("magnet:?"))
            {
                AddTorrent::from_url(source)
            } else {
                let bytes = tokio::fs::read(source).await?;
                AddTorrent::from_bytes(bytes)
            };

            let add_opts = AddTorrentOptions {
                overwrite: request.overwrite,
                ..Default::default()
            };

            let added = session
                .add_torrent(add_torrent, Some(add_opts))
                .await
                .map_err(|err| MayaError::Protocol(format!("failed to add torrent: {err}")))?;

            let handle = match added {
                AddTorrentResponse::Added(_, handle)
                | AddTorrentResponse::AlreadyManaged(_, handle) => handle,
                AddTorrentResponse::ListOnly(_) => {
                    session.stop().await;
                    return Err(MayaError::Protocol(
                        "torrent added in list-only mode unexpectedly".to_string(),
                    ));
                }
            };

            let mut started_emitted = false;
            let mut last_downloaded: u64 = 0;

            loop {
                if *cancel.borrow() {
                    session.stop().await;
                    let _ = events
                        .send(ProgressEvent::Failed {
                            error: "cancelled".to_string(),
                        })
                        .await;
                    return Err(MayaError::Cancelled);
                }

                let stats = handle.stats();
                let total = stats.total_bytes;
                let downloaded = stats.progress_bytes;

                if let Some(max_bytes) = request.security.max_download_bytes {
                    if total > max_bytes || downloaded > max_bytes {
                        session.stop().await;
                        return Err(MayaError::SecurityViolation(format!(
                            "torrent size exceeds max_download_bytes ({max_bytes})"
                        )));
                    }
                }

                if !started_emitted && total > 0 {
                    started_emitted = true;
                    let _ = events
                        .send(ProgressEvent::Started {
                            total_size: Some(total),
                            segment_count: 1,
                        })
                        .await;
                }

                if downloaded > last_downloaded {
                    let delta = downloaded - last_downloaded;
                    last_downloaded = downloaded;
                    let _ = events
                        .send(ProgressEvent::SegmentProgress {
                            segment_index: 0,
                            bytes_delta: delta,
                            total_downloaded: downloaded,
                            total_size: (total > 0).then_some(total),
                        })
                        .await;
                }

                if stats.finished {
                    let _ = events
                        .send(ProgressEvent::SegmentCompleted { segment_index: 0 })
                        .await;

                    let output_path = resolve_torrent_output_path(
                        &request.output_dir,
                        handle.name().as_deref(),
                        request.filename.as_deref(),
                    );

                    let _ = events
                        .send(ProgressEvent::Completed {
                            output_path: output_path.clone(),
                            total_bytes: total.max(downloaded),
                        })
                        .await;

                    if self.seed_after_completion {
                        while !*cancel.borrow() {
                            if cancel.changed().await.is_err() {
                                break;
                            }
                        }
                    } else {
                        let _ = session.pause(&handle).await;
                    }

                    session.stop().await;
                    return Ok(output_path);
                }

                if let Some(err) = stats.error {
                    session.stop().await;
                    let _ = events
                        .send(ProgressEvent::Failed { error: err.clone() })
                        .await;
                    return Err(MayaError::Protocol(format!("torrent error: {err}")));
                }

                tokio::select! {
                    _ = cancel.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        }
    }

    fn resolve_torrent_output_path(
        output_dir: &Path,
        torrent_name: Option<&str>,
        explicit_filename: Option<&str>,
    ) -> PathBuf {
        if let Some(name) = explicit_filename.or(torrent_name) {
            output_dir.join(maya_core::security::sanitize_filename(name))
        } else {
            output_dir.to_path_buf()
        }
    }
}

#[cfg(not(feature = "torrent"))]
mod imp {
    use super::*;

    impl TorrentClient {
        pub(super) async fn download_impl(
            &self,
            _request: &DownloadRequest,
            _events: mpsc::Sender<ProgressEvent>,
            _cancel: watch::Receiver<bool>,
        ) -> Result<PathBuf> {
            Err(MayaError::Unsupported(
                "BitTorrent/Magnet support is disabled (enable `torrent` feature)".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_magnet_and_torrent_sources() {
        let client = TorrentClient::new();
        assert!(client.supports("magnet:?xt=urn:btih:0123456789abcdef"));
        assert!(client.supports("MAGNET:?xt=urn:btih:0123456789abcdef"));
        assert!(client.supports("ubuntu-24.04.torrent"));
        assert!(client.supports(r"C:\Downloads\distro.TORRENT"));
        assert!(client.supports("/tmp/archive.torrent"));

        assert!(!client.supports(""));
        assert!(!client.supports("http://example.com/file.torrent"));
        assert!(!client.supports("https://example.com/file.torrent"));
        assert!(!client.supports("https://example.com/file.iso"));
        assert!(!client.supports(".torrent"));
    }

    #[cfg(not(feature = "torrent"))]
    #[tokio::test]
    async fn stub_returns_unsupported_when_feature_disabled() {
        let client = TorrentClient::new();
        let req = DownloadRequest::new("magnet:?xt=urn:btih:abc", PathBuf::from("."));
        let (tx, _rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let err = client.download(&req, tx, cancel_rx).await.unwrap_err();
        assert!(matches!(err, MayaError::Unsupported(_)));
    }
}
