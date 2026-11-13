# Contributing to Maya Downloader

Thank you for your interest in contributing to **Maya Downloader**! Maya is an open-source, resilient, high-speed multi-protocol download manager built in Rust with CLI, interactive Ratatui TUI, Tauri v2 desktop GUI, and browser extension integrations.

We welcome all contributions: bug reports, documentation improvements, feature requests, and code contributions.

---

## Code of Conduct

All contributors and participants are expected to adhere to our [Code of Conduct](CODE_OF_CONDUCT.md). Please report unacceptable behavior to [uixatik@gmail.com](mailto:uixatik@gmail.com).

---

## Development Prerequisites

Before building Maya locally, make sure you have installed:

1. **Rust Toolchain**: Rust 1.75 or newer (installed via `rustup`).
   ```bash
   rustup default stable
   rustup component add clippy rustfmt
   ```
2. **C Compiler**:
   - **Windows**: MSVC build tools (via Visual Studio Installer or C++ Build Tools).
   - **Linux**: `build-essential` / `gcc`.
   - **macOS**: Xcode Command Line Tools (`xcode-select --install`).
3. **Node.js & npm** (optional, only required when building `apps/maya-gui`):
   - Node.js 18+ and npm 9+.
4. **Media Helpers** (optional runtime tools):
   - `ffmpeg` (for video stream multiplexing).
   - `yt-dlp` (for extracting media stream formats).

---

## Repository Architecture

The Maya repository is organized as a Cargo workspace with clean separation of concerns:

```
maya/
├── crates/
│   ├── maya-core/        # Chunk scheduler, adaptive work stealing, SSRF security, checksum verification
│   ├── maya-storage/     # Sparse preallocated file writer, crash-safe atomic journal
│   ├── maya-protocols/   # Multi-protocol clients: HTTP/HTTPS, HLS (m3u8), DASH (mpd), BitTorrent, yt-dlp
│   ├── maya-daemon/      # mayad background service, SQLite job store, queue manager
│   ├── maya-ipc/         # Windows Named Pipes & Unix Domain Sockets JSON-RPC protocol
│   ├── maya-nmh/         # Native Messaging Host for Chrome, Edge, and Firefox
│   └── maya-cli/         # maya CLI runner, interactive Ratatui TUI dashboard, system diagnostics
├── apps/
│   └── maya-gui/         # Tauri v2 + TypeScript dashboard desktop application
├── extensions/
│   └── browser-capture/  # Manifest V3 browser extension for Chrome, Edge, and Firefox
├── packaging/            # Scoop, WinGet, and Arch Linux package manifests
└── .github/              # GitHub Actions CI/CD workflows and issue templates
```

---

## Building and Testing

### 1. Build Workspace
```bash
# Debug build
cargo build --workspace

# Optimized release build
cargo build --release --workspace
```

### 2. Run Test Suite
Maya enforces 100% test pass rate across all workspace crates and end-to-end suites:
```bash
cargo test --workspace
```

### 3. Check Code Formatting
Code formatting must strictly comply with `rustfmt`:
```bash
cargo fmt --all -- --check
```
To automatically format all files:
```bash
cargo fmt --all
```

### 4. Run Linter (Zero-Warning Policy)
Maya maintains a zero-tolerance policy for compiler and clippy warnings:
```bash
cargo clippy --workspace --all-targets -- -D warnings
```

---

## Pull Request Guidelines

1. **Create a Feature Branch**:
   ```bash
   git checkout -b feat/your-feature-name
   # or
   git checkout -b fix/issue-description
   ```

2. **Commit Conventions**:
   Follow [Conventional Commits](https://www.conventionalcommits.org/):
   - `feat(scope): add support for feature`
   - `fix(scope): resolve bug in module`
   - `docs: update documentation`
   - `test: add unit test coverage`
   - `refactor: clean up internal data structures`
   - `perf: optimize chunk write throughput`

3. **Verify Locally Before Pushing**:
   Ensure all checks pass locally:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

4. **Submit Pull Request**:
   - Provide a clear summary of changes in the PR description.
   - Reference any relevant issues (e.g. `Fixes #12`).
   - Confirm all CI checks pass.

Thank you for helping make Maya the fastest and most dependable download manager!
