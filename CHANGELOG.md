# Changelog

All notable changes to **Maya Downloader** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.0.1] - 2026-11-13

### Initial Release — Windows Launch & Multi-Protocol Foundation

#### Added
- **Multi-Connection Segmented Download Engine (`maya-core`)**:
  - Dynamic byte-range chunk splitting with adaptive work-stealing for idle connections.
  - Token-bucket global and per-task bandwidth throttling.
  - Multi-mirror failover supporting RFC 5854 Metalink v4 and Metalink v3 XML manifests.
  - Comprehensive SSRF firewall blocking private IPv4, IPv6 loopback, and IPv4-mapped IPv6 ranges.
- **Storage Subsystem (`maya-storage`)**:
  - Sparse preallocated disk file writer with zero-fragmentation chunks.
  - Memory-mapped file verification engine.
  - Atomic, crash-safe state journaling (`.part` and `.state.json`) enabling pause and resume.
  - Checksum validation for SHA-256, SHA-512, MD5, and BLAKE3.
- **Protocol Handlers (`maya-protocols`)**:
  - Resilient HTTP/HTTPS client with RFC 7233 range probe and fallback.
  - HLS (`.m3u8`) streaming parser with AES-128-CBC chunk decryption and sliding-window download.
  - MPEG-DASH (`.mpd`) manifest parser supporting segment templates and timelines.
  - BitTorrent client integration for torrent metadata files and magnet links.
  - Zero-latency `yt-dlp` stream format discovery and extractor bridge.
- **Background Daemon (`maya-daemon`)**:
  - `mayad` standalone persistent service with SQLite job storage.
  - Queue management: concurrency limits, priority queues, retry policies, and auto-scheduling.
- **Inter-Process Communication (`maya-ipc`)**:
  - JSON-RPC 2.0 transport over Windows Named Pipes (`\\.\pipe\maya-ipc`) and Unix Domain Sockets.
  - Full bidirectional RPC methods and real-time download progress event streaming.
- **Native Messaging Host (`maya-nmh`)**:
  - Chrome, Edge, and Firefox browser bridge using stdio binary framing.
  - Automated platform manifest generator and registry installer.
- **Command Line Interface (`maya-cli`)**:
  - High-performance CLI commands: `maya get <url>`, `maya batch`, `maya daemon`, `maya queue`, `maya doctor`.
  - Interactive terminal UI dashboard built with `ratatui` and `crossterm`.
- **Desktop Graphical Interface (`maya-gui`)**:
  - Tauri v2 + TypeScript dashboard application.
  - Live speed charts, concurrent download list, pause/resume controls, and system tray integration.
- **Browser Extension (`extensions/browser-capture`)**:
  - Manifest V3 browser extension for Chrome, Edge, and Firefox.
  - Instant download interception, popup quick-add modal, and connection status indicator.
- **Release Packaging**:
  - Standalone Windows NSIS Setup Wizard (`Maya_0.0.1_x64-setup.exe`).
  - Portable x86_64 zip package (`maya-v0.0.1-windows-x86_64.zip`).
  - One-line PowerShell web installer (`install.ps1`) and CMD wrapper (`install.cmd`).
  - WinGet, Scoop, and Arch Linux AUR manifests.
