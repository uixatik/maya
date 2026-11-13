use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use maya_core::{
    ChecksumAlgorithm, ChecksumSpec, DownloadEngine, DownloadRequest, DownloadState, MayaError,
    ProgressEvent, Result, SecurityPolicy, StateStore,
};
use maya_protocols::HttpProtocolClient;
use maya_storage::{part_path_for, state_path_for, SparseDiskWriter, StateJournal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("maya-cli-e2e-{prefix}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create e2e temp dir");
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Test `StateStore` mirroring `runner::JournalStateStore` using `StateJournal`.
#[derive(Debug, Default, Clone, Copy)]
struct JournalStateStore;

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

#[derive(Clone, Copy)]
struct ServerOptions {
    supports_ranges: bool,
    slow_first_range: bool,
    drip_chunk_size: Option<usize>,
    drip_delay: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            supports_ranges: true,
            slow_first_range: false,
            drip_chunk_size: None,
            drip_delay: Duration::ZERO,
        }
    }
}

struct MockHttpServer {
    base_url: String,
    request_count: Arc<AtomicUsize>,
    bytes_sent: Arc<AtomicUsize>,
    first_chunk_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
}

impl MockHttpServer {
    async fn start(payload: Vec<u8>, options: ServerOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mock HTTP listener");
        let addr = listener.local_addr().expect("listener local addr");
        let payload = Arc::new(payload);
        let request_count = Arc::new(AtomicUsize::new(0));
        let bytes_sent = Arc::new(AtomicUsize::new(0));
        let first_chunk_notify = Arc::new(Notify::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let req_count_clone = Arc::clone(&request_count);
        let bytes_sent_clone = Arc::clone(&bytes_sent);
        let notify_clone = Arc::clone(&first_chunk_notify);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((mut socket, _)) = accepted else {
                            break;
                        };
                        let payload = Arc::clone(&payload);
                        let req_count = Arc::clone(&req_count_clone);
                        let bytes_sent = Arc::clone(&bytes_sent_clone);
                        let notify = Arc::clone(&notify_clone);

                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 8192];
                            let n = match socket.read(&mut buf).await {
                                Ok(n) if n > 0 => n,
                                _ => return,
                            };
                            req_count.fetch_add(1, Ordering::SeqCst);

                            let raw_req = String::from_utf8_lossy(&buf[..n]);
                            let mut lines = raw_req.lines();
                            let request_line = lines.next().unwrap_or_default();
                            let method = request_line.split_whitespace().next().unwrap_or("GET");

                            let range_header = lines.find_map(|line| {
                                let (name, val) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("range")
                                    .then(|| val.trim().to_string())
                            });

                            let total = payload.len();
                            if method.eq_ignore_ascii_case("HEAD") {
                                let accept_ranges = if options.supports_ranges {
                                    "Accept-Ranges: bytes\r\n"
                                } else {
                                    "Accept-Ranges: none\r\n"
                                };
                                let headers = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n{accept_ranges}ETag: \"maya-e2e-v1\"\r\nConnection: close\r\n\r\n"
                                );
                                let _ = socket.write_all(headers.as_bytes()).await;
                                let _ = socket.shutdown().await;
                                return;
                            }

                            if options.supports_ranges {
                                if let Some(range_val) = range_header {
                                    if let Some((start, end_inclusive)) = parse_byte_range(&range_val, total) {
                                        let slice = &payload[start..=end_inclusive];
                                        let len = slice.len();
                                        let headers = format!(
                                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end_inclusive}/{total}\r\nContent-Length: {len}\r\nETag: \"maya-e2e-v1\"\r\nConnection: close\r\n\r\n"
                                        );
                                        if socket.write_all(headers.as_bytes()).await.is_err() {
                                            return;
                                        }

                                        if options.slow_first_range && start == 0 {
                                            tokio::time::sleep(Duration::from_millis(80)).await;
                                        }

                                        if let Some(chunk_size) = options.drip_chunk_size {
                                            for chunk in slice.chunks(chunk_size) {
                                                if socket.write_all(chunk).await.is_err() {
                                                    return;
                                                }
                                                bytes_sent.fetch_add(chunk.len(), Ordering::SeqCst);
                                                notify.notify_waiters();
                                                if !options.drip_delay.is_zero() {
                                                    tokio::time::sleep(options.drip_delay).await;
                                                }
                                            }
                                        } else if socket.write_all(slice).await.is_ok() {
                                            bytes_sent.fetch_add(slice.len(), Ordering::SeqCst);
                                            notify.notify_waiters();
                                        }
                                        let _ = socket.shutdown().await;
                                        return;
                                    }
                                }
                            }

                            let headers = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"maya-e2e-v1\"\r\nConnection: close\r\n\r\n"
                            );
                            if socket.write_all(headers.as_bytes()).await.is_err() {
                                return;
                            }
                            if let Some(chunk_size) = options.drip_chunk_size {
                                for chunk in payload.chunks(chunk_size) {
                                    if socket.write_all(chunk).await.is_err() {
                                        return;
                                    }
                                    bytes_sent.fetch_add(chunk.len(), Ordering::SeqCst);
                                    notify.notify_waiters();
                                    if !options.drip_delay.is_zero() {
                                        tokio::time::sleep(options.drip_delay).await;
                                    }
                                }
                            } else if socket.write_all(&payload).await.is_ok() {
                                bytes_sent.fetch_add(payload.len(), Ordering::SeqCst);
                                notify.notify_waiters();
                            }
                            let _ = socket.shutdown().await;
                        });
                    }
                }
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            request_count,
            bytes_sent,
            first_chunk_notify,
            shutdown_tx,
        }
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

fn parse_byte_range(header: &str, total: usize) -> Option<(usize, usize)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start_str, end_str) = spec.split_once('-')?;
    let start: usize = start_str.trim().parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_str.trim().is_empty() {
        total - 1
    } else {
        end_str.trim().parse::<usize>().ok()?.min(total - 1)
    };
    (start <= end).then_some((start, end))
}

fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i.wrapping_mul(31).wrapping_add(17)) & 0xff) as u8)
        .collect()
}

fn build_engine(cancel_rx: Option<watch::Receiver<bool>>) -> DownloadEngine {
    let mut engine = DownloadEngine::new(Arc::new(HttpProtocolClient::new()), |p| {
        Ok(Arc::new(SparseDiskWriter::new(p.to_path_buf())))
    })
    .with_state_store(Arc::new(JournalStateStore))
    .with_checkpoint_interval_bytes(16 * 1024);

    if let Some(rx) = cancel_rx {
        engine = engine.with_cancel(rx);
    }
    engine
}

/// Pure-Rust SHA-256 helper so integration tests do not require extra `Cargo.toml` dev-deps.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[tokio::test]
async fn e2e_multi_connection_range_and_work_stealing() {
    let dir = TempDirGuard::new("range-steal");
    let payload = deterministic_payload(512 * 1024);
    let server = MockHttpServer::start(
        payload.clone(),
        ServerOptions {
            supports_ranges: true,
            slow_first_range: true,
            ..Default::default()
        },
    )
    .await;

    let mut req = DownloadRequest::new(format!("{}/payload.bin", server.base_url), &dir.path);
    req.connections = 4;
    req.min_segment_size = 64 * 1024;
    req.security.allow_private_ips = true;

    let out_path = build_engine(None)
        .download(&req, None)
        .await
        .expect("multi-connection range download must succeed");

    assert_eq!(out_path, dir.path.join("payload.bin"));
    assert_eq!(std::fs::read(&out_path).expect("read output"), payload);
    assert!(!part_path_for(&out_path).exists());
    assert!(!state_path_for(&out_path).exists());
    // HEAD + at least 4 ranged GET requests (plus potential work-stealing splits)
    assert!(server.request_count.load(Ordering::SeqCst) >= 5);
}

#[tokio::test]
async fn e2e_cancel_mid_transfer_creates_checkpoint_and_resumes_exact() {
    let dir = TempDirGuard::new("cancel-resume");
    let payload = deterministic_payload(256 * 1024);
    let slow_server = MockHttpServer::start(
        payload.clone(),
        ServerOptions {
            supports_ranges: true,
            drip_chunk_size: Some(16 * 1024),
            drip_delay: Duration::from_millis(25),
            ..Default::default()
        },
    )
    .await;

    let mut req = DownloadRequest::new(format!("{}/resume.bin", slow_server.base_url), &dir.path);
    req.connections = 2;
    req.min_segment_size = 64 * 1024;
    req.security.allow_private_ips = true;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let engine = build_engine(Some(cancel_rx));

    let req_clone = req.clone();
    let dl_task = tokio::spawn(async move { engine.download(&req_clone, None).await });

    // Wait until the server has streamed enough bytes for at least one checkpointed chunk.
    while slow_server.bytes_sent.load(Ordering::SeqCst) < 32 * 1024 {
        slow_server.first_chunk_notify.notified().await;
    }
    // Give the worker a brief moment to write and checkpoint to disk before cancelling.
    tokio::time::sleep(Duration::from_millis(40)).await;
    cancel_tx.send(true).expect("send cancel");

    let err = dl_task
        .await
        .expect("join cancelled task")
        .expect_err("cancelled download must return Err");
    assert!(
        matches!(err, MayaError::Cancelled),
        "expected MayaError::Cancelled, got {err:?}"
    );

    let target_path = dir.path.join("resume.bin");
    assert!(!target_path.exists());
    assert!(
        part_path_for(&target_path).exists(),
        ".maya.part must exist after mid-transfer cancellation"
    );
    assert!(
        state_path_for(&target_path).exists(),
        ".maya.state must exist after mid-transfer cancellation"
    );

    // Resume against a fast server serving the identical resource + ETag.
    let fast_server = MockHttpServer::start(payload.clone(), ServerOptions::default()).await;
    req.source = format!("{}/resume.bin", fast_server.base_url);

    let resumed_path = build_engine(None)
        .download(&req, None)
        .await
        .expect("resumed download must succeed");

    assert_eq!(resumed_path, target_path);
    assert_eq!(std::fs::read(&resumed_path).expect("read resumed"), payload);
    assert!(!part_path_for(&resumed_path).exists());
    assert!(!state_path_for(&resumed_path).exists());
}

#[tokio::test]
async fn e2e_non_range_200_ok_fallback() {
    let dir = TempDirGuard::new("non-range");
    let payload = deterministic_payload(96 * 1024);
    let server = MockHttpServer::start(
        payload.clone(),
        ServerOptions {
            supports_ranges: false,
            ..Default::default()
        },
    )
    .await;

    let mut req = DownloadRequest::new(format!("{}/stream.bin", server.base_url), &dir.path);
    req.connections = 4;
    req.security.allow_private_ips = true;

    let out_path = build_engine(None)
        .download(&req, None)
        .await
        .expect("non-range fallback must succeed");

    assert_eq!(std::fs::read(&out_path).expect("read output"), payload);
    assert!(!part_path_for(&out_path).exists());
    assert!(!state_path_for(&out_path).exists());
}

#[tokio::test]
async fn e2e_ssrf_blocks_loopback_under_default_policy() {
    let dir = TempDirGuard::new("ssrf-block");
    let payload = b"secret-internal-data".to_vec();
    let server = MockHttpServer::start(payload, ServerOptions::default()).await;

    let mut req = DownloadRequest::new(format!("{}/secret.bin", server.base_url), &dir.path);
    req.security = SecurityPolicy::default();
    assert!(!req.security.allow_private_ips);

    let err = build_engine(None)
        .download(&req, None)
        .await
        .expect_err("loopback must be blocked when allow_private_ips is false");
    assert!(
        matches!(err, MayaError::SecurityViolation(_)),
        "expected SecurityViolation, got {err:?}"
    );
    assert_eq!(
        server.request_count.load(Ordering::SeqCst),
        0,
        "server must receive zero requests when SSRF check blocks 127.0.0.1"
    );
}

#[tokio::test]
async fn e2e_checksum_sha256_and_blake3_verify_and_reject_mismatch() {
    let dir = TempDirGuard::new("checksums");
    let payload = deterministic_payload(48 * 1024);
    let server = MockHttpServer::start(payload.clone(), ServerOptions::default()).await;

    // 1. SHA-256 valid pass
    let sha_hex = sha256_hex(&payload);
    let mut req_sha_ok = DownloadRequest::new(format!("{}/ok-sha.bin", server.base_url), &dir.path);
    req_sha_ok.security.allow_private_ips = true;
    req_sha_ok.checksum = Some(ChecksumSpec {
        algorithm: ChecksumAlgorithm::Sha256,
        expected_hex: sha_hex,
    });
    let sha_out = build_engine(None)
        .download(&req_sha_ok, None)
        .await
        .expect("valid sha256 must pass");
    assert_eq!(std::fs::read(&sha_out).expect("read sha output"), payload);

    // 2. SHA-256 mismatch fail
    let mut req_sha_bad =
        DownloadRequest::new(format!("{}/bad-sha.bin", server.base_url), &dir.path);
    req_sha_bad.security.allow_private_ips = true;
    req_sha_bad.checksum = Some(ChecksumSpec {
        algorithm: ChecksumAlgorithm::Sha256,
        expected_hex: "0".repeat(64),
    });
    let err_sha = build_engine(None)
        .download(&req_sha_bad, None)
        .await
        .expect_err("wrong sha256 must fail");
    assert!(matches!(err_sha, MayaError::ChecksumMismatch { .. }));
    assert!(!dir.path.join("bad-sha.bin").exists());

    // 3. BLAKE3 mismatch returns actual digest, then re-verify with exact BLAKE3 digest passes.
    let mut req_b3_bad =
        DownloadRequest::new(format!("{}/b3-check.bin", server.base_url), &dir.path);
    req_b3_bad.security.allow_private_ips = true;
    req_b3_bad.checksum = Some(ChecksumSpec {
        algorithm: ChecksumAlgorithm::Blake3,
        expected_hex: "f".repeat(64),
    });
    let err_b3 = build_engine(None)
        .download(&req_b3_bad, None)
        .await
        .expect_err("wrong blake3 must fail");
    let actual_b3 = match err_b3 {
        MayaError::ChecksumMismatch { actual, .. } => actual,
        other => panic!("expected ChecksumMismatch, got {other:?}"),
    };
    assert!(!dir.path.join("b3-check.bin").exists());

    let mut req_b3_ok = DownloadRequest::new(format!("{}/b3-pass.bin", server.base_url), &dir.path);
    req_b3_ok.security.allow_private_ips = true;
    req_b3_ok.checksum = Some(ChecksumSpec {
        algorithm: ChecksumAlgorithm::Blake3,
        expected_hex: actual_b3,
    });
    let b3_out = build_engine(None)
        .download(&req_b3_ok, None)
        .await
        .expect("valid blake3 must pass");
    assert_eq!(std::fs::read(&b3_out).expect("read b3 output"), payload);
}

#[tokio::test]
async fn e2e_compiled_binary_version_doctor_and_get_json() {
    let bin = env!("CARGO_BIN_EXE_maya");
    let dir = TempDirGuard::new("cli-bin");

    // 1. `maya version`
    let ver_out = Command::new(bin)
        .arg("version")
        .output()
        .expect("run maya version");
    assert!(ver_out.status.success(), "maya version failed: {ver_out:?}");
    let ver_stdout = String::from_utf8_lossy(&ver_out.stdout);
    assert!(ver_stdout.contains("maya 0.0.1 (v0.0.01)"));
    assert!(ver_stdout.contains("HTTP/1.1 + H2"));
    assert!(ver_stdout.contains("rustls"));

    // 2. `maya doctor -d <dir>`
    let doc_out = Command::new(bin)
        .args(["doctor", "-d"])
        .arg(&dir.path)
        .output()
        .expect("run maya doctor");
    assert!(doc_out.status.success(), "maya doctor failed: {doc_out:?}");
    let doc_stdout = String::from_utf8_lossy(&doc_out.stdout);
    assert!(doc_stdout.contains("Maya Doctor"));
    assert!(doc_stdout.contains("Download  : OK"));
    assert!(doc_stdout.contains("Torrent   : enabled"));

    // 3. `maya get <url> -d <dir> -o artifact.bin -c 4 --allow-private-ips --verify sha256:<hex> --json`
    let payload = deterministic_payload(300 * 1024);
    let expected_sha = sha256_hex(&payload);
    let server = MockHttpServer::start(payload.clone(), ServerOptions::default()).await;
    let url = format!("{}/artifact.bin", server.base_url);

    let bin_path = bin.to_string();
    let out_dir = dir.path.clone();
    let get_out = tokio::task::spawn_blocking(move || {
        Command::new(&bin_path)
            .args(["get", &url, "-d"])
            .arg(&out_dir)
            .args([
                "-o",
                "artifact.bin",
                "-c",
                "4",
                "--allow-private-ips",
                "--verify",
                &format!("sha256:{expected_sha}"),
                "--json",
            ])
            .output()
            .expect("run maya get --json")
    })
    .await
    .expect("join maya get command");

    assert!(
        get_out.status.success(),
        "maya get failed: stdout={} stderr={}",
        String::from_utf8_lossy(&get_out.stdout),
        String::from_utf8_lossy(&get_out.stderr)
    );

    let stdout_str = String::from_utf8(get_out.stdout).expect("utf8 ndjson stdout");
    let events: Vec<ProgressEvent> = stdout_str
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<ProgressEvent>(line)
                .unwrap_or_else(|err| panic!("invalid NDJSON ProgressEvent line `{line}`: {err}"))
        })
        .collect();

    assert!(events
        .iter()
        .any(|e| matches!(e, ProgressEvent::Probed { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, ProgressEvent::Started { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, ProgressEvent::SegmentProgress { .. })));
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::Verifying {
            algorithm: ChecksumAlgorithm::Sha256
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::Completed { total_bytes, .. } if *total_bytes == payload.len() as u64
    )));

    let target = dir.path.join("artifact.bin");
    assert_eq!(
        std::fs::read(&target).expect("read downloaded file"),
        payload
    );
    assert!(!part_path_for(&target).exists());
    assert!(!state_path_for(&target).exists());
}
