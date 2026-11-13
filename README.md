# Maya Downloader (`v0.0.1` / `v0.0.01`)

Fast, resilient, security-first multi-protocol CLI, daemon, and TUI downloader written in Rust.

## Highlights & Capabilities

- **Segmented High-Speed HTTP/HTTPS**: Multi-connection byte-range transfers with IDM-style in-half work stealing (`SegmentScheduler`) and `HEAD` → `GET Range: bytes=0-0` probe fallback.
- **Media & Streaming Video Downloader**: Automatic media URL detection (YouTube, Vimeo, TikTok, Bilibili, SoundCloud, generic HLS/DASH). Discovers `yt-dlp` to extract direct streams, downloads separate high-definition video and audio tracks concurrently via `DownloadEngine`, and automatically muxes with `ffmpeg` (`-c copy`) with seamless temp cleanup.
- **HLS & DASH Manifest Support**: Custom pure-Rust parser for HLS (`.m3u8` master and media playlists, AES-128-CBC sliding-window segment decryption) and MPEG-DASH (`.mpd` SegmentTemplate and SegmentList).
- **Metalink & Multi-Mirror Failover**: Full support for Metalink XML (`.meta4` RFC 5854 and `.metalink` v3). Parallel mirror probing, latency and priority scoring, cross-mirror ETag/Content-Length consensus, and automatic failover on HTTP 429/503.
- **Zero-Merge Resumable Storage**: Positional concurrent zero-merge writes (`seek_write` on Windows, `write_at` on Unix) directly into `<target>.maya.part`, atomic `<target>.maya.state` checkpointing, and ETag/`Last-Modified` validation on resume.
- **BitTorrent & Magnet Transfers**: Built-in torrent/magnet engine via `librqbit` (`rust-tls`), with seeding cleanly terminated upon completion unless `--seed` is requested.
- **Batch Downloading**: `maya batch <FILE>` parses URL lists, ignores comments (`#`) and empty lines, and downloads with configurable concurrency (`-j`/`--concurrency`).
- **Interactive TUI**: `maya tui` launches a terminal interface built with `ratatui` and `crossterm`. Features a real-time aggregate speed sparkline, active download table, and hotkeys (`[p] Pause`, `[r] Resume`, `[c] Cancel`, `[a] Add URL dialog`, `[q] Quit`).
- **Background Daemon & Persistent Queue**: Dedicated `mayad` daemon binary with SQLite persistent queue storage (`~/.maya/queue.db`), Windows Named Pipe / Unix Domain Socket IPC, and CLI management via `maya daemon` and `maya queue`.
- **Browser Extension & Native Messaging**: `maya-nmh` binary implementing Chrome/Firefox standard native messaging (length-prefixed JSON stdio protocol). Automatically generates and installs manifests for seamless 1-click browser download interception.
- **Bandwidth Throttling**: Token-bucket async rate limiting (`--limit 500K`, `1.5M`, `5M`, `1G`, or raw bytes/sec).
- **Cryptographic Verification**: Streaming verification supporting SHA-256, BLAKE3, MD5, and SHA-512 (`--verify <algo>:<hex>`) with constant-time equality checks before finalization.
- **Graceful Cancellation**: `Ctrl+C` cleanly cancels active transfers and persists checkpoints (exit code `130`).

---

## Architecture

Maya is organized as a Cargo workspace of focused, modular crates:

| Crate | Responsibility |
| --- | --- |
| `crates/maya-core` | Core error/request/progress types, traits (`ProtocolClient`, `StorageWriter`, `StateStore`), `SecurityPolicy` (SSRF/path guards), `TokenBucketLimiter`, `SegmentScheduler`, `DownloadEngine`, and Metalink/Mirror routing. |
| `crates/maya-storage` | `SparseDiskWriter` (`StorageWriter`), atomic `StateJournal` (`.maya.state`), and streaming SHA-256, BLAKE3, MD5, and SHA-512 verification. |
| `crates/maya-protocols` | Multi-protocol clients: `HttpProtocolClient` (`reqwest` with `rustls-tls`, IP pinning), `TorrentClient` (`librqbit`), `MediaExtractor` (`yt-dlp`), and pure-Rust HLS/DASH parsers. |
| `crates/maya-ipc` | Cross-platform JSON-RPC IPC: Windows Named Pipes (`\\.\pipe\maya-daemon`) and Unix domain sockets (`~/.maya/maya.sock`). |
| `crates/maya-daemon` | `mayad` daemon service, SQLite queue manager (`~/.maya/queue.db`), and background download worker pool. |
| `crates/maya-cli` | `maya` command-line binary (`get`, `batch`, `torrent`, `tui`, `daemon`, `queue`, `doctor`, `version`). |
| `crates/maya-nmh` | Browser Native Messaging Host (`maya-nmh`) supporting Chrome, Edge, Brave, and Firefox. |

---

## Security Protections

- **Pure-Rust TLS**: Powered exclusively by `rustls` — no dynamic OpenSSL or external C SSL dependencies.
- **SSRF & DNS Rebinding Defense**: Hostnames resolved via `tokio::net::lookup_host`. Automatically blocks loopback (`127.0.0.0/8`), private RFC 1918 networks (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), CGNAT (`100.64.0.0/10`), link-local / IMDS (`169.254.0.0/16`), unspecified, multicast, IPv6 ULA (`fc00::/7`), IPv6 link-local (`fe80::/10`), and IPv4-mapped IPv6 (`::ffff:...`) unless `--allow-private-ips` is specified. IP addresses are pinned per redirect hop.
- **Cross-Origin Credential Stripping**: Automatically removes `Authorization`, `Cookie`, and `Proxy-Authorization` headers when HTTP redirects cross origins.
- **Path Traversal & Device Name Sanitization**: Filenames derived from URLs or RFC 5987 / RFC 6266 `Content-Disposition` headers are sanitized to block directory traversal, control codes, and Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`). Target paths are guaranteed to reside within the destination directory.
- **Strict HTTPS Option**: `--strict-https` rejects plaintext HTTP sources and redirects.

---

## Installation

### 1. One-Liner Installers

#### Windows (PowerShell)
Installs `maya.exe`, `mayad.exe`, and `maya-nmh.exe` into `%LOCALAPPDATA%\Programs\Maya\bin`, adds the folder to your User `PATH`, and registers native messaging manifests for Chrome, Edge, and Firefox:

```powershell
irm https://raw.githubusercontent.com/uixatik/maya/main/install.ps1 | iex
```

Or run locally from repository / release directory:
```powershell
.\install.ps1
```

#### Linux & macOS (POSIX Shell)
Installs binaries into `~/.local/bin` and registers browser native host manifests:

```sh
curl -fsSL https://raw.githubusercontent.com/uixatik/maya/main/install.sh | sh
```

Or run locally:
```sh
chmod +x ./install.sh && ./install.sh
```

---

### 2. Package Managers

#### Windows (Scoop)
```powershell
scoop bucket add maya https://github.com/uixatik/maya
scoop install maya
```

#### Windows (WinGet)
```powershell
winget install Maya.MayaDownloader
```

#### Arch Linux (AUR)
```sh
cd packaging/arch
makepkg -si
```

---

### 3. Build & Install via Cargo
```sh
cargo install --path crates/maya-cli --force
cargo install --path crates/maya-daemon --force
cargo install --path crates/maya-nmh --force
```

---

## Desktop Graphical Interface (GUI)

Maya features a dedicated high-performance desktop GUI built with **Tauri v2**, providing an IDM/FDM-grade user experience with real-time transfer monitoring:

- **Dashboard**: Real-time aggregate download/upload throughput sparkline chart, global speed badges, and active task progress meters.
- **Categorized Queue Management**: Filter downloads by `All`, `Downloading`, `Completed`, `Paused`, `Failed`, `Videos`, and `Torrents`.
- **Add URL Dialog**: Configurable connection count (1–32 streams), destination folder picker, custom headers, and instant download vs. background queue scheduling.
- **Daemon Synchronization**: Automatically communicates with `mayad` over Windows Named Pipe (`\\.\pipe\maya-daemon`) or Unix domain socket (`~/.maya/maya.sock`).

```sh
# Run in development mode
cd apps/maya-gui
npm install
npm run tauri dev

# Build production bundle
npm run tauri build
```

---

## CLI Usage

### 1. `maya get` (aliases: `dl`, `download`)

Download files via HTTP/HTTPS, media platforms, or torrents:

```sh
maya get <SOURCE> [OPTIONS]
```

**Common Examples:**
```sh
# High-speed segmented download with 8 connections
maya get "https://example.com/largefile.iso" -c 8 -d ./downloads

# Bandwidth-limited download
maya get "https://example.com/data.tar.gz" --limit 5M

# Streaming Video extraction (YouTube, Vimeo, etc.)
maya get "https://www.youtube.com/watch?v=BMzW1i1OJ_o" -d ./downloads

# Checksum verification (SHA-256, BLAKE3, MD5, SHA-512)
maya get "https://example.com/file.zip" --verify sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855

# Multi-mirror failover and Metalink
maya get "https://primary.mirror.org/ubuntu.iso" --mirror "https://backup1.org/ubuntu.iso" --mirror "https://backup2.org/ubuntu.iso"
maya get --metalink ./release.meta4 -d ./downloads
```

---

## Browser Extension Integration

Maya includes a high-performance Browser Extension (`extensions/browser-capture`) paired with the Native Messaging Host (`maya-nmh`) for Chrome, Microsoft Edge, Brave, Opera, Vivaldi, and Mozilla Firefox. It automatically captures browser downloads and routes them directly to `mayad` for accelerated, segmented transfer.

### 1. Register Native Messaging Host
```sh
# Automatically registers with Chrome, Edge, and Firefox
maya-nmh --install-manifest
```

### 2. Load the Extension into Chromium Browsers (Chrome / Edge / Brave)
1. Open your browser and navigate to `chrome://extensions` or `edge://extensions`.
2. Enable the **Developer mode** toggle in the top-right corner.
3. Click the **Load unpacked** button.
4. Select the `extensions/browser-capture` directory.

---

## Development & Test Suite

Run full code quality verification across all workspace crates:

```sh
# Formatting check
cargo fmt --all -- --check

# Clippy linter
cargo clippy --workspace --all-targets -- -D warnings

# Full workspace test suite (125 tests passing)
cargo test --workspace
```
