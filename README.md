# Maya Downloader (`v0.0.1` / `v0.0.01`)

Fast, resilient, security-first multi-protocol CLI, daemon, TUI, and desktop GUI downloader written in Rust.

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
- **Desktop Graphical Interface (GUI)**: Tauri v2 + Vite + TypeScript modern desktop app with dark mode, category filters, real-time speed meters, and background daemon auto-recovery.
- **Browser Extension & Native Messaging**: `maya-nmh` binary implementing Chrome/Firefox standard native messaging (length-prefixed JSON stdio protocol). Automatically generates and installs manifests for seamless 1-click browser download interception.
- **Bandwidth Throttling**: Token-bucket async rate limiting (`--limit 500K`, `1.5M`, `5M`, `1G`, or raw bytes/sec).
- **Cryptographic Verification**: Streaming verification supporting SHA-256, BLAKE3, MD5, and SHA-512 (`--verify <algo>:<hex>`) with constant-time equality checks before finalization.
- **Graceful Cancellation**: `Ctrl+C` cleanly cancels active transfers and persists checkpoints (exit code `130`).

---

## Architecture

Maya is organized as a Cargo workspace of focused, modular crates and applications:

| Crate / Path | Responsibility |
| --- | --- |
| `crates/maya-core` | Core error/request/progress types, traits (`ProtocolClient`, `StorageWriter`, `StateStore`), `SecurityPolicy` (SSRF/path guards), `TokenBucketLimiter`, `SegmentScheduler`, `DownloadEngine`, and Metalink/Mirror routing. |
| `crates/maya-storage` | `SparseDiskWriter` (`StorageWriter`), atomic `StateJournal` (`.maya.state`), and streaming SHA-256, BLAKE3, MD5, and SHA-512 verification. |
| `crates/maya-protocols` | Multi-protocol clients: `HttpProtocolClient` (`reqwest` with `rustls-tls`, IP pinning), `TorrentClient` (`librqbit`), `MediaExtractor` (`yt-dlp`), and pure-Rust HLS/DASH parsers. |
| `crates/maya-ipc` | Cross-platform JSON-RPC IPC: Windows Named Pipes (`\\.\pipe\maya-daemon`) and Unix domain sockets (`~/.maya/maya.sock`). |
| `crates/maya-daemon` | `mayad` daemon service, SQLite queue manager (`~/.maya/queue.db`), and background download worker pool. |
| `crates/maya-cli` | `maya` command-line binary (`get`, `batch`, `torrent`, `tui`, `daemon`, `queue`, `doctor`, `version`). |
| `crates/maya-nmh` | Browser Native Messaging Host (`maya-nmh`) supporting Chrome, Edge, Brave, and Firefox. |
| `apps/maya-gui` | Desktop Graphical Application built with Tauri v2, Vite, and TypeScript. |
| `extensions/browser-capture` | Manifest V3 Browser Extension for Chrome, Edge, and Firefox. |

---

## Quick Installation (Windows PowerShell)

```powershell
irm https://raw.githubusercontent.com/uixatik/maya/main/install.ps1 | iex
```

Or download the installer `Maya_0.0.1_x64-setup.exe` directly from the [GitHub Releases](https://github.com/uixatik/maya/releases) page.
