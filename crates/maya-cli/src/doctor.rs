use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Result of checking write permissions for a download target directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCheck {
    Writable { path: PathBuf },
    MissingDirParentWritable { path: PathBuf, ancestor: PathBuf },
    NotWritable { path: PathBuf, reason: String },
}

impl WriteCheck {
    /// Return `true` when the path (or its nearest existing ancestor) is writable.
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            Self::Writable { .. } | Self::MissingDirParentWritable { .. }
        )
    }
}

/// Optional external tool discovery entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatus {
    pub name: &'static str,
    pub path: Option<PathBuf>,
}

/// Structured diagnostic report produced by `maya doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub os: &'static str,
    pub arch: &'static str,
    pub family: &'static str,
    pub write_check: WriteCheck,
    pub torrent_supported: bool,
    pub tools: Vec<ToolStatus>,
}

/// Inspect the local environment without spawning any shell or child processes.
pub fn collect_report(download_dir: &Path) -> DoctorReport {
    let path_env = std::env::var_os("PATH");
    let pathext_env = std::env::var_os("PATHEXT");
    let search_dirs: Vec<PathBuf> = path_env
        .as_deref()
        .map(|raw| std::env::split_paths(raw).collect())
        .unwrap_or_default();

    let tools = ["ffmpeg", "yt-dlp"]
        .into_iter()
        .map(|name| ToolStatus {
            name,
            path: find_tool_in_dirs(name, &search_dirs, pathext_env.as_deref()),
        })
        .collect();

    // `maya-cli` enables `maya-protocols`'s default `torrent` feature (`librqbit`).
    let torrent_supported = true;

    DoctorReport {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        family: std::env::consts::FAMILY,
        write_check: check_writable_dir(download_dir),
        torrent_supported,
        tools,
    }
}

/// Print the `maya doctor` report to stdout and return `true` if all core checks pass.
pub fn run_doctor(download_dir: &Path) -> bool {
    let report = collect_report(download_dir);

    println!("Maya Doctor");
    println!(
        "  OS / Arch : {}/{} ({})",
        report.os, report.arch, report.family
    );
    match &report.write_check {
        WriteCheck::Writable { path } => {
            println!("  Download  : OK (writable: {})", path.display());
        }
        WriteCheck::MissingDirParentWritable { path, ancestor } => {
            println!(
                "  Download  : OK (directory {} does not exist yet; parent {} is writable)",
                path.display(),
                ancestor.display()
            );
        }
        WriteCheck::NotWritable { path, reason } => {
            println!("  Download  : FAIL ({}: {reason})", path.display());
        }
    }
    println!(
        "  Torrent   : {}",
        if report.torrent_supported {
            "enabled (maya-protocols torrent feature / librqbit)"
        } else {
            "disabled"
        }
    );
    println!("  Optional tools (PATH):");
    for tool in &report.tools {
        match &tool.path {
            Some(path) => println!("    - {:<8}: found ({})", tool.name, path.display()),
            None => println!("    - {:<8}: not found", tool.name),
        }
    }

    report.write_check.is_ok()
}

/// Verify whether `dir` (or its nearest existing ancestor) can accept new files.
pub fn check_writable_dir(dir: &Path) -> WriteCheck {
    let resolved = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    };

    if resolved.exists() {
        if !resolved.is_dir() {
            return WriteCheck::NotWritable {
                path: resolved,
                reason: "path exists and is not a directory".to_string(),
            };
        }
        return match probe_directory_write(&resolved) {
            Ok(()) => WriteCheck::Writable { path: resolved },
            Err(err) => WriteCheck::NotWritable {
                path: resolved,
                reason: err.to_string(),
            },
        };
    }

    let abs_or_rel = if resolved.is_absolute() {
        resolved.clone()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&resolved)
    } else {
        resolved.clone()
    };

    let mut cursor = abs_or_rel.parent();
    while let Some(ancestor) = cursor {
        if ancestor.exists() {
            if !ancestor.is_dir() {
                return WriteCheck::NotWritable {
                    path: resolved,
                    reason: format!("ancestor `{}` is not a directory", ancestor.display()),
                };
            }
            return match probe_directory_write(ancestor) {
                Ok(()) => WriteCheck::MissingDirParentWritable {
                    path: resolved,
                    ancestor: ancestor.to_path_buf(),
                },
                Err(err) => WriteCheck::NotWritable {
                    path: resolved,
                    reason: format!("parent `{}` is not writable: {err}", ancestor.display()),
                },
            };
        }
        cursor = ancestor.parent();
    }

    WriteCheck::NotWritable {
        path: resolved,
        reason: "no existing parent directory found".to_string(),
    }
}

fn probe_directory_write(dir: &Path) -> std::io::Result<()> {
    let seq = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let probe_name = format!(".maya-doctor-probe-{}-{seq}", std::process::id());
    let probe_path = dir.join(probe_name);

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)?;
    drop(file);
    let _ = std::fs::remove_file(&probe_path);
    Ok(())
}

/// Safely probe `dirs` for an executable named `tool` without spawning any shell or subprocess.
pub fn find_tool_in_dirs(
    tool: &str,
    dirs: &[PathBuf],
    pathext_env: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if tool.is_empty() {
        return None;
    }

    let candidates = candidate_filenames(tool, pathext_env);
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in &candidates {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn candidate_filenames(tool: &str, pathext_env: Option<&std::ffi::OsStr>) -> Vec<OsString> {
    if !cfg!(windows) {
        return vec![OsString::from(tool)];
    }

    let mut names = Vec::new();
    let has_ext = Path::new(tool).extension().is_some();
    if has_ext {
        names.push(OsString::from(tool));
        return names;
    }

    let exts: Vec<String> = match pathext_env {
        Some(raw) => raw
            .to_string_lossy()
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect(),
        None => vec![
            ".exe".to_string(),
            ".cmd".to_string(),
            ".bat".to_string(),
            ".com".to_string(),
        ],
    };

    for ext in exts {
        let mut os = OsString::from(tool);
        os.push(&ext);
        names.push(os);
    }
    names.push(OsString::from(tool));
    names
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let seq = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "maya-cli-doctor-{prefix}-{}-{seq}",
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

    #[test]
    fn checks_writable_existing_and_missing_dirs() {
        let dir = TempDirGuard::new("writable");
        assert!(matches!(
            check_writable_dir(&dir.path),
            WriteCheck::Writable { .. }
        ));

        let nested_missing = dir.path.join("does-not-exist").join("sub");
        assert!(matches!(
            check_writable_dir(&nested_missing),
            WriteCheck::MissingDirParentWritable { .. }
        ));

        let file_path = dir.path.join("not-a-dir");
        std::fs::write(&file_path, b"x").expect("create file");
        assert!(matches!(
            check_writable_dir(&file_path),
            WriteCheck::NotWritable { .. }
        ));
    }

    #[test]
    fn probes_path_directories_safely() {
        let dir = TempDirGuard::new("path-probe");
        let empty_dir = dir.path.join("empty");
        let bin_dir = dir.path.join("bin");
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();

        let tool_name = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let fake_tool = bin_dir.join(tool_name);
        std::fs::write(&fake_tool, b"stub").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_tool).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_tool, perms).unwrap();
        }

        let found = find_tool_in_dirs(
            "ffmpeg",
            &[empty_dir, bin_dir],
            Some(OsStr::new(".COM;.EXE;.BAT")),
        );
        assert_eq!(found, Some(fake_tool));
    }
}
