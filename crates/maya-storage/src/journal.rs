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
