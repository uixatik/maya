//! Browser Native Messaging manifest generation and registry installation.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::NmhError;

pub const HOST_NAME: &str = "com.maya.downloader";
pub const HOST_DESCRIPTION: &str = "Maya Downloader Native Messaging Host";
pub const FIREFOX_EXTENSION_ID: &str = "maya@downloader.local";
pub const DEFAULT_CHROME_EXTENSION_ID: &str =
    "chrome-extension://knldjmfmopnppmplflldnbgahglgbeoh/";

/// Native messaging manifest for Chromium-based browsers (Chrome, Edge, Brave, Opera, Vivaldi).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChromeManifest {
    pub name: String,
    pub description: String,
    pub path: String,
    pub r#type: String,
    pub allowed_origins: Vec<String>,
}

/// Native messaging manifest for Mozilla Firefox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirefoxManifest {
    pub name: String,
    pub description: String,
    pub path: String,
    pub r#type: String,
    pub allowed_extensions: Vec<String>,
}

/// Report returned by manifest installation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestInstallReport {
    pub host_binary: PathBuf,
    pub manifest_files: Vec<PathBuf>,
    pub registry_keys: Vec<String>,
    pub success: bool,
    pub message: String,
}

/// Generate a Chrome/Edge manifest JSON structure.
pub fn generate_chrome_manifest(host_binary: &Path, allowed_origins: &[String]) -> ChromeManifest {
    let mut origins = allowed_origins.to_vec();
    if origins.is_empty() {
        origins.push(DEFAULT_CHROME_EXTENSION_ID.to_string());
    }
    ChromeManifest {
        name: HOST_NAME.to_string(),
        description: HOST_DESCRIPTION.to_string(),
        path: host_binary.to_string_lossy().to_string(),
        r#type: "stdio".to_string(),
        allowed_origins: origins,
    }
}

/// Generate a Mozilla Firefox manifest JSON structure.
pub fn generate_firefox_manifest(
    host_binary: &Path,
    allowed_extensions: &[String],
) -> FirefoxManifest {
    let mut extensions = allowed_extensions.to_vec();
    if extensions.is_empty() {
        extensions.push(FIREFOX_EXTENSION_ID.to_string());
    }
    FirefoxManifest {
        name: HOST_NAME.to_string(),
        description: HOST_DESCRIPTION.to_string(),
        path: host_binary.to_string_lossy().to_string(),
        r#type: "stdio".to_string(),
        allowed_extensions: extensions,
    }
}

/// Resolve the current executable path or fallback to a standard executable name.
pub fn resolve_host_binary(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    std::env::current_exe().unwrap_or_else(|_| {
        #[cfg(windows)]
        {
            PathBuf::from("maya-nmh.exe")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("maya-nmh")
        }
    })
}

/// Return the base configuration directory for Maya NMH files (`~/.maya/nmh`).
pub fn default_nmh_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".maya").join("nmh")
    } else {
        PathBuf::from(".maya").join("nmh")
    }
}

/// Install the Native Messaging manifests for Chrome, Edge, and Firefox.
///
/// On Windows:
/// - Writes manifest JSONs to `~/.maya/nmh/`
/// - Registers keys in `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.maya.downloader`,
///   `HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.maya.downloader`, and
///   `HKCU\Software\Mozilla\NativeMessagingHosts\com.maya.downloader`.
///
/// On Unix (Linux / macOS):
/// - Writes manifest JSONs directly to the respective browser config directories.
pub fn install_manifests(
    explicit_binary: Option<&Path>,
    custom_origins: &[String],
) -> Result<ManifestInstallReport, NmhError> {
    let host_binary = resolve_host_binary(explicit_binary);
    let mut report = ManifestInstallReport {
        host_binary: host_binary.clone(),
        ..Default::default()
    };

    let nmh_dir = default_nmh_dir();
    std::fs::create_dir_all(&nmh_dir)?;

    let chrome_manifest = generate_chrome_manifest(&host_binary, custom_origins);
    let firefox_manifest = generate_firefox_manifest(&host_binary, &[]);

    let chrome_json_path = nmh_dir.join("com.maya.downloader.chrome.json");
    let firefox_json_path = nmh_dir.join("com.maya.downloader.firefox.json");

    let chrome_str = serde_json::to_string_pretty(&chrome_manifest)?;
    let firefox_str = serde_json::to_string_pretty(&firefox_manifest)?;

    std::fs::write(&chrome_json_path, chrome_str)?;
    std::fs::write(&firefox_json_path, firefox_str)?;

    report.manifest_files.push(chrome_json_path.clone());
    report.manifest_files.push(firefox_json_path.clone());

    #[cfg(windows)]
    {
        let registry_targets = [
            (
                r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.maya.downloader",
                &chrome_json_path,
            ),
            (
                r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.maya.downloader",
                &chrome_json_path,
            ),
            (
                r"HKCU\Software\Mozilla\NativeMessagingHosts\com.maya.downloader",
                &firefox_json_path,
            ),
        ];

        for (reg_key, json_file) in registry_targets {
            let status = Command::new("reg")
                .args([
                    "add",
                    reg_key,
                    "/ve",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &json_file.to_string_lossy(),
                    "/f",
                ])
                .status();

            match status {
                Ok(s) if s.success() => {
                    report.registry_keys.push(reg_key.to_string());
                }
                Ok(s) => {
                    eprintln!("Warning: reg add failed for {reg_key} with exit code {s}");
                }
                Err(e) => {
                    eprintln!("Warning: failed to invoke reg for {reg_key}: {e}");
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        // Linux / macOS standard directories
        if let Some(home) = dirs::home_dir() {
            let mut dirs_chrome = Vec::new();
            let mut dirs_edge = Vec::new();
            let mut dirs_firefox = Vec::new();

            #[cfg(target_os = "macos")]
            {
                dirs_chrome.push(
                    home.join("Library/Application Support/Google/Chrome/NativeMessagingHosts"),
                );
                dirs_edge.push(
                    home.join("Library/Application Support/Microsoft Edge/NativeMessagingHosts"),
                );
                dirs_firefox
                    .push(home.join("Library/Application Support/Mozilla/NativeMessagingHosts"));
            }

            #[cfg(not(target_os = "macos"))]
            {
                dirs_chrome.push(home.join(".config/google-chrome/NativeMessagingHosts"));
                dirs_chrome.push(home.join(".config/chromium/NativeMessagingHosts"));
                dirs_edge.push(home.join(".config/microsoft-edge/NativeMessagingHosts"));
                dirs_firefox.push(home.join(".mozilla/native-messaging-hosts"));
            }

            for dir in dirs_chrome.into_iter().chain(dirs_edge.into_iter()) {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    eprintln!("Warning: failed to create dir {}: {e}", dir.display());
                    continue;
                }
                let target = dir.join("com.maya.downloader.json");
                let _ = std::fs::write(&target, serde_json::to_string_pretty(&chrome_manifest)?);
                report.manifest_files.push(target);
            }

            for dir in dirs_firefox {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    eprintln!("Warning: failed to create dir {}: {e}", dir.display());
                    continue;
                }
                let target = dir.join("com.maya.downloader.json");
                let _ = std::fs::write(&target, serde_json::to_string_pretty(&firefox_manifest)?);
                report.manifest_files.push(target);
            }
        }
    }

    report.success = true;
    report.message = "Native messaging manifests installed successfully".to_string();
    Ok(report)
}

/// Uninstall native messaging manifests and registry entries.
pub fn uninstall_manifests() -> Result<(), NmhError> {
    #[cfg(windows)]
    {
        let registry_targets = [
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.maya.downloader",
            r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.maya.downloader",
            r"HKCU\Software\Mozilla\NativeMessagingHosts\com.maya.downloader",
        ];

        for reg_key in registry_targets {
            let _ = Command::new("reg").args(["delete", reg_key, "/f"]).status();
        }
    }

    let nmh_dir = default_nmh_dir();
    let chrome_json = nmh_dir.join("com.maya.downloader.chrome.json");
    let firefox_json = nmh_dir.join("com.maya.downloader.firefox.json");

    if chrome_json.exists() {
        let _ = std::fs::remove_file(chrome_json);
    }
    if firefox_json.exists() {
        let _ = std::fs::remove_file(firefox_json);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_chrome_manifest() {
        let bin = Path::new("C:/test/maya-nmh.exe");
        let manifest =
            generate_chrome_manifest(bin, &["chrome-extension://my-ext-id/".to_string()]);
        assert_eq!(manifest.name, "com.maya.downloader");
        assert_eq!(manifest.r#type, "stdio");
        assert_eq!(manifest.path, "C:/test/maya-nmh.exe");
        assert_eq!(
            manifest.allowed_origins,
            vec!["chrome-extension://my-ext-id/"]
        );

        let json = serde_json::to_string(&manifest).expect("serialize");
        assert!(json.contains("com.maya.downloader"));
        assert!(json.contains("allowed_origins"));

        let default_manifest = generate_chrome_manifest(bin, &[]);
        assert_eq!(
            default_manifest.allowed_origins,
            vec![DEFAULT_CHROME_EXTENSION_ID]
        );
        assert!(!default_manifest
            .allowed_origins
            .contains(&"chrome-extension://*".to_string()));
    }

    #[test]
    fn test_generate_firefox_manifest() {
        let bin = Path::new("/usr/local/bin/maya-nmh");
        let manifest = generate_firefox_manifest(bin, &[]);
        assert_eq!(manifest.name, "com.maya.downloader");
        assert_eq!(manifest.r#type, "stdio");
        assert_eq!(manifest.path, "/usr/local/bin/maya-nmh");
        assert_eq!(manifest.allowed_extensions, vec![FIREFOX_EXTENSION_ID]);

        let json = serde_json::to_string(&manifest).expect("serialize");
        assert!(json.contains("allowed_extensions"));
        assert!(json.contains("maya@downloader.local"));
    }
}
