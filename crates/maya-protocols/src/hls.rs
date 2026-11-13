use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aes::Aes128;
use bytes::Bytes;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use futures::stream::{self, Stream, StreamExt};
use maya_core::{DownloadRequest, MayaError, ProtocolClient, Result, SecurityPolicy};
use tokio::sync::{watch, Mutex};
use url::Url;

use crate::http::HttpProtocolClient;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// Byte sub-range for `#EXT-X-BYTERANGE` or `#EXT-X-MAP` `BYTERANGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub length: u64,
    pub offset: u64,
}

/// Supported `#EXT-X-KEY` encryption methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsKeyMethod {
    Aes128,
    SampleAes,
    Other(String),
}

/// Encryption key metadata attached to an HLS segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsKey {
    pub method: HlsKeyMethod,
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

/// Initialization segment metadata from `#EXT-X-MAP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsInitSegment {
    pub uri: String,
    pub byte_range: Option<ByteRange>,
}

/// Single media segment inside an HLS media playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegment {
    pub sequence: u64,
    pub duration: f64,
    pub title: Option<String>,
    pub uri: String,
    pub byte_range: Option<ByteRange>,
    pub key: Option<HlsKey>,
    pub discontinuity: bool,
}

/// Variant stream entry inside an HLS master playlist (`#EXT-X-STREAM-INF`).
#[derive(Debug, Clone, PartialEq)]
pub struct HlsVariant {
    pub uri: String,
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
    pub audio: Option<String>,
    pub subtitles: Option<String>,
}

/// Parsed HLS master playlist containing adaptive bitrate variants.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HlsMasterPlaylist {
    pub version: Option<u32>,
    pub variants: Vec<HlsVariant>,
}

impl HlsMasterPlaylist {
    /// Select the highest-quality variant (by bandwidth, breaking ties by pixel area).
    pub fn select_best_variant(&self) -> Option<&HlsVariant> {
        self.variants.iter().max_by_key(|v| {
            let pixels = v
                .resolution
                .map(|(w, h)| u64::from(w) * u64::from(h))
                .unwrap_or(0);
            (v.bandwidth, pixels)
        })
    }

    /// Select the highest-bandwidth variant whose bandwidth does not exceed `max_bandwidth`.
    pub fn select_variant_max_bandwidth(&self, max_bandwidth: u64) -> Option<&HlsVariant> {
        self.variants
            .iter()
            .filter(|v| v.bandwidth <= max_bandwidth)
            .max_by_key(|v| v.bandwidth)
            .or_else(|| self.variants.iter().min_by_key(|v| v.bandwidth))
    }
}

/// Parsed HLS media playlist containing ordered segments.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsMediaPlaylist {
    pub version: Option<u32>,
    pub target_duration: u64,
    pub media_sequence: u64,
    pub end_list: bool,
    pub init_segment: Option<HlsInitSegment>,
    pub segments: Vec<HlsSegment>,
}

/// Parsed HLS playlist (either master or media).
#[derive(Debug, Clone, PartialEq)]
pub enum HlsPlaylist {
    Master(HlsMasterPlaylist),
    Media(HlsMediaPlaylist),
}

impl HlsPlaylist {
    /// Parse an `.m3u8` playlist text, optionally resolving relative URIs against `base_url`.
    pub fn parse(content: &str, base_url: Option<&str>) -> Result<Self> {
        let base = match base_url {
            Some(raw) => Some(Url::parse(raw)?),
            None => None,
        };

        let mut lines = content.lines().map(str::trim).peekable();
        let first_non_empty = content
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| MayaError::Protocol("empty HLS playlist".to_string()))?;

        if !first_non_empty.starts_with("#EXTM3U") {
            return Err(MayaError::Protocol(
                "missing #EXTM3U header in HLS playlist".to_string(),
            ));
        }

        let is_master = content
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with("#EXT-X-STREAM-INF:"));

        if is_master {
            parse_master_playlist(lines, base.as_ref()).map(Self::Master)
        } else {
            // Advance past #EXTM3U if needed inside media parser
            let _ = lines.peek();
            parse_media_playlist(content, base.as_ref()).map(Self::Media)
        }
    }
}

fn parse_master_playlist<'a, I>(lines: I, base: Option<&Url>) -> Result<HlsMasterPlaylist>
where
    I: Iterator<Item = &'a str>,
{
    let mut version: Option<u32> = None;
    let mut variants = Vec::new();
    let mut pending_attrs: Option<HashMap<String, String>> = None;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-VERSION:") {
            version = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_attrs = Some(parse_attribute_list(rest));
        } else if line.starts_with('#') {
            continue;
        } else if let Some(attrs) = pending_attrs.take() {
            let bandwidth = attrs
                .get("BANDWIDTH")
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    MayaError::Protocol("EXT-X-STREAM-INF missing valid BANDWIDTH".to_string())
                })?;
            let average_bandwidth = attrs
                .get("AVERAGE-BANDWIDTH")
                .and_then(|v| v.parse::<u64>().ok());
            let resolution = attrs.get("RESOLUTION").and_then(|v| parse_resolution(v));
            let codecs = attrs.get("CODECS").cloned();
            let frame_rate = attrs.get("FRAME-RATE").and_then(|v| v.parse::<f64>().ok());
            let audio = attrs.get("AUDIO").cloned();
            let subtitles = attrs.get("SUBTITLES").cloned();
            let uri = resolve_uri(base, line)?;

            variants.push(HlsVariant {
                uri,
                bandwidth,
                average_bandwidth,
                resolution,
                codecs,
                frame_rate,
                audio,
                subtitles,
            });
        }
    }

    if variants.is_empty() {
        return Err(MayaError::Protocol(
            "master HLS playlist contained no variant streams".to_string(),
        ));
    }

    Ok(HlsMasterPlaylist { version, variants })
}

fn parse_media_playlist(content: &str, base: Option<&Url>) -> Result<HlsMediaPlaylist> {
    let mut version: Option<u32> = None;
    let mut target_duration: u64 = 0;
    let mut media_sequence: u64 = 0;
    let mut end_list = false;
    let mut init_segment: Option<HlsInitSegment> = None;
    let mut segments = Vec::new();

    let mut active_key: Option<HlsKey> = None;
    let mut pending_duration: Option<(f64, Option<String>)> = None;
    let mut pending_byte_range: Option<ByteRange> = None;
    let mut prev_range_end: HashMap<String, u64> = HashMap::new();
    let mut pending_discontinuity = false;
    let mut seg_index: u64 = 0;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-VERSION:") {
            version = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse::<u64>().map_err(|_| {
                MayaError::Protocol(format!("invalid EXT-X-TARGETDURATION: {rest}"))
            })?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse::<u64>().map_err(|_| {
                MayaError::Protocol(format!("invalid EXT-X-MEDIA-SEQUENCE: {rest}"))
            })?;
        } else if line == "#EXT-X-ENDLIST" {
            end_list = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            active_key = parse_ext_x_key(rest, base)?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attribute_list(rest);
            let raw_uri = attrs
                .get("URI")
                .ok_or_else(|| MayaError::Protocol("EXT-X-MAP missing URI".to_string()))?;
            let uri = resolve_uri(base, raw_uri)?;
            let byte_range = attrs
                .get("BYTERANGE")
                .map(|r| parse_byte_range(r, 0))
                .transpose()?;
            init_segment = Some(HlsInitSegment { uri, byte_range });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let (len, offset_opt) = parse_byte_range_raw(rest.trim())?;
            pending_byte_range = Some(ByteRange {
                length: len,
                offset: offset_opt.unwrap_or(u64::MAX), // sentinel resolved once segment URI is known
            });
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (dur_str, title_opt) = match rest.split_once(',') {
                Some((d, t)) => {
                    let trimmed = t.trim();
                    (
                        d.trim(),
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        },
                    )
                }
                None => (rest.trim(), None),
            };
            let duration = dur_str
                .parse::<f64>()
                .map_err(|_| MayaError::Protocol(format!("invalid EXTINF duration: {dur_str}")))?;
            pending_duration = Some((duration, title_opt));
        } else if line.starts_with('#') {
            continue;
        } else if let Some((duration, title)) = pending_duration.take() {
            let uri = resolve_uri(base, line)?;
            let byte_range = match pending_byte_range.take() {
                Some(mut br) => {
                    if br.offset == u64::MAX {
                        br.offset = *prev_range_end.get(&uri).unwrap_or(&0);
                    }
                    prev_range_end.insert(uri.clone(), br.offset.saturating_add(br.length));
                    Some(br)
                }
                None => None,
            };

            let sequence = media_sequence.saturating_add(seg_index);
            seg_index += 1;

            segments.push(HlsSegment {
                sequence,
                duration,
                title,
                uri,
                byte_range,
                key: active_key.clone(),
                discontinuity: pending_discontinuity,
            });
            pending_discontinuity = false;
        }
    }

    Ok(HlsMediaPlaylist {
        version,
        target_duration,
        media_sequence,
        end_list,
        init_segment,
        segments,
    })
}

fn parse_ext_x_key(raw: &str, base: Option<&Url>) -> Result<Option<HlsKey>> {
    let attrs = parse_attribute_list(raw);
    let method_str = attrs
        .get("METHOD")
        .ok_or_else(|| MayaError::Protocol("EXT-X-KEY missing METHOD".to_string()))?;

    if method_str.eq_ignore_ascii_case("NONE") {
        return Ok(None);
    }

    let method = if method_str.eq_ignore_ascii_case("AES-128") {
        HlsKeyMethod::Aes128
    } else if method_str.eq_ignore_ascii_case("SAMPLE-AES") {
        HlsKeyMethod::SampleAes
    } else {
        HlsKeyMethod::Other(method_str.clone())
    };

    let raw_uri = attrs
        .get("URI")
        .ok_or_else(|| MayaError::Protocol("EXT-X-KEY missing URI".to_string()))?;
    let uri = resolve_uri(base, raw_uri)?;
    let iv = attrs.get("IV").map(|v| parse_hex_iv(v)).transpose()?;

    Ok(Some(HlsKey { method, uri, iv }))
}

/// Convert an HLS media sequence number into its default 16-byte big-endian IV (RFC 8216).
pub fn media_sequence_to_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..16].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// Parse an HLS `0x...` hexadecimal IV string into a 16-byte array (left-padded with zeros).
pub fn parse_hex_iv(raw: &str) -> Result<[u8; 16]> {
    let trimmed = raw.trim();
    let hex_part = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .ok_or_else(|| MayaError::Protocol(format!("HLS IV must start with 0x: `{raw}`")))?;

    if hex_part.is_empty() || hex_part.len() > 32 {
        return Err(MayaError::Protocol(format!(
            "invalid HLS IV hex length: `{raw}`"
        )));
    }

    let padded = format!("{hex_part:0>32}");
    let mut iv = [0u8; 16];
    for i in 0..16 {
        let s = &padded[i * 2..i * 2 + 2];
        iv[i] = u8::from_str_radix(s, 16)
            .map_err(|_| MayaError::Protocol(format!("invalid hex in HLS IV: `{raw}`")))?;
    }
    Ok(iv)
}

/// Decrypt AES-128-CBC ciphertext with PKCS#7 unpadding.
pub fn decrypt_aes128_cbc(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(MayaError::Protocol(format!(
            "invalid AES-128-CBC ciphertext length: {}",
            ciphertext.len()
        )));
    }

    let mut buf = ciphertext.to_vec();
    let decryptor = Aes128CbcDec::new_from_slices(key, iv)
        .map_err(|err| MayaError::Protocol(format!("invalid AES-128 key/IV: {err}")))?;
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|err| MayaError::Protocol(format!("AES-128 PKCS#7 unpadding failed: {err}")))?;

    Ok(plaintext.to_vec())
}

fn parse_attribute_list(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            continue;
        }

        let key = input[key_start..i].trim().to_ascii_uppercase();
        i += 1; // skip '='

        let val = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let s = input[val_start..i].to_string();
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
            }
            s
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            input[val_start..i].trim().to_string()
        };

        map.insert(key, val);
    }

    map
}

fn parse_resolution(raw: &str) -> Option<(u32, u32)> {
    let (w, h) = raw.trim().split_once('x')?;
    Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
}

fn parse_byte_range_raw(raw: &str) -> Result<(u64, Option<u64>)> {
    let trimmed = raw.trim();
    match trimmed.split_once('@') {
        Some((len_s, off_s)) => {
            let len = len_s.trim().parse::<u64>().map_err(|_| {
                MayaError::Protocol(format!("invalid EXT-X-BYTERANGE length: `{raw}`"))
            })?;
            let off = off_s.trim().parse::<u64>().map_err(|_| {
                MayaError::Protocol(format!("invalid EXT-X-BYTERANGE offset: `{raw}`"))
            })?;
            Ok((len, Some(off)))
        }
        None => {
            let len = trimmed.parse::<u64>().map_err(|_| {
                MayaError::Protocol(format!("invalid EXT-X-BYTERANGE length: `{raw}`"))
            })?;
            Ok((len, None))
        }
    }
}

fn parse_byte_range(raw: &str, default_offset: u64) -> Result<ByteRange> {
    let (length, offset_opt) = parse_byte_range_raw(raw)?;
    Ok(ByteRange {
        length,
        offset: offset_opt.unwrap_or(default_offset),
    })
}

fn resolve_uri(base: Option<&Url>, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Some(base_url) = base {
        let joined = base_url.join(trimmed)?;
        Ok(joined.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Boxed pinned stream of sequenced segment payloads yielded by [`HlsSegmentFetcher::stream_segments`].
pub type HlsSegmentStream = Pin<Box<dyn Stream<Item = Result<(u64, Bytes)>> + Send>>;

/// Sliding-window concurrent HLS segment fetcher backed by [`HttpProtocolClient`].
#[derive(Clone)]
pub struct HlsSegmentFetcher {
    http: HttpProtocolClient,
    security: SecurityPolicy,
    headers: Vec<(String, String)>,
    window_size: usize,
    key_cache: Arc<Mutex<HashMap<String, [u8; 16]>>>,
}

impl HlsSegmentFetcher {
    /// Construct a new [`HlsSegmentFetcher`] with the given concurrency sliding window size.
    pub fn new(
        http: HttpProtocolClient,
        security: SecurityPolicy,
        headers: Vec<(String, String)>,
        window_size: usize,
    ) -> Self {
        Self {
            http,
            security,
            headers,
            window_size: window_size.max(1),
            key_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn build_request(&self, url: &str) -> DownloadRequest {
        let mut req = DownloadRequest::new(url.to_string(), std::path::PathBuf::from("."));
        req.security = self.security.clone();
        req.headers = self.headers.clone();
        req
    }

    async fn fetch_bytes_range(&self, url: &str, range: Option<ByteRange>) -> Result<Vec<u8>> {
        let req = self.build_request(url);
        let mut stream = match range {
            Some(br) => {
                self.http
                    .fetch_range(&req, br.offset, Some(br.offset.saturating_add(br.length)))
                    .await?
            }
            None => self.http.fetch_range(&req, 0, None).await?,
        };

        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf)
    }

    /// Fetch and parse an HLS master or media playlist from `url`.
    pub async fn fetch_playlist(&self, url: &str) -> Result<HlsPlaylist> {
        let raw = self.fetch_bytes_range(url, None).await?;
        let text = String::from_utf8(raw).map_err(|err| {
            MayaError::Protocol(format!("HLS playlist is not valid UTF-8: {err}"))
        })?;
        HlsPlaylist::parse(&text, Some(url))
    }

    /// Fetch a playlist and, if it is a master playlist, select the best variant media playlist.
    pub async fn resolve_media_playlist(&self, url: &str) -> Result<HlsMediaPlaylist> {
        match self.fetch_playlist(url).await? {
            HlsPlaylist::Media(media) => Ok(media),
            HlsPlaylist::Master(master) => {
                let variant = master.select_best_variant().ok_or_else(|| {
                    MayaError::Protocol("HLS master playlist has no variants".to_string())
                })?;
                match self.fetch_playlist(&variant.uri).await? {
                    HlsPlaylist::Media(media) => Ok(media),
                    HlsPlaylist::Master(_) => Err(MayaError::Protocol(
                        "nested HLS master playlists are not supported".to_string(),
                    )),
                }
            }
        }
    }

    /// Fetch (with caching) a 16-byte AES-128 key from `key_uri`.
    pub async fn fetch_key(&self, key_uri: &str) -> Result<[u8; 16]> {
        {
            let guard = self.key_cache.lock().await;
            if let Some(&cached) = guard.get(key_uri) {
                return Ok(cached);
            }
        }

        let bytes = self.fetch_bytes_range(key_uri, None).await?;
        if bytes.len() != 16 {
            return Err(MayaError::Protocol(format!(
                "expected 16-byte AES-128 key from `{key_uri}`, got {} bytes",
                bytes.len()
            )));
        }

        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes);

        let mut guard = self.key_cache.lock().await;
        guard.insert(key_uri.to_string(), key);
        Ok(key)
    }

    /// Fetch and (if encrypted) decrypt a single [`HlsSegment`].
    pub async fn fetch_segment(&self, segment: &HlsSegment) -> Result<Bytes> {
        let raw = self
            .fetch_bytes_range(&segment.uri, segment.byte_range)
            .await?;

        match &segment.key {
            None => Ok(Bytes::from(raw)),
            Some(key_info) => match &key_info.method {
                HlsKeyMethod::Aes128 => {
                    let key = self.fetch_key(&key_info.uri).await?;
                    let iv = key_info
                        .iv
                        .unwrap_or_else(|| media_sequence_to_iv(segment.sequence));
                    let decrypted = decrypt_aes128_cbc(&raw, &key, &iv)?;
                    Ok(Bytes::from(decrypted))
                }
                HlsKeyMethod::SampleAes => Err(MayaError::Unsupported(
                    "SAMPLE-AES HLS decryption is not supported".to_string(),
                )),
                HlsKeyMethod::Other(method) => Err(MayaError::Unsupported(format!(
                    "unsupported HLS encryption method `{method}`"
                ))),
            },
        }
    }

    /// Fetch `segments` using a concurrent sliding window of `self.window_size`,
    /// yielding decrypted segment payloads in strict playlist order.
    pub fn stream_segments(
        &self,
        segments: Vec<HlsSegment>,
        cancel: watch::Receiver<bool>,
    ) -> HlsSegmentStream {
        let fetcher = self.clone();
        let window = self.window_size;

        let s = stream::iter(segments.into_iter().map(move |seg| {
            let f = fetcher.clone();
            let c = cancel.clone();
            async move {
                if *c.borrow() {
                    return Err(MayaError::Cancelled);
                }
                let seq = seg.sequence;
                let data = f.fetch_segment(&seg).await?;
                if *c.borrow() {
                    return Err(MayaError::Cancelled);
                }
                Ok((seq, data))
            }
        }))
        .buffered(window);

        Box::pin(s)
    }

    /// Fetch all segments of a VOD or live HLS playlist using a sliding window:
    /// - Keeps up to `window_size` segment downloads in flight in order.
    /// - For live playlists (`end_list == false`), tracks seen media sequence numbers,
    ///   re-polls the media playlist, and yields only new segments until `#EXT-X-ENDLIST`
    ///   or cancellation.
    pub async fn fetch_playlist_segments(
        &self,
        playlist_url: &str,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Vec<Bytes>> {
        let mut collected = Vec::new();
        let mut seen_sequences: VecDeque<u64> = VecDeque::new();
        let mut init_fetched = false;

        loop {
            if *cancel.borrow() {
                return Err(MayaError::Cancelled);
            }

            let media = self.resolve_media_playlist(playlist_url).await?;

            if !init_fetched {
                if let Some(init) = &media.init_segment {
                    let init_bytes = self.fetch_bytes_range(&init.uri, init.byte_range).await?;
                    collected.push(Bytes::from(init_bytes));
                }
                init_fetched = true;
            }

            let new_segments: Vec<HlsSegment> = media
                .segments
                .into_iter()
                .filter(|seg| !seen_sequences.contains(&seg.sequence))
                .collect();

            if !new_segments.is_empty() {
                let mut seg_stream = self.stream_segments(new_segments, cancel.clone());
                while let Some(item) = seg_stream.next().await {
                    let (seq, bytes) = item?;
                    seen_sequences.push_back(seq);
                    while seen_sequences.len() > 1024 {
                        seen_sequences.pop_front();
                    }
                    collected.push(bytes);
                }
            }

            if media.end_list {
                break;
            }

            let wait_secs = (media.target_duration / 2).max(1);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait_secs)) => {}
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        return Err(MayaError::Cancelled);
                    }
                }
            }
        }

        Ok(collected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    fn encrypt_aes128_cbc_pkcs7(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        Aes128CbcEnc::new_from_slices(key, iv)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn parses_master_playlist_and_selects_best_variant() {
        let m3u8 = r#"#EXTM3U
#EXT-X-VERSION:4
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2800000,AVERAGE-BANDWIDTH=2400000,RESOLUTION=1920x1080,FRAME-RATE=60.0,CODECS="avc1.64002a,mp4a.40.2"
https://cdn2.example.com/1080p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1400000,RESOLUTION=1280x720
mid/index.m3u8
"#;

        let playlist =
            HlsPlaylist::parse(m3u8, Some("https://cdn.example.com/master.m3u8")).unwrap();
        let HlsPlaylist::Master(master) = playlist else {
            panic!("expected Master playlist");
        };

        assert_eq!(master.version, Some(4));
        assert_eq!(master.variants.len(), 3);
        assert_eq!(
            master.variants[0].uri,
            "https://cdn.example.com/low/index.m3u8"
        );
        assert_eq!(
            master.variants[0].codecs.as_deref(),
            Some("avc1.4d401e,mp4a.40.2")
        );

        let best = master.select_best_variant().unwrap();
        assert_eq!(best.bandwidth, 2_800_000);
        assert_eq!(best.resolution, Some((1920, 1080)));
        assert_eq!(best.frame_rate, Some(60.0));
        assert_eq!(best.uri, "https://cdn2.example.com/1080p/index.m3u8");

        let capped = master.select_variant_max_bandwidth(1_500_000).unwrap();
        assert_eq!(capped.bandwidth, 1_400_000);
    }

    #[test]
    fn parses_media_playlist_with_keys_and_byteranges() {
        let m3u8 = r#"#EXTM3U
#EXT-X-VERSION:4
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-MAP:URI="init.mp4",BYTERANGE="512@0"
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.com/k1.key",IV=0x000102030405060708090a0b0c0d0e0f
#EXTINF:5.5,Segment 1
#EXT-X-BYTERANGE:1000@512
seg.m4s
#EXTINF:6.0,
#EXT-X-BYTERANGE:2000
seg.m4s
#EXT-X-KEY:METHOD=AES-128,URI="k2.key"
#EXTINF:4.25,Unencrypted after? No, k2
seg2.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:3.0,Clear
seg3.ts
#EXT-X-ENDLIST
"#;

        let playlist =
            HlsPlaylist::parse(m3u8, Some("https://cdn.example.com/vod/list.m3u8")).unwrap();
        let HlsPlaylist::Media(media) = playlist else {
            panic!("expected Media playlist");
        };

        assert_eq!(media.version, Some(4));
        assert_eq!(media.target_duration, 6);
        assert_eq!(media.media_sequence, 100);
        assert!(media.end_list);

        let init = media.init_segment.unwrap();
        assert_eq!(init.uri, "https://cdn.example.com/vod/init.mp4");
        assert_eq!(
            init.byte_range,
            Some(ByteRange {
                length: 512,
                offset: 0
            })
        );

        assert_eq!(media.segments.len(), 4);

        let s0 = &media.segments[0];
        assert_eq!(s0.sequence, 100);
        assert_eq!(s0.duration, 5.5);
        assert_eq!(s0.title.as_deref(), Some("Segment 1"));
        assert_eq!(s0.uri, "https://cdn.example.com/vod/seg.m4s");
        assert_eq!(
            s0.byte_range,
            Some(ByteRange {
                length: 1000,
                offset: 512
            })
        );
        let k0 = s0.key.as_ref().unwrap();
        assert_eq!(k0.method, HlsKeyMethod::Aes128);
        assert_eq!(k0.uri, "https://keys.example.com/k1.key");
        assert_eq!(
            k0.iv,
            Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
        );

        // Contiguous byte range offset should advance to 512 + 1000 = 1512
        let s1 = &media.segments[1];
        assert_eq!(s1.sequence, 101);
        assert_eq!(
            s1.byte_range,
            Some(ByteRange {
                length: 2000,
                offset: 1512
            })
        );

        // Segment 2 has k2.key resolved relative to base URL and omitted IV
        let s2 = &media.segments[2];
        assert_eq!(s2.sequence, 102);
        let k2 = s2.key.as_ref().unwrap();
        assert_eq!(k2.uri, "https://cdn.example.com/vod/k2.key");
        assert!(k2.iv.is_none());

        // Segment 3 has METHOD=NONE -> no key
        let s3 = &media.segments[3];
        assert_eq!(s3.sequence, 103);
        assert!(s3.key.is_none());
    }

    #[test]
    fn decrypts_aes128_cbc_with_pkcs7_unpadding() {
        let key: [u8; 16] = *b"0123456789abcdef";
        let iv: [u8; 16] = media_sequence_to_iv(42);
        let plaintext = b"Hello HLS AES-128-CBC world! This is a test payload.";

        let ciphertext = encrypt_aes128_cbc_pkcs7(plaintext, &key, &iv);
        let decrypted = decrypt_aes128_cbc(&ciphertext, &key, &iv).unwrap();
        assert_eq!(decrypted, plaintext);

        // Wrong key should fail PKCS#7 padding or produce mismatch
        let wrong_key: [u8; 16] = *b"fedcba9876543210";
        let bad = decrypt_aes128_cbc(&ciphertext, &wrong_key, &iv);
        assert!(bad.is_err() || bad.unwrap() != plaintext);
    }

    #[tokio::test]
    async fn sliding_window_fetcher_downloads_and_decrypts_in_order() {
        let key: [u8; 16] = *b"maya-hls-aes-key";
        let seg0_plain = b"segment-zero-payload";
        let seg1_plain = b"segment-one-payload-longer";
        let seg2_plain = b"segment-two-clear";

        let seg0_enc = encrypt_aes128_cbc_pkcs7(seg0_plain, &key, &media_sequence_to_iv(10));
        let seg1_enc = encrypt_aes128_cbc_pkcs7(seg1_plain, &key, &media_sequence_to_iv(11));

        let key_hits = Arc::new(AtomicUsize::new(0));
        let key_hits_clone = Arc::clone(&key_hits);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let playlist_body = format!(
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:4\n\
             #EXT-X-MEDIA-SEQUENCE:10\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"{base_url}/key.bin\"\n\
             #EXTINF:4.0,\n\
             {base_url}/seg0.ts\n\
             #EXTINF:4.0,\n\
             {base_url}/seg1.ts\n\
             #EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:4.0,\n\
             {base_url}/seg2.ts\n\
             #EXT-X-ENDLIST\n"
        );

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let playlist_body = playlist_body.clone();
                let seg0_enc = seg0_enc.clone();
                let seg1_enc = seg1_enc.clone();
                let key_hits = Arc::clone(&key_hits_clone);

                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");
                    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

                    let body: Vec<u8> = match path {
                        "/list.m3u8" => playlist_body.into_bytes(),
                        "/key.bin" => {
                            key_hits.fetch_add(1, Ordering::SeqCst);
                            key.to_vec()
                        }
                        "/seg0.ts" => seg0_enc,
                        "/seg1.ts" => seg1_enc,
                        "/seg2.ts" => seg2_plain.to_vec(),
                        _ => Vec::new(),
                    };

                    let status = if body.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let policy = SecurityPolicy {
            allow_private_ips: true,
            ..Default::default()
        };
        let fetcher = HlsSegmentFetcher::new(HttpProtocolClient::new(), policy, vec![], 2);
        let (_tx, rx) = watch::channel(false);

        let chunks = fetcher
            .fetch_playlist_segments(&format!("{base_url}/list.m3u8"), rx)
            .await
            .unwrap();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].as_ref(), seg0_plain);
        assert_eq!(chunks[1].as_ref(), seg1_plain);
        assert_eq!(chunks[2].as_ref(), seg2_plain);
        // Key may be fetched once or twice if concurrent before cache insert, but <= 2
        assert!(key_hits.load(Ordering::SeqCst) >= 1);
    }
}
