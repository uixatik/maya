use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Pin-boxed stream of byte chunks yielded by a protocol handler.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// Supported download transport protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolKind {
    Http,
    Https,
    Torrent,
    Magnet,
}

/// Supported cryptographic digest algorithms for post-download verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksumAlgorithm {
    Sha256,
    Blake3,
    Md5,
    Sha512,
}

impl ChecksumAlgorithm {
    /// Canonical lowercase name for display and serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
            Self::Md5 => "md5",
            Self::Sha512 => "sha512",
        }
    }
}

/// Expected checksum specification used to verify a completed download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksumSpec {
    pub algorithm: ChecksumAlgorithm,
    /// Lowercase hex-encoded expected digest.
    pub expected_hex: String,
}

/// Mirror endpoint metadata for multi-source segmented downloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorSource {
    /// Full HTTP/HTTPS URL of the mirror resource.
    pub url: String,
    /// Relative priority (lower values indicate higher preference, e.g. Metalink priority 1..=100).
    pub priority: u32,
    /// Optional ISO-3166 geographic location code (e.g. `"us"`, `"de"`).
    pub location: Option<String>,
}

impl MirrorSource {
    /// Create a new [`MirrorSource`] with default priority (`100`) and no location tag.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            priority: 100,
            location: None,
        }
    }

    /// Set the mirror priority (lower value = higher priority).
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    /// Set the optional geographic location tag.
    pub fn with_location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }
}

/// File entry parsed from a Metalink (`.metalink` v3 or `.meta4` RFC 5854) document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetalinkFile {
    /// Target filename declared by the `<file name="...">` element.
    pub name: String,
    /// Expected file size in bytes, if declared.
    pub size: Option<u64>,
    /// Verification digests extracted from `<hash>` elements.
    pub checksums: Vec<ChecksumSpec>,
    /// Mirror URLs sorted by priority.
    pub urls: Vec<MirrorSource>,
}

/// Security constraints enforced across URL parsing, networking, and disk writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityPolicy {
    /// Allow plaintext HTTP URLs in addition to HTTPS.
    pub allow_http: bool,
    /// Allow connections to loopback, private, link-local, or unspecified IP addresses.
    pub allow_private_ips: bool,
    /// Maximum number of HTTP redirects followed per request.
    pub max_redirects: usize,
    /// Optional upper bound on total downloaded size in bytes.
    pub max_download_bytes: Option<u64>,
    /// Enforce path-traversal checks so output stays within the target directory.
    pub enforce_safe_paths: bool,
    /// Optional host allowlist; when non-empty, only listed hosts are permitted.
    pub allowed_hosts: Vec<String>,
    /// Host denylist; listed hosts are always rejected.
    pub blocked_hosts: Vec<String>,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            allow_http: true,
            allow_private_ips: false,
            max_redirects: 10,
            max_download_bytes: None,
            enforce_safe_paths: true,
            allowed_hosts: Vec::new(),
            blocked_hosts: Vec::new(),
        }
    }
}

impl SecurityPolicy {
    /// Strict security preset that disables plaintext HTTP and private IP access.
    pub fn strict() -> Self {
        Self {
            allow_http: false,
            ..Self::default()
        }
    }
}

/// Caller-supplied parameters describing a single download job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRequest {
    /// Source URL, magnet URI, or local `.torrent` file path.
    pub source: String,
    /// Destination directory for the downloaded output.
    pub output_dir: PathBuf,
    /// Optional explicit output file name overriding server or torrent metadata.
    pub filename: Option<String>,
    /// Number of concurrent segment connections to use when range requests are supported.
    pub connections: usize,
    /// Minimum byte size per segment when splitting a range-capable download.
    pub min_segment_size: u64,
    /// Optional global throughput cap in bytes per second (`None` or `Some(0)` = unlimited).
    pub max_bytes_per_sec: Option<u64>,
    /// Additional HTTP request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Resume from existing `.maya.part` and `.maya.state` files if present and valid.
    pub resume: bool,
    /// Overwrite an existing completed destination file if one already exists.
    pub overwrite: bool,
    /// Optional expected digest to verify before finalizing the output file.
    pub checksum: Option<ChecksumSpec>,
    /// Security policy governing network and filesystem operations.
    pub security: SecurityPolicy,
}

impl DownloadRequest {
    /// Construct a [`DownloadRequest`] with default concurrency, resume, and security settings.
    pub fn new(source: impl Into<String>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            output_dir: output_dir.into(),
            filename: None,
            connections: 4,
            min_segment_size: 1024 * 1024,
            max_bytes_per_sec: None,
            headers: Vec::new(),
            resume: true,
            overwrite: false,
            checksum: None,
            security: SecurityPolicy::default(),
        }
    }
}

/// Metadata obtained by probing a remote resource prior to transferring payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub protocol: ProtocolKind,
    /// Final resolved URL after redirects, if applicable.
    pub resolved_url: String,
    /// Total resource size in bytes, if known.
    pub total_size: Option<u64>,
    /// Whether the remote endpoint honors HTTP `Range` requests.
    pub supports_ranges: bool,
    /// Suggested output filename from headers, URL path, or torrent metadata.
    pub suggested_filename: Option<String>,
    /// `ETag` validator returned by the server, if any.
    pub etag: Option<String>,
    /// `Last-Modified` validator returned by the server, if any.
    pub last_modified: Option<String>,
    /// `Content-Type` header value, if any.
    pub content_type: Option<String>,
}

/// Lifecycle state of an individual download segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Pending,
    Active,
    Completed,
    Failed,
}

/// Byte-range slice tracked during segmented and resumable downloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub index: usize,
    /// Inclusive start byte offset within the target file.
    pub start: u64,
    /// Exclusive end byte offset (`None` for unknown-length single-stream transfers).
    pub end: Option<u64>,
    /// Number of bytes persisted for this segment so far.
    pub downloaded: u64,
    pub state: SegmentState,
}

impl Segment {
    /// Total planned length in bytes when `end` is known.
    pub fn len(&self) -> Option<u64> {
        self.end.map(|end| end.saturating_sub(self.start))
    }

    /// Returns `true` when the planned length is known and equal to zero.
    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    /// Next byte offset to request when resuming this segment.
    pub fn current_offset(&self) -> u64 {
        self.start.saturating_add(self.downloaded)
    }

    /// Returns `true` if the segment is marked completed or has downloaded its full range.
    pub fn is_complete(&self) -> bool {
        self.state == SegmentState::Completed
            || self.len().is_some_and(|total| self.downloaded >= total)
    }
}

/// Persistent `.maya.state` checkpoint stored alongside a `.maya.part` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadState {
    pub version: u32,
    pub source: String,
    pub resolved_url: String,
    pub target_path: PathBuf,
    pub part_path: PathBuf,
    pub total_size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<Segment>,
}

/// Real-time progress and lifecycle events emitted during a download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    Probed {
        resolved_url: String,
        total_size: Option<u64>,
        supports_ranges: bool,
        filename: Option<String>,
    },
    Started {
        total_size: Option<u64>,
        segment_count: usize,
    },
    SegmentProgress {
        segment_index: usize,
        bytes_delta: u64,
        total_downloaded: u64,
        total_size: Option<u64>,
    },
    SegmentCompleted {
        segment_index: usize,
    },
    Verifying {
        algorithm: ChecksumAlgorithm,
    },
    Completed {
        output_path: PathBuf,
        total_bytes: u64,
    },
    Failed {
        error: String,
    },
}

/// Storage backend abstraction for staged `.maya.part` writes and atomic finalization.
#[async_trait]
pub trait StorageWriter: Send + Sync {
    /// Prepare or reopen the backing `.maya.part` file, preallocating `total_size` when known.
    async fn prepare(&self, total_size: Option<u64>) -> Result<()>;

    /// Persist `data` at exact byte `offset` in the backing `.maya.part` file.
    async fn write_at(&self, offset: u64, data: Bytes) -> Result<()>;

    /// Flush buffered writes to the underlying storage device.
    async fn flush(&self) -> Result<()>;

    /// Verify the staged file against `checksum` if present, then atomically rename to target.
    async fn finalize(&self, checksum: Option<&ChecksumSpec>) -> Result<PathBuf>;

    /// Clean up temporary `.maya.part` and `.maya.state` files after cancellation or failure.
    async fn abort(&self) -> Result<()>;
}

/// Protocol transport abstraction implemented by HTTP/HTTPS and BitTorrent backends.
#[async_trait]
pub trait ProtocolClient: Send + Sync {
    /// Return `true` if this client handles the given source string.
    fn supports(&self, source: &str) -> bool;

    /// Probe metadata without downloading the full payload.
    async fn probe(&self, request: &DownloadRequest) -> Result<ProbeResult>;

    /// Stream payload bytes for `[start, end)` (`end = None` streams until EOF).
    async fn fetch_range(
        &self,
        request: &DownloadRequest,
        start: u64,
        end: Option<u64>,
    ) -> Result<ByteStream>;
}
