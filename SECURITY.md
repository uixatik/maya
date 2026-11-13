# Security Policy

The Maya Downloader project takes security seriously. As a tool designed to fetch data from diverse networks, protocols, and mirrors, Maya implements strict defensive controls at every layer.

---

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.0.1   | :white_check_mark: |
| < 0.0.1 | :x:                |

Critical security patches will be backported to the current minor release series.

---

## Reporting a Vulnerability

If you discover a security vulnerability or weakness in Maya, please **do not report it in a public issue**. Instead, follow responsible disclosure:

1. Send an email to **[uixatik@gmail.com](mailto:uixatik@gmail.com)** with the subject line: `[SECURITY] Vulnerability Report: Maya`.
2. Include the following details in your report:
   - Crate or component affected (`maya-core`, `maya-nmh`, `mayad`, etc.)
   - Steps to reproduce or proof-of-concept exploit
   - Impact assessment (e.g. arbitrary file overwrite, SSRF bypass, denial of service)
   - Any proposed remediation or patch
3. You will receive an initial response within **48 hours**. We will coordinate a fix and publish a security advisory alongside a patch release.

---

## Maya Security Architecture

Maya is architected from the ground up with strict defense-in-depth security principles:

### 1. Server-Side Request Forgery (SSRF) Prevention
- In non-loopback download modes, Maya's network resolver proactively checks resolved IP addresses against IPv4 private ranges (RFC 1918), link-local (`169.254.0.0/16`), loopback (`127.0.0.0/8`), IPv6 link-local (`fe80::/10`), IPv6 loopback (`::1`), and IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`).
- Redirects are validated before establishing connections; attempts to redirect from public HTTPS endpoints to internal RFC 1918 addresses are aborted immediately.

### 2. Path Traversal & Filename Sanitization
- Filenames extracted from URL paths or HTTP `Content-Disposition` headers are sanitized to strip path separators (`/`, `\`), directory traversal tokens (`..`), null bytes, Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`), and control characters.
- Downloads cannot escape the target destination directory.

### 3. Native Messaging Host (NMH) Framing
- The browser integration (`maya-nmh`) enforces strict maximum message payload limits (1 MiB), preventing buffer overflows and memory exhaustion attacks from malicious web scripts.
- Only authenticated and whitelisted browser extension IDs are permitted to communicate with the host.

### 4. Integrity Verification
- Downloads support streaming cryptographic validation using SHA-256, SHA-512, MD5, and BLAKE3. If a hash mismatch occurs, partial artifacts are preserved for inspection but never promoted to the target file name.
