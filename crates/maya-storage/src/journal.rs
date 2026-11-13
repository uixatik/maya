use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use maya_core::{DownloadState, Result};

/// Return the staged partial download path (`<target>.maya.part`).
pub fn part_path_for(target: &Path) -> PathBuf {
    append_suffix(target, ".maya.part")
}

/// Return the resume state journal path (`<target>.maya.state`).
pub fn state_path_for(target: &Path) -> PathBuf {
    append_suffix(target, ".maya.state")
}

fn append_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(base.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Atomic JSON persistence for `.maya.state` resume checkpoints.
pub struct StateJournal;

impl StateJournal {
    /// Load a previously saved [`DownloadState`], returning `Ok(None)` if the file does not exist.
    pub fn load(path: &Path) -> Result<Option<DownloadState>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state = serde_json::from_slice(&bytes)?;
                Ok(Some(state))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Persist `state` atomically by writing `<path>.tmp`, calling `sync_all()`,
    /// and renaming over `path`.
    pub fn save_atomic(path: &Path, state: &DownloadState) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let payload = serde_json::to_vec_pretty(state)?;
        let tmp_path = append_suffix(path, ".tmp");

        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err.into());
        }

        Ok(())
    }

    /// Remove the journal file at `path`, succeeding if it does not exist.
    pub fn remove(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maya_core::{Segment, SegmentState};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "maya-storage-journal-{prefix}-{}-{id}",
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

    fn sample_state(dir: &Path, downloaded: u64) -> DownloadState {
        let target = dir.join("archive.tar.gz");
        DownloadState {
            version: 1,
            source: "https://example.com/archive.tar.gz".to_string(),
            resolved_url: "https://cdn.example.com/archive.tar.gz".to_string(),
            part_path: part_path_for(&target),
            target_path: target,
            total_size: Some(4096),
            etag: Some("\"etag-1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            segments: vec![
                Segment {
                    index: 0,
                    start: 0,
                    end: Some(2048),
                    downloaded,
                    state: SegmentState::Active,
                },
                Segment {
                    index: 1,
                    start: 2048,
                    end: Some(4096),
                    downloaded: 0,
                    state: SegmentState::Pending,
                },
            ],
        }
    }

    #[test]
    fn path_helpers_append_suffixes_preserving_extensions() {
        let target = Path::new("downloads/archive.tar.gz");
        assert_eq!(
            part_path_for(target),
            PathBuf::from("downloads/archive.tar.gz.maya.part")
        );
        assert_eq!(
            state_path_for(target),
            PathBuf::from("downloads/archive.tar.gz.maya.state")
        );

        let no_ext = Path::new("downloads/README");
        assert_eq!(
            part_path_for(no_ext),
            PathBuf::from("downloads/README.maya.part")
        );
        assert_eq!(
            state_path_for(no_ext),
            PathBuf::from("downloads/README.maya.state")
        );
    }

    #[test]
    fn load_missing_returns_none_and_save_atomic_overwrites() {
        let dir = TempDirGuard::new("atomic");
        let state_path = dir.path.join("nested").join("file.bin.maya.state");

        assert_eq!(StateJournal::load(&state_path).expect("load missing"), None);

        let first = sample_state(&dir.path, 512);
        StateJournal::save_atomic(&state_path, &first).expect("save initial state");
        assert!(!append_suffix(&state_path, ".tmp").exists());
        assert_eq!(
            StateJournal::load(&state_path).expect("load first"),
            Some(first)
        );

        let second = sample_state(&dir.path, 1536);
        StateJournal::save_atomic(&state_path, &second).expect("overwrite state atomically");
        assert!(!append_suffix(&state_path, ".tmp").exists());
        assert_eq!(
            StateJournal::load(&state_path).expect("load second"),
            Some(second)
        );

        StateJournal::remove(&state_path).expect("remove existing");
        assert!(!state_path.exists());
        StateJournal::remove(&state_path).expect("remove idempotent");
    }
}
