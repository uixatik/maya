use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use maya_core::{ChecksumAlgorithm, ChecksumSpec, DownloadRequest, SecurityPolicy};

/// Fast, resilient, multi-protocol downloader CLI.
#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(
    name = "maya",
    version,
    about = "Maya Downloader - fast, resumable HTTP/HTTPS, BitTorrent, and media streaming downloader"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level `maya` subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Download a file via HTTP/HTTPS (or magnet/torrent source).
    #[command(aliases = ["dl", "download"])]
    Get(GetArgs),

    /// Download a magnet URI or `.torrent` file.
    Torrent(TorrentArgs),

    /// Download multiple URLs concurrently from a batch text file.
    Batch(BatchArgs),

    /// Launch interactive Terminal User Interface (TUI).
    Tui(TuiArgs),

    /// Launch Maya Desktop GUI or download and run installer.
    #[command(alias = "desktop")]
    Gui(GuiArgs),

    /// Manage Maya background daemon (`mayad`).
    Daemon(DaemonArgs),

    /// Manage background download queue over IPC.
    Queue(QueueArgs),

    /// Display version and compiled protocol/TLS capabilities.
    Version,

    /// Inspect OS, writable download path, feature support, and optional external tools.
    Doctor(DoctorArgs),
}

/// Arguments for `maya get` (`dl`, `download`).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct GetArgs {
    /// Source URL, magnet URI, or `.torrent` path.
    #[arg(value_name = "SOURCE")]
    pub source: String,

    /// Number of concurrent segment connections.
    #[arg(
        short = 'c',
        long = "connections",
        default_value_t = 8,
        value_name = "N",
        value_parser = parse_connections
    )]
    pub connections: usize,

    /// Override output filename inside the destination directory.
    #[arg(short = 'o', long = "out", value_name = "FILENAME")]
    pub out: Option<String>,

    /// Destination directory for downloaded files.
    #[arg(short = 'd', long = "dir", default_value = ".", value_name = "DIR")]
    pub dir: PathBuf,

    /// Bandwidth limit (e.g. `500K`, `1.5M`, `5M`, `1G`, or bytes/sec).
    #[arg(long = "limit", value_name = "RATE", value_parser = parse_rate_limit)]
    pub limit: Option<u64>,

    /// Custom HTTP request header (`Name:Value`), repeatable.
    #[arg(
        short = 'H',
        long = "header",
        value_name = "Name:Value",
        value_parser = parse_header
    )]
    pub headers: Vec<(String, String)>,

    /// Verify completed download digest (`sha256:<hex>`, `blake3:<hex>`, `md5:<hex>`, `sha512:<hex>`).
    #[arg(long = "verify", value_name = "ALGO:HEX", value_parser = parse_checksum)]
    pub verify: Option<ChecksumSpec>,

    /// Disable resuming from `.maya.part` and `.maya.state` checkpoints.
    #[arg(long = "no-resume")]
    pub no_resume: bool,

    /// Overwrite an existing destination file.
    #[arg(long = "overwrite")]
    pub overwrite: bool,

    /// Allow connections to private, loopback, or link-local IP addresses.
    #[arg(long = "allow-private-ips")]
    pub allow_private_ips: bool,

    /// Reject plaintext `http://` URLs and allow only `https://`.
    #[arg(long = "strict-https")]
    pub strict_https: bool,

    /// Emit newline-delimited JSON `ProgressEvent` records to stdout.
    #[arg(long = "json")]
    pub json: bool,

    /// Extract and download streaming video/audio (YouTube, HLS, DASH).
    #[arg(long = "video", visible_alias = "extract")]
    pub video: bool,

    /// HTTP/HTTPS/SOCKS5 proxy URL.
    #[arg(long = "proxy", value_name = "PROXY")]
    pub proxy: Option<String>,

    /// Netscape or plain cookie file for authenticated requests.
    #[arg(long = "cookies", value_name = "FILE")]
    pub cookies: Option<PathBuf>,

    /// Alternative mirror URL(s) for the file.
    #[arg(long = "mirror", value_name = "URL")]
    pub mirrors: Vec<String>,

    /// Metalink (.metalink or .meta4) XML document.
    #[arg(long = "metalink", value_name = "FILE")]
    pub metalink: Option<PathBuf>,
}

impl GetArgs {
    /// Convert parsed CLI arguments into a [`DownloadRequest`].
    pub fn into_request(mut self) -> DownloadRequest {
        if let Some(ref cookies_path) = self.cookies {
            if let Ok(cookie_str) = load_cookies_content(cookies_path) {
                if !cookie_str.is_empty() {
                    self.headers.push(("Cookie".to_string(), cookie_str));
                }
            }
        }
        if let Some(ref proxy_url) = self.proxy {
            std::env::set_var("HTTP_PROXY", proxy_url);
            std::env::set_var("HTTPS_PROXY", proxy_url);
            std::env::set_var("ALL_PROXY", proxy_url);
        }

        let mut req = DownloadRequest::new(self.source, self.dir);
        req.filename = self.out;
        req.connections = self.connections;
        req.max_bytes_per_sec = self.limit;
        req.headers = self.headers;
        req.resume = !self.no_resume;
        req.overwrite = self.overwrite;
        req.checksum = self.verify;
        req.security = if self.strict_https {
            SecurityPolicy::strict()
        } else {
            SecurityPolicy::default()
        };
        req.security.allow_private_ips = self.allow_private_ips;

        if let Some(ref metalink_path) = self.metalink {
            if let Ok(content) = std::fs::read_to_string(metalink_path) {
                if let Ok(meta) = maya_core::mirror::Metalink::parse_xml(&content) {
                    if let Some(primary) = meta.primary_file() {
                        if req.filename.is_none() {
                            req.filename = Some(primary.name.clone());
                        }
                        if req.checksum.is_none() && !primary.checksums.is_empty() {
                            req.checksum = Some(primary.checksums[0].clone());
                        }
                    }
                }
            }
        }

        req
    }
}

/// Arguments for `maya torrent`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct TorrentArgs {
    /// Magnet URI or local `.torrent` file path.
    #[arg(value_name = "SOURCE")]
    pub source: String,

    /// Override output filename/directory name inside the destination directory.
    #[arg(short = 'o', long = "out", value_name = "FILENAME")]
    pub out: Option<String>,

    /// Destination directory for downloaded files.
    #[arg(short = 'd', long = "dir", default_value = ".", value_name = "DIR")]
    pub dir: PathBuf,

    /// Bandwidth limit (e.g. `500K`, `1.5M`, `5M`, `1G`, or bytes/sec).
    #[arg(long = "limit", value_name = "RATE", value_parser = parse_rate_limit)]
    pub limit: Option<u64>,

    /// Overwrite an existing destination file.
    #[arg(long = "overwrite")]
    pub overwrite: bool,

    /// Allow connections to private, loopback, or link-local IP addresses.
    #[arg(long = "allow-private-ips")]
    pub allow_private_ips: bool,

    /// Emit newline-delimited JSON `ProgressEvent` records to stdout.
    #[arg(long = "json")]
    pub json: bool,

    /// Continue seeding after torrent download completes.
    #[arg(long = "seed")]
    pub seed: bool,
}

impl TorrentArgs {
    /// Convert parsed torrent CLI arguments into a [`DownloadRequest`].
    pub fn into_request(self) -> DownloadRequest {
        let mut req = DownloadRequest::new(self.source, self.dir);
        req.filename = self.out;
        req.max_bytes_per_sec = self.limit;
        req.overwrite = self.overwrite;
        req.security.allow_private_ips = self.allow_private_ips;
        req
    }
}

/// Arguments for `maya batch <FILE>`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BatchArgs {
    /// Text file containing URLs (one per line, # for comments).
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Destination directory for downloaded files.
    #[arg(short = 'd', long = "dir", default_value = ".", value_name = "DIR")]
    pub dir: PathBuf,

    /// Number of concurrent segment connections per download.
    #[arg(
        short = 'c',
        long = "connections",
        default_value_t = 4,
        value_name = "N",
        value_parser = parse_connections
    )]
    pub connections: usize,

    /// Maximum simultaneous active downloads.
    #[arg(
        short = 'j',
        long = "concurrency",
        default_value_t = 3,
        value_name = "N"
    )]
    pub concurrency: usize,

    /// Bandwidth limit (e.g. `500K`, `1.5M`, `5M`, `1G`, or bytes/sec).
    #[arg(long = "limit", value_name = "RATE", value_parser = parse_rate_limit)]
    pub limit: Option<u64>,

    /// Allow connections to private, loopback, or link-local IP addresses.
    #[arg(long = "allow-private-ips")]
    pub allow_private_ips: bool,

    /// Reject plaintext `http://` URLs and allow only `https://`.
    #[arg(long = "strict-https")]
    pub strict_https: bool,

    /// Emit newline-delimited JSON records to stdout.
    #[arg(long = "json")]
    pub json: bool,
}

/// Arguments for `maya tui`.
#[derive(Args, Debug, Clone, PartialEq, Eq, Default)]
pub struct TuiArgs {
    /// Custom IPC endpoint (Windows named pipe or Unix domain socket).
    #[arg(long = "endpoint", value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
}

/// Arguments for `maya gui` (`desktop`).
#[derive(Args, Debug, Clone, PartialEq, Eq, Default)]
pub struct GuiArgs {
    /// Download and run the Maya Desktop GUI installer.
    #[arg(long, short)]
    pub install: bool,

    /// Optional action (e.g. `install`).
    #[arg(value_name = "ACTION")]
    pub action: Option<String>,
}

/// Arguments for `maya daemon <start|stop|status>`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: DaemonSubcommand,
}

/// Subcommands for `maya daemon`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum DaemonSubcommand {
    /// Start the background daemon process (`mayad --background`).
    Start {
        /// Optional database file path for queue persistence.
        #[arg(long = "db", value_name = "PATH")]
        db: Option<PathBuf>,

        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },
    /// Stop the running background daemon via IPC.
    Stop {
        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },
    /// Query the status and queue health of the running daemon.
    Status {
        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },
}

/// Arguments for `maya queue <add|list|pause|resume|cancel>`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct QueueArgs {
    #[command(subcommand)]
    pub action: QueueSubcommand,
}

/// Subcommands for `maya queue`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum QueueSubcommand {
    /// Add a download job to the daemon queue.
    Add(QueueAddArgs),

    /// List all jobs tracked in the daemon queue.
    List {
        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,

        /// Output queue items as JSON.
        #[arg(long = "json")]
        json: bool,
    },

    /// Pause a queued or active download job.
    Pause {
        /// Job ID to pause.
        #[arg(value_name = "ID")]
        id: String,

        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },

    /// Resume a paused or failed download job.
    Resume {
        /// Job ID to resume.
        #[arg(value_name = "ID")]
        id: String,

        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },

    /// Cancel a download job.
    Cancel {
        /// Job ID to cancel.
        #[arg(value_name = "ID")]
        id: String,

        /// Optional custom IPC endpoint.
        #[arg(long = "endpoint", value_name = "ENDPOINT")]
        endpoint: Option<String>,
    },
}

/// Arguments for `maya queue add`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct QueueAddArgs {
    /// Download URL.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Destination directory.
    #[arg(short = 'd', long = "dir", value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Override output filename.
    #[arg(short = 'o', long = "out", value_name = "FILENAME")]
    pub out: Option<String>,

    /// Segment connections count.
    #[arg(
        short = 'c',
        long = "connections",
        value_name = "N",
        value_parser = parse_connections
    )]
    pub connections: Option<usize>,

    /// Bandwidth limit.
    #[arg(long = "limit", value_name = "RATE", value_parser = parse_rate_limit)]
    pub limit: Option<u64>,

    /// Checksum verification (`sha256:<hex>`, etc.).
    #[arg(long = "verify", value_name = "ALGO:HEX")]
    pub verify: Option<String>,

    /// Overwrite existing output file.
    #[arg(long = "overwrite")]
    pub overwrite: bool,

    /// Allow connections to private IP addresses.
    #[arg(long = "allow-private-ips")]
    pub allow_private_ips: bool,

    /// Optional custom IPC endpoint.
    #[arg(long = "endpoint", value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
}

/// Arguments for `maya doctor`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct DoctorArgs {
    /// Directory to test for download write access.
    #[arg(short = 'd', long = "dir", default_value = ".", value_name = "DIR")]
    pub dir: PathBuf,
}

fn parse_connections(raw: &str) -> Result<usize, String> {
    let trimmed = raw.trim();
    let value: usize = trimmed
        .parse()
        .map_err(|_| format!("invalid connection count `{raw}`: expected positive integer"))?;
    if value == 0 {
        return Err("connection count must be at least 1".to_string());
    }
    Ok(value)
}

/// Parse a bandwidth rate string such as `500K`, `1.5M`, `1G`, or raw bytes/sec.
pub fn parse_rate_limit(raw: &str) -> Result<u64, String> {
    let mut s = raw.trim();
    if s.is_empty() {
        return Err("rate limit cannot be empty".to_string());
    }

    if let Some(prefix) = strip_suffix_ci(s, "/sec") {
        s = prefix.trim_end();
    } else if let Some(prefix) = strip_suffix_ci(s, "/s") {
        s = prefix.trim_end();
    }

    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split_idx);
    let num_part = num_part.trim();
    let unit_part = unit_part.trim();

    if num_part.is_empty() {
        return Err(format!("invalid rate limit `{raw}`: missing numeric value"));
    }

    let multiplier: u64 = match unit_part.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "ki" | "kib" => 1024,
        "m" | "mb" | "mi" | "mib" => 1024 * 1024,
        "g" | "gb" | "gi" | "gib" => 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "invalid rate limit unit `{unit_part}` in `{raw}` (expected B, K, M, or G)"
            ));
        }
    };

    let bytes_per_sec = if num_part.contains('.') {
        if num_part.bytes().filter(|&b| b == b'.').count() > 1
            || !num_part.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(format!("invalid numeric rate `{num_part}` in `{raw}`"));
        }
        let val: f64 = num_part
            .parse()
            .map_err(|_| format!("invalid numeric rate `{num_part}` in `{raw}`"))?;
        if !val.is_finite() || val <= 0.0 {
            return Err(format!("rate limit `{raw}` must be greater than zero"));
        }
        let scaled = val * (multiplier as f64);
        if !scaled.is_finite() || scaled > (u64::MAX as f64) {
            return Err(format!("rate limit `{raw}` overflows u64"));
        }
        let rounded = scaled.round() as u64;
        if rounded == 0 {
            return Err(format!("rate limit `{raw}` must be at least 1 byte/sec"));
        }
        rounded
    } else {
        if !num_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid numeric rate `{num_part}` in `{raw}`"));
        }
        let base: u64 = num_part
            .parse()
            .map_err(|_| format!("invalid numeric rate `{num_part}` in `{raw}`"))?;
        if base == 0 {
            return Err(format!("rate limit `{raw}` must be greater than zero"));
        }
        base.checked_mul(multiplier)
            .ok_or_else(|| format!("rate limit `{raw}` overflows u64"))?
    };

    Ok(bytes_per_sec)
}

fn strip_suffix_ci<'a>(input: &'a str, suffix: &str) -> Option<&'a str> {
    let suffix_len = suffix.len();
    if input.len() < suffix_len {
        return None;
    }
    let split = input.len() - suffix_len;
    if !input.is_char_boundary(split) {
        return None;
    }
    if input[split..].eq_ignore_ascii_case(suffix) {
        Some(&input[..split])
    } else {
        None
    }
}

/// Parse an HTTP header argument in `Name:Value` format.
pub fn parse_header(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("invalid header `{raw}`: expected `Name:Value`"))?;
    let name = name.trim();
    let value = value.trim();

    if name.is_empty() {
        return Err(format!(
            "invalid header `{raw}`: header name cannot be empty"
        ));
    }
    if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
        return Err("header name/value must not contain CR or LF".to_string());
    }

    Ok((name.to_string(), value.to_string()))
}

/// Parse a checksum verification specification (`sha256:<hex>`, `blake3:<hex>`, `md5:<hex>`, `sha512:<hex>`).
pub fn parse_checksum(raw: &str) -> Result<ChecksumSpec, String> {
    let (algo_raw, hex_raw) = raw.trim().split_once(':').ok_or_else(|| {
        format!("invalid checksum `{raw}`: expected `<sha256|blake3|md5|sha512>:<hex>`")
    })?;

    let (algorithm, expected_len) = match algo_raw.trim().to_ascii_lowercase().as_str() {
        "sha256" | "sha-256" => (ChecksumAlgorithm::Sha256, 64),
        "blake3" => (ChecksumAlgorithm::Blake3, 64),
        "md5" => (ChecksumAlgorithm::Md5, 32),
        "sha512" | "sha-512" => (ChecksumAlgorithm::Sha512, 128),
        other => {
            return Err(format!(
                "unsupported checksum algorithm `{other}` (expected `sha256`, `blake3`, `md5`, or `sha512`)"
            ));
        }
    };

    let hex = hex_raw.trim();
    if hex.len() != expected_len {
        return Err(format!(
            "invalid {algo_raw} digest length: expected {expected_len} hex characters, got {}",
            hex.len()
        ));
    }
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid hex character in checksum digest `{hex_raw}`"
        ));
    }

    Ok(ChecksumSpec {
        algorithm,
        expected_hex: hex.to_ascii_lowercase(),
    })
}

/// Load Netscape or raw cookies file format into a `Cookie: key=value` header string.
pub fn load_cookies_content(path: &Path) -> std::result::Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read cookies file `{}`: {e}", path.display()))?;
    let mut pairs = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = trimmed.split('\t').collect();
        if cols.len() >= 7 {
            pairs.push(format!("{}={}", cols[5].trim(), cols[6].trim()));
        } else if trimmed.contains('=') {
            pairs.push(trimmed.to_string());
        }
    }
    if pairs.is_empty() {
        Ok(raw.trim().to_string())
    } else {
        Ok(pairs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_full_get_with_all_options() {
        let parsed = Cli::parse_from([
            "maya",
            "get",
            "https://example.com/test.zip",
            "--connections",
            "16",
            "--out",
            "custom.zip",
            "--dir",
            "downloads",
            "--limit",
            "2.5M",
            "-H",
            "Authorization: Bearer token123",
            "--verify",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--allow-private-ips",
            "--video",
            "--proxy",
            "http://127.0.0.1:8080",
            "--mirror",
            "https://mirror1.example.com/test.zip",
        ]);

        match parsed.command {
            Commands::Get(args) => {
                assert_eq!(args.source, "https://example.com/test.zip");
                assert_eq!(args.connections, 16);
                assert_eq!(args.out.as_deref(), Some("custom.zip"));
                assert_eq!(args.limit, Some(2_621_440));
                assert_eq!(
                    args.headers,
                    vec![("Authorization".into(), "Bearer token123".into())]
                );
                assert!(args.allow_private_ips);
                assert!(args.video);
                assert_eq!(args.proxy.as_deref(), Some("http://127.0.0.1:8080"));
                assert_eq!(
                    args.mirrors,
                    vec!["https://mirror1.example.com/test.zip".to_string()]
                );
            }
            _ => panic!("expected Commands::Get"),
        }
    }

    #[test]
    fn parse_cli_batch_command() {
        let parsed = Cli::parse_from([
            "maya",
            "batch",
            "urls.txt",
            "-d",
            "/tmp/dest",
            "-c",
            "6",
            "-j",
            "4",
        ]);

        match parsed.command {
            Commands::Batch(args) => {
                assert_eq!(args.file, PathBuf::from("urls.txt"));
                assert_eq!(args.dir, PathBuf::from("/tmp/dest"));
                assert_eq!(args.connections, 6);
                assert_eq!(args.concurrency, 4);
            }
            _ => panic!("expected Commands::Batch"),
        }
    }

    #[test]
    fn parse_cli_tui_command() {
        let parsed = Cli::parse_from(["maya", "tui", "--endpoint", r"\\.\pipe\maya-custom"]);

        match parsed.command {
            Commands::Tui(args) => {
                assert_eq!(args.endpoint.as_deref(), Some(r"\\.\pipe\maya-custom"));
            }
            _ => panic!("expected Commands::Tui"),
        }
    }

    #[test]
    fn parse_cli_daemon_and_queue_commands() {
        let parsed_daemon = Cli::parse_from(["maya", "daemon", "status"]);
        match parsed_daemon.command {
            Commands::Daemon(args) => {
                assert!(matches!(args.action, DaemonSubcommand::Status { .. }));
            }
            _ => panic!("expected Commands::Daemon"),
        }

        let parsed_queue = Cli::parse_from([
            "maya",
            "queue",
            "add",
            "https://example.com/file.bin",
            "-o",
            "out.bin",
        ]);
        match parsed_queue.command {
            Commands::Queue(args) => match args.action {
                QueueSubcommand::Add(add) => {
                    assert_eq!(add.url, "https://example.com/file.bin");
                    assert_eq!(add.out.as_deref(), Some("out.bin"));
                }
                _ => panic!("expected QueueSubcommand::Add"),
            },
            _ => panic!("expected Commands::Queue"),
        }
    }

    #[test]
    fn parse_checksum_supports_all_algorithms() {
        // sha256
        let hex64 = "a".repeat(64);
        let res = parse_checksum(&format!("sha256:{hex64}")).unwrap();
        assert_eq!(res.algorithm, ChecksumAlgorithm::Sha256);

        // blake3
        let res = parse_checksum(&format!("blake3:{hex64}")).unwrap();
        assert_eq!(res.algorithm, ChecksumAlgorithm::Blake3);

        // md5
        let hex32 = "b".repeat(32);
        let res = parse_checksum(&format!("md5:{hex32}")).unwrap();
        assert_eq!(res.algorithm, ChecksumAlgorithm::Md5);

        // sha512
        let hex128 = "c".repeat(128);
        let res = parse_checksum(&format!("sha512:{hex128}")).unwrap();
        assert_eq!(res.algorithm, ChecksumAlgorithm::Sha512);

        // invalid algo
        assert!(parse_checksum(&format!("sha1:{hex64}")).is_err());
    }

    #[test]
    fn parse_cli_gui_command() {
        let parsed = Cli::parse_from(["maya", "gui"]);
        match parsed.command {
            Commands::Gui(args) => {
                assert!(!args.install);
                assert_eq!(args.action, None);
            }
            _ => panic!("expected Commands::Gui"),
        }

        let parsed_desktop = Cli::parse_from(["maya", "desktop"]);
        match parsed_desktop.command {
            Commands::Gui(args) => {
                assert!(!args.install);
                assert_eq!(args.action, None);
            }
            _ => panic!("expected Commands::Gui for alias desktop"),
        }

        let parsed_install_flag = Cli::parse_from(["maya", "gui", "--install"]);
        match parsed_install_flag.command {
            Commands::Gui(args) => {
                assert!(args.install);
                assert_eq!(args.action, None);
            }
            _ => panic!("expected Commands::Gui with --install"),
        }

        let parsed_short_flag = Cli::parse_from(["maya", "gui", "-i"]);
        match parsed_short_flag.command {
            Commands::Gui(args) => {
                assert!(args.install);
                assert_eq!(args.action, None);
            }
            _ => panic!("expected Commands::Gui with -i"),
        }

        let parsed_install_action = Cli::parse_from(["maya", "gui", "install"]);
        match parsed_install_action.command {
            Commands::Gui(args) => {
                assert!(!args.install);
                assert_eq!(args.action.as_deref(), Some("install"));
            }
            _ => panic!("expected Commands::Gui with positional action install"),
        }
    }
}
