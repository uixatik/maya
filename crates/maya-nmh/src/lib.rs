//! Maya Native Messaging Host (`maya-nmh`) library.
//!
//! Provides protocol framing, extension manifest generation and registry registration,
//! and request dispatching to `mayad` via `maya-ipc`.

pub mod handler;
pub mod manifest;
pub mod protocol;

use thiserror::Error;

/// Errors produced during Native Messaging Host operations.
#[derive(Debug, Error)]
pub enum NmhError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IPC error: {0}")]
    Ipc(#[from] maya_ipc::IpcError),

    #[error("Message too large: {0} bytes (maximum {1} bytes)")]
    MessageTooLarge(usize, usize),

    #[error("Unexpected EOF while reading message framing")]
    UnexpectedEof,

    #[error("Invalid request payload: {0}")]
    InvalidRequest(String),

    #[error("Manifest error: {0}")]
    Manifest(String),
}

pub use handler::handle_message;
pub use manifest::{
    generate_chrome_manifest, generate_firefox_manifest, install_manifests, resolve_host_binary,
    uninstall_manifests, ChromeManifest, FirefoxManifest, ManifestInstallReport,
    FIREFOX_EXTENSION_ID, HOST_NAME,
};
pub use protocol::{
    read_message, read_message_sync, write_message, write_message_sync, MAX_MESSAGE_SIZE,
};
