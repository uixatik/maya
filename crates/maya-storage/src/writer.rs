use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bytes::Bytes;

use maya_core::{ChecksumSpec, MayaError, Result, StorageWriter};

use crate::journal::{part_path_for, state_path_for, StateJournal};
use crate::verify::verify_file;

/// Thread-safe, zero-merge sparse disk writer backed by `<target>.maya.part`.
#[derive(Debug, Clone)]
pub struct SparseDiskWriter {
    target_path: PathBuf,
    part_path: PathBuf,
    state_path: PathBuf,
    file: Arc<RwLock<Option<File>>>,
}

impl SparseDiskWriter {
    /// Construct a new writer targeting `target_path` with staged `<target>.maya.part`
    /// and `<target>.maya.state` sibling paths.
    pub fn new(target_path: PathBuf) -> Self {
        let part_path = part_path_for(&target_path);
        let state_path = state_path_for(&target_path);
        Self {
            target_path,
            part_path,
            state_path,
            file: Arc::new(RwLock::new(None)),
        }
    }

    /// Final destination file path.
    pub fn target_path(&self) -> &Path {
        &self.target_path
    }

    /// Staged partial file path (`<target>.maya.part`).
    pub fn part_path(&self) -> &Path {
        &self.part_path
    }

    /// Resume journal file path (`<target>.maya.state`).
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    fn close_handle_blocking(file_lock: &RwLock<Option<File>>, flush_first: bool) -> Result<bool> {
        let mut guard = file_lock
            .write()
            .map_err(|_| MayaError::Storage("writer lock poisoned".to_string()))?;
        match guard.take() {
            Some(file) => {
                if flush_first {
                    file.sync_data()?;
                }
                drop(file);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[async_trait]
impl StorageWriter for SparseDiskWriter {
    async fn prepare(&self, total_size: Option<u64>) -> Result<()> {
        let target_path = self.target_path.clone();
        let part_path = self.part_path.clone();
        let file_lock = Arc::clone(&self.file);

        tokio::task::spawn_blocking(move || {
            if let Some(parent) = target_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }

            let mut guard = file_lock
                .write()
                .map_err(|_| MayaError::Storage("writer lock poisoned".to_string()))?;

            // Drop any previously held handle before reopening.
            let _ = guard.take();

            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&part_path)?;

            if let Some(len) = total_size {
                let current_len = file.metadata()?.len();
                if current_len != len {
                    file.set_len(len)?;
                }
            }

            *guard = Some(file);
            Ok(())
        })
        .await
        .map_err(|err| MayaError::Storage(format!("prepare task failed: {err}")))?
    }

    async fn write_at(&self, offset: u64, data: Bytes) -> Result<()> {
        let file_lock = Arc::clone(&self.file);

        tokio::task::spawn_blocking(move || {
            let guard = file_lock
                .read()
                .map_err(|_| MayaError::Storage("writer lock poisoned".to_string()))?;
            let file = guard.as_ref().ok_or_else(|| {
                MayaError::Storage("writer is not prepared or already closed".to_string())
            })?;

            write_all_at(file, offset, &data)
        })
        .await
        .map_err(|err| MayaError::Storage(format!("write_at task failed: {err}")))?
    }

    async fn flush(&self) -> Result<()> {
        let file_lock = Arc::clone(&self.file);

        tokio::task::spawn_blocking(move || {
            let guard = file_lock
                .read()
                .map_err(|_| MayaError::Storage("writer lock poisoned".to_string()))?;
            let file = guard.as_ref().ok_or_else(|| {
                MayaError::Storage("writer is not prepared or already closed".to_string())
            })?;
            file.sync_data()?;
            Ok(())
        })
        .await
        .map_err(|err| MayaError::Storage(format!("flush task failed: {err}")))?
    }

    async fn finalize(&self, checksum: Option<&ChecksumSpec>) -> Result<PathBuf> {
        let file_lock = Arc::clone(&self.file);
        let closed =
            tokio::task::spawn_blocking(move || Self::close_handle_blocking(&file_lock, true))
                .await
                .map_err(|err| {
                    MayaError::Storage(format!("finalize flush/close task failed: {err}"))
                })??;

        if !closed && !self.part_path.exists() {
            return Err(MayaError::Storage(
                "cannot finalize: writer is not prepared and part file is missing".to_string(),
            ));
        }

        if let Some(spec) = checksum {
            verify_file(&self.part_path, spec).await?;
        }

        let part_path = self.part_path.clone();
        let target_path = self.target_path.clone();
        let state_path = self.state_path.clone();

        tokio::task::spawn_blocking(move || {
            if let Err(err) = std::fs::rename(&part_path, &target_path) {
                if target_path.exists() && target_path.is_file() {
                    std::fs::remove_file(&target_path)?;
                    std::fs::rename(&part_path, &target_path)?;
                } else {
                    return Err(err.into());
                }
            }
            StateJournal::remove(&state_path)?;
            Ok(target_path)
        })
        .await
        .map_err(|err| MayaError::Storage(format!("finalize rename task failed: {err}")))?
    }

    async fn abort(&self) -> Result<()> {
        let file_lock = Arc::clone(&self.file);
        let part_path = self.part_path.clone();
        let state_path = self.state_path.clone();

        tokio::task::spawn_blocking(move || {
            let _ = Self::close_handle_blocking(&file_lock, false)?;
            remove_if_exists(&part_path)?;
            StateJournal::remove(&state_path)?;
            Ok(())
        })
        .await
        .map_err(|err| MayaError::Storage(format!("abort task failed: {err}")))?
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn write_all_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
    let mut written = 0usize;
    while written < data.len() {
        let write_offset = offset
            .checked_add(written as u64)
            .ok_or_else(|| MayaError::Storage("write offset overflow".to_string()))?;
        let n = platform_write_at(file, write_offset, &data[written..])?;
        if n == 0 {
            return Err(MayaError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "positional write returned 0 bytes",
            )));
        }
        written += n;
    }
    Ok(())
}

#[cfg(windows)]
fn platform_write_at(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(unix)]
fn platform_write_at(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(not(any(windows, unix)))]
fn platform_write_at(_file: &File, _offset: u64, _buf: &[u8]) -> std::io::Result<usize> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "positional file writes are only supported on Windows and Unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maya_core::{ChecksumAlgorithm, DownloadState, Segment, SegmentState};
    use sha2::Digest;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "maya-storage-writer-{prefix}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn concurrent_out_of_order_writes_and_zero_merge_finalize() {
        let dir = TempDirGuard::new("concurrent");
        let target = dir.path.join("nested").join("payload.bin");
        let writer = Arc::new(SparseDiskWriter::new(target.clone()));

        assert_eq!(writer.target_path(), target.as_path());
        assert_eq!(writer.part_path(), part_path_for(&target).as_path());
        assert_eq!(writer.state_path(), state_path_for(&target).as_path());

        let chunk_count = 16usize;
        let chunk_size = 8192usize;
        let total_size = (chunk_count * chunk_size) as u64;

        writer
            .prepare(Some(total_size))
            .await
            .expect("prepare sparse part file");
        assert!(writer.part_path().exists());

        // Write a journal file to ensure finalize removes it.
        let dummy_state = DownloadState {
            version: 1,
            source: "https://example.com/payload.bin".to_string(),
            resolved_url: "https://example.com/payload.bin".to_string(),
            target_path: target.clone(),
            part_path: writer.part_path().to_path_buf(),
            total_size: Some(total_size),
            etag: None,
            last_modified: None,
            segments: vec![Segment {
                index: 0,
                start: 0,
                end: Some(total_size),
                downloaded: total_size,
                state: SegmentState::Completed,
            }],
        };
        StateJournal::save_atomic(writer.state_path(), &dummy_state).expect("save journal");
        assert!(writer.state_path().exists());

        // Spawn concurrent out-of-order tasks (reverse/interleaved order).
        let mut expected = vec![0u8; total_size as usize];
        let mut handles = Vec::with_capacity(chunk_count);
        for idx in (0..chunk_count).rev() {
            let pattern = ((idx * 37 + 11) & 0xff) as u8;
            let chunk = vec![pattern; chunk_size];
            let start = idx * chunk_size;
            expected[start..start + chunk_size].copy_from_slice(&chunk);

            let writer_ref = Arc::clone(&writer);
            handles.push(tokio::spawn(async move {
                writer_ref
                    .write_at(start as u64, Bytes::from(chunk))
                    .await
                    .expect("concurrent write_at");
            }));
        }

        for handle in handles {
            handle.await.expect("join write task");
        }
        writer.flush().await.expect("flush writes");

        let checksum = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Blake3,
            expected_hex: blake3::hash(&expected).to_hex().to_string(),
        };

        let final_path = writer
            .finalize(Some(&checksum))
            .await
            .expect("finalize with blake3 verification");
        assert_eq!(final_path, target);
        assert!(target.exists());
        assert!(!writer.part_path().exists());
        assert!(!writer.state_path().exists());

        let actual = std::fs::read(&target).expect("read finalized file");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn prepare_preserves_existing_resume_bytes_when_length_matches() {
        let dir = TempDirGuard::new("resume");
        let target = dir.path.join("resume.bin");
        let writer = SparseDiskWriter::new(target.clone());

        writer.prepare(Some(1024)).await.expect("initial prepare");
        writer
            .write_at(0, Bytes::from_static(b"SEGMENT-ZERO"))
            .await
            .expect("write initial segment");
        writer.flush().await.expect("flush");
        drop(writer);

        let resumed = SparseDiskWriter::new(target.clone());
        resumed.prepare(Some(1024)).await.expect("resume prepare");
        resumed
            .write_at(512, Bytes::from_static(b"SEGMENT-ONE"))
            .await
            .expect("write second segment");
        resumed.finalize(None).await.expect("finalize resumed");

        let bytes = std::fs::read(&target).expect("read target");
        assert_eq!(bytes.len(), 1024);
        assert_eq!(&bytes[..12], b"SEGMENT-ZERO");
        assert_eq!(&bytes[512..523], b"SEGMENT-ONE");
    }

    #[tokio::test]
    async fn finalize_rejects_checksum_mismatch_without_renaming_and_overwrites_existing_on_success(
    ) {
        let dir = TempDirGuard::new("finalize-edge");
        let target = dir.path.join("artifact.bin");
        std::fs::write(&target, b"old content").expect("write existing target");

        let writer = SparseDiskWriter::new(target.clone());
        let payload = b"new verified content";
        writer
            .prepare(Some(payload.len() as u64))
            .await
            .expect("prepare");
        writer
            .write_at(0, Bytes::from_static(payload))
            .await
            .expect("write");

        let bad_spec = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha256,
            expected_hex: "f".repeat(64),
        };
        let err = writer
            .finalize(Some(&bad_spec))
            .await
            .expect_err("checksum mismatch must fail finalize");
        assert!(matches!(err, MayaError::ChecksumMismatch { .. }));
        assert!(writer.part_path().exists());
        assert_eq!(std::fs::read(&target).expect("read old"), b"old content");

        // Now finalize with the real SHA-256 digest (verifying handle was closed cleanly).
        let digest = sha2::Sha256::digest(payload);
        let mut good_hex = String::with_capacity(64);
        for b in digest {
            good_hex.push_str(&format!("{b:02x}"));
        }
        let good_spec = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha256,
            expected_hex: good_hex,
        };
        writer
            .finalize(Some(&good_spec))
            .await
            .expect("finalize with valid sha256");
        assert!(!writer.part_path().exists());
        assert_eq!(std::fs::read(&target).expect("read new"), payload);
    }

    #[tokio::test]
    async fn abort_closes_handle_and_removes_part_and_state() {
        let dir = TempDirGuard::new("abort");
        let target = dir.path.join("aborted.bin");
        let writer = SparseDiskWriter::new(target.clone());

        writer.prepare(Some(4096)).await.expect("prepare");
        writer
            .write_at(0, Bytes::from_static(b"partial"))
            .await
            .expect("write");

        let state = DownloadState {
            version: 1,
            source: "https://example.com/aborted.bin".to_string(),
            resolved_url: "https://example.com/aborted.bin".to_string(),
            target_path: target.clone(),
            part_path: writer.part_path().to_path_buf(),
            total_size: Some(4096),
            etag: None,
            last_modified: None,
            segments: Vec::new(),
        };
        StateJournal::save_atomic(writer.state_path(), &state).expect("save state");

        writer.abort().await.expect("abort cleanup");
        assert!(!writer.part_path().exists());
        assert!(!writer.state_path().exists());
        assert!(!target.exists());
    }
}
