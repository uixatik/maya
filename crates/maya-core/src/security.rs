use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};

use crate::error::{MayaError, Result};
use crate::types::SecurityPolicy;

const MAX_FILENAME_BYTES: usize = 200;

/// Validate a download URL against the active [`SecurityPolicy`].
///
/// Enforces `http`/`https` schemes, `policy.allow_http`, non-empty host,
/// `blocked_hosts`, and `allowed_hosts` (case-insensitive exact or subdomain match).
pub fn validate_url(raw: &str, policy: &SecurityPolicy) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    match url.scheme() {
        "https" => {}
        "http" => {
            if !policy.allow_http {
                return Err(MayaError::SecurityViolation(
                    "plaintext HTTP is disallowed by security policy".to_string(),
                ));
            }
        }
        other => return Err(MayaError::UnsupportedScheme(other.to_string())),
    }

    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| MayaError::SecurityViolation("URL must specify a valid host".to_string()))?;

    if policy
        .blocked_hosts
        .iter()
        .any(|pat| host_matches(host, pat))
    {
        return Err(MayaError::SecurityViolation(format!(
            "host '{host}' is blocked by security policy"
        )));
    }

    if !policy.allowed_hosts.is_empty()
        && !policy
            .allowed_hosts
            .iter()
            .any(|pat| host_matches(host, pat))
    {
        return Err(MayaError::SecurityViolation(format!(
            "host '{host}' is not in the allowed_hosts policy"
        )));
    }

    Ok(url)
}

/// Return `true` when `ip` is forbidden under `policy` (SSRF protection).
///
/// Rejects loopback, RFC 1918 private, CGNAT `100.64.0.0/10`, link-local/IMDS
/// `169.254.0.0/16`, unspecified, multicast, documentation/reserved IPv4 ranges,
/// IPv6 ULA (`fc00::/7`), IPv6 link-local (`fe80::/10`), and IPv4-mapped IPv6
/// (`::ffff:...`) unless `policy.allow_private_ips` is enabled.
pub fn is_forbidden_ip(ip: IpAddr, policy: &SecurityPolicy) -> bool {
    if policy.allow_private_ips {
        return false;
    }

    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

/// Resolve `host:port` using Tokio DNS lookup and validate every returned address
/// against [`is_forbidden_ip`].
pub async fn resolve_and_validate_addrs(
    host: &str,
    port: u16,
    policy: &SecurityPolicy,
) -> Result<Vec<SocketAddr>> {
    let clean_host = strip_ipv6_brackets(host.trim());
    if clean_host.is_empty() {
        return Err(MayaError::SecurityViolation("empty host".to_string()));
    }

    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        if is_forbidden_ip(ip, policy) {
            return Err(MayaError::SecurityViolation(format!(
                "IP address '{ip}' is forbidden by security policy"
            )));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((clean_host, port))
        .await
        .map_err(|err| MayaError::Network(format!("DNS lookup failed for '{clean_host}': {err}")))?
        .collect();

    if addrs.is_empty() {
        return Err(MayaError::Network(format!(
            "DNS lookup returned no addresses for '{clean_host}'"
        )));
    }

    for addr in &addrs {
        if is_forbidden_ip(addr.ip(), policy) {
            return Err(MayaError::SecurityViolation(format!(
                "resolved IP '{}' for host '{clean_host}' is forbidden by security policy",
                addr.ip()
            )));
        }
    }

    Ok(addrs)
}

/// Sanitize an untrusted filename into a safe single path component across
/// Windows, macOS, and Linux.
pub fn sanitize_filename(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = basename
        .chars()
        .filter(|&c| !c.is_control() && !is_forbidden_filename_char(c))
        .collect();

    let trimmed = cleaned.trim_start().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "download".to_string();
    }

    let mut result = truncate_preserving_extension(trimmed, MAX_FILENAME_BYTES);
    result = result.trim_end_matches(['.', ' ']).to_string();

    if result.is_empty() || result == "." || result == ".." {
        return "download".to_string();
    }

    if is_windows_reserved_name(&result) {
        result.insert(0, '_');
        if result.len() > MAX_FILENAME_BYTES {
            result = truncate_preserving_extension(&result, MAX_FILENAME_BYTES);
            result = result.trim_end_matches(['.', ' ']).to_string();
        }
    }

    if result.is_empty() {
        "download".to_string()
    } else {
        result
    }
}

/// Validate that `filename` is a safe single path component and remains inside
/// `output_dir`, returning the joined destination path.
pub fn safe_target_path(output_dir: &Path, filename: &str) -> Result<PathBuf> {
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains(['/', '\\', '\0'])
        || filename
            .chars()
            .any(|c| c.is_control() || is_forbidden_filename_char(c))
        || filename.ends_with(['.', ' '])
        || is_windows_reserved_name(filename)
    {
        return Err(MayaError::SecurityViolation(format!(
            "unsafe target filename: '{filename}'"
        )));
    }

    let mut components = Path::new(filename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => {
            return Err(MayaError::SecurityViolation(format!(
                "filename must be a single path component: '{filename}'"
            )));
        }
    }

    let candidate = output_dir.join(filename);

    if output_dir.exists() {
        let canon_dir = std::fs::canonicalize(output_dir)?;
        if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() || candidate.exists() {
                let canon_target = std::fs::canonicalize(&candidate).map_err(|_| {
                    MayaError::SecurityViolation(format!(
                        "target path '{}' could not be resolved safely",
                        candidate.display()
                    ))
                })?;
                if !canon_target.starts_with(&canon_dir) {
                    return Err(MayaError::SecurityViolation(format!(
                        "target path '{}' escapes output directory '{}'",
                        canon_target.display(),
                        canon_dir.display()
                    )));
                }
            }
        }
    }

    Ok(candidate)
}

/// Extract and sanitize a filename from an HTTP `Content-Disposition` header.
///
/// Supports both RFC 5987 `filename*=` (UTF-8) and standard `filename=`
/// parameters, preferring `filename*=` when both are present.
pub fn parse_content_disposition_filename(header: &str) -> Option<String> {
    let mut extended: Option<String> = None;
    let mut standard: Option<String> = None;

    for part in split_semicolon_preserving_quotes(header) {
        let trimmed = part.trim();
        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };
        let key_lower = key.trim().to_ascii_lowercase();
        let val = val.trim();

        if key_lower == "filename*" {
            if let Some(decoded) = decode_rfc5987_value(val) {
                if !decoded.trim().is_empty() {
                    extended = Some(decoded);
                }
            }
        } else if key_lower == "filename" && standard.is_none() {
            let unquoted = unquote_header_value(val);
            if !unquoted.trim().is_empty() {
                standard = Some(unquoted);
            }
        }
    }

    let raw = extended.or(standard)?;
    let sanitized = sanitize_raw_optional(&raw)?;
    Some(sanitized)
}

/// Derive a safe fallback filename from a URL's path segments.
pub fn derive_filename_from_url(url: &url::Url) -> String {
    let last_segment = url
        .path_segments()
        .and_then(|segments| segments.rev().find(|seg| !seg.is_empty()));

    match last_segment {
        Some(seg) => {
            let decoded = percent_decode_utf8(seg);
            sanitize_filename(&decoded)
        }
        None => "download".to_string(),
    }
}

fn sanitize_raw_optional(raw: &str) -> Option<String> {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = basename
        .chars()
        .filter(|&c| !c.is_control() && !is_forbidden_filename_char(c))
        .collect();
    let trimmed = cleaned.trim_start().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    Some(sanitize_filename(raw))
}

fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

fn normalize_host(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('.');
    strip_ipv6_brackets(trimmed).to_ascii_lowercase()
}

fn host_matches(host: &str, pattern: &str) -> bool {
    let host = normalize_host(host);
    let pat = normalize_host(pattern);
    let pat = pat.strip_prefix("*.").unwrap_or(&pat);
    if pat.is_empty() || host.is_empty() {
        return false;
    }
    host == pat || host.ends_with(&format!(".{pat}"))
}

fn is_forbidden_ipv4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_this_network = octets[0] == 0;
    let is_benchmarking = octets[0] == 198 && (octets[1] & 0b1111_1110) == 18;
    let is_reserved_240 = octets[0] >= 240;

    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_documentation()
        || is_cgnat
        || is_this_network
        || is_benchmarking
        || is_reserved_240
}

fn is_forbidden_ipv6(v6: Ipv6Addr) -> bool {
    let segs = v6.segments();

    // IPv4-mapped IPv6 (`::ffff:0:0/96`)
    if segs[0..5] == [0, 0, 0, 0, 0] && segs[5] == 0xffff {
        let v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        return is_forbidden_ipv4(v4);
    }

    // IPv4-compatible IPv6 (`::a.b.c.d`) excluding `::` and `::1`
    if segs[0..6] == [0, 0, 0, 0, 0, 0] && (segs[6] != 0 || segs[7] > 1) {
        let v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        return is_forbidden_ipv4(v4);
    }

    let is_ula = (segs[0] & 0xfe00) == 0xfc00; // fc00::/7
    let is_link_local = (segs[0] & 0xffc0) == 0xfe80; // fe80::/10
    let is_site_local = (segs[0] & 0xffc0) == 0xfec0; // fec0::/10
    let is_doc = segs[0] == 0x2001 && segs[1] == 0x0db8; // 2001:db8::/32

    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || is_ula
        || is_link_local
        || is_site_local
        || is_doc
}

fn is_forbidden_filename_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn truncate_preserving_extension(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    if let Some(dot_idx) = s.rfind('.') {
        let ext = &s[dot_idx..];
        if dot_idx > 0 && ext.len() <= 32 && ext.len() < max_bytes {
            let stem_budget = max_bytes - ext.len();
            let truncated_stem = truncate_utf8_bytes(&s[..dot_idx], stem_budget);
            if !truncated_stem.is_empty() {
                return format!("{truncated_stem}{ext}");
            }
        }
    }

    truncate_utf8_bytes(s, max_bytes).to_string()
}

fn split_semicolon_preserving_quotes(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&input[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn unquote_header_value(val: &str) -> String {
    let trimmed = val.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        trimmed.to_string()
    }
}

fn decode_rfc5987_value(raw: &str) -> Option<String> {
    let unquoted = unquote_header_value(raw);
    let mut parts = unquoted.splitn(3, '\'');
    let charset = parts.next()?.trim();
    let _lang = parts.next()?;
    let encoded = parts.next()?;

    if !charset.is_empty()
        && !charset.eq_ignore_ascii_case("utf-8")
        && !charset.eq_ignore_ascii_case("utf8")
    {
        // Fallback: still attempt UTF-8 percent-decoding if ASCII/UTF-8 compatible.
        return Some(percent_decode_utf8(encoded));
    }
    Some(percent_decode_utf8(encoded))
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_enforces_schemes_and_allow_http() {
        let mut policy = SecurityPolicy::default();
        assert!(validate_url("https://example.com/file.zip", &policy).is_ok());
        assert!(validate_url("http://example.com/file.zip", &policy).is_ok());

        policy.allow_http = false;
        assert!(matches!(
            validate_url("http://example.com/file.zip", &policy),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(validate_url("https://example.com/file.zip", &policy).is_ok());

        assert!(matches!(
            validate_url("ftp://example.com/file.zip", &policy),
            Err(MayaError::UnsupportedScheme(s)) if s == "ftp"
        ));
        assert!(matches!(
            validate_url("file:///etc/passwd", &policy),
            Err(MayaError::UnsupportedScheme(s)) if s == "file"
        ));
    }

    #[test]
    fn validate_url_enforces_allowed_and_blocked_hosts() {
        let policy = SecurityPolicy {
            allowed_hosts: vec!["Example.COM".to_string()],
            blocked_hosts: vec!["evil.example.com".to_string()],
            ..SecurityPolicy::default()
        };

        assert!(validate_url("https://example.com/a", &policy).is_ok());
        assert!(validate_url("https://cdn.example.com/a", &policy).is_ok());
        assert!(matches!(
            validate_url("https://evil.example.com/a", &policy),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            validate_url("https://sub.evil.example.com/a", &policy),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            validate_url("https://badexample.com/a", &policy),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            validate_url("https://other.org/a", &policy),
            Err(MayaError::SecurityViolation(_))
        ));
    }

    #[test]
    fn forbidden_ip_blocks_ssrf_ranges_and_allows_public() {
        let policy = SecurityPolicy::default();

        let blocked = [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::",
            "ff02::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:100.64.0.5",
        ];
        for ip in blocked {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(
                is_forbidden_ip(parsed, &policy),
                "expected {ip} to be blocked"
            );
        }

        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ];
        for ip in allowed {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(
                !is_forbidden_ip(parsed, &policy),
                "expected {ip} to be allowed"
            );
        }

        let permissive = SecurityPolicy {
            allow_private_ips: true,
            ..SecurityPolicy::default()
        };
        assert!(!is_forbidden_ip("127.0.0.1".parse().unwrap(), &permissive));
        assert!(!is_forbidden_ip(
            "169.254.169.254".parse().unwrap(),
            &permissive
        ));
    }

    #[tokio::test]
    async fn resolve_and_validate_addrs_checks_ip_literals() {
        let strict = SecurityPolicy::default();
        assert!(matches!(
            resolve_and_validate_addrs("127.0.0.1", 8080, &strict).await,
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            resolve_and_validate_addrs("[::1]", 8080, &strict).await,
            Err(MayaError::SecurityViolation(_))
        ));

        let permissive = SecurityPolicy {
            allow_private_ips: true,
            ..SecurityPolicy::default()
        };
        let addrs = resolve_and_validate_addrs("127.0.0.1", 8080, &permissive)
            .await
            .unwrap();
        assert_eq!(addrs, vec!["127.0.0.1:8080".parse().unwrap()]);
    }

    #[test]
    fn sanitize_filename_handles_traversal_reserved_and_length() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\secret.txt"), "secret.txt");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("."), "download");
        assert_eq!(sanitize_filename("   ...  "), "download");
        assert_eq!(sanitize_filename("bad\0name\x07.txt"), "badname.txt");
        assert_eq!(sanitize_filename("file:name*?\"<>|.zip"), "filename.zip");
        assert_eq!(sanitize_filename("trailing. . "), "trailing");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_filename("nul.tar.gz"), "_nul.tar.gz");
        assert_eq!(sanitize_filename("com1"), "_com1");
        assert_eq!(sanitize_filename("LPT9.log"), "_LPT9.log");
        assert_eq!(sanitize_filename("COM10.log"), "COM10.log");

        let long = format!("{}.tar.gz", "a".repeat(300));
        let sanitized = sanitize_filename(&long);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.ends_with(".gz"));
    }

    #[test]
    fn safe_target_path_blocks_traversal_and_accepts_valid() {
        let dir = std::env::temp_dir().join(format!("maya_sec_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(safe_target_path(&dir, "valid_file-1.0.tar.gz").is_ok());
        assert!(matches!(
            safe_target_path(&dir, "../escape.txt"),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            safe_target_path(&dir, "sub/file.txt"),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            safe_target_path(&dir, "CON"),
            Err(MayaError::SecurityViolation(_))
        ));
        assert!(matches!(
            safe_target_path(&dir, ""),
            Err(MayaError::SecurityViolation(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_content_disposition_supports_rfc5987_and_standard() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"hello world.zip\""),
            Some("hello world.zip".to_string())
        );
        assert_eq!(
            parse_content_disposition_filename(
                "attachment; filename=\"fallback.txt\"; filename*=UTF-8''my%20file%20%281%29.tar.gz"
            ),
            Some("my file (1).tar.gz".to_string())
        );
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"../../CON\""),
            Some("_CON".to_string())
        );
        assert_eq!(parse_content_disposition_filename("attachment"), None);
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"\""),
            None
        );
    }

    #[test]
    fn derive_filename_from_url_extracts_and_sanitizes() {
        let u1 = url::Url::parse("https://example.com/releases/maya%20v1.zip?dl=1").unwrap();
        assert_eq!(derive_filename_from_url(&u1), "maya v1.zip");

        let u2 = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(derive_filename_from_url(&u2), "download");
    }
}
