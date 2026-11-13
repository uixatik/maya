use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use futures::Stream;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};

use crate::error::{MayaError, Result};
use crate::types::{
    ByteStream, ChecksumAlgorithm, ChecksumSpec, DownloadRequest, MetalinkFile, MirrorSource,
    ProbeResult, ProtocolClient, ProtocolKind,
};

const DEFAULT_BASE_PENALTY: Duration = Duration::from_secs(1);
const DEFAULT_MAX_PENALTY: Duration = Duration::from_secs(60);
const DEFAULT_CONCURRENCY_PENALTY_MS: f64 = 25.0;
const DEFAULT_UNKNOWN_LATENCY_MS: f64 = 100.0;

/// Parsed Metalink document (`.meta4` RFC 5854 or `.metalink` v3.0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metalink {
    pub files: Vec<MetalinkFile>,
}

impl Metalink {
    /// Parse a Metalink XML document (`.meta4` RFC 5854 or `.metalink` v3.0) into
    /// file entries with URLs, priorities, and checksums.
    pub fn parse_xml(xml: &str) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut buf = Vec::new();
        let mut files: Vec<MetalinkFile> = Vec::new();
        let mut current_file: Option<MetalinkFile> = None;

        // Track nested tag context inside `<file>`.
        let mut tag_stack: Vec<String> = Vec::new();
        let mut pending_hash_algo: Option<ChecksumAlgorithm> = None;
        let mut pending_url: Option<UrlBuilder> = None;
        let mut text_acc = String::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let local = local_name_string(e.name().as_ref());
                    tag_stack.push(local.clone());
                    text_acc.clear();

                    match local.as_str() {
                        "file" => {
                            let mut name = String::new();
                            for attr in e.attributes() {
                                let attr = attr.map_err(|err| {
                                    MayaError::Protocol(format!(
                                        "invalid metalink attribute: {err}"
                                    ))
                                })?;
                                if local_name_string(attr.key.as_ref()) == "name" {
                                    name = attr
                                        .unescape_value()
                                        .map_err(|err| {
                                            MayaError::Protocol(format!(
                                                "invalid metalink attribute value: {err}"
                                            ))
                                        })?
                                        .into_owned();
                                }
                            }
                            current_file = Some(MetalinkFile {
                                name,
                                size: None,
                                checksums: Vec::new(),
                                urls: Vec::new(),
                            });
                        }
                        "hash" if current_file.is_some() => {
                            pending_hash_algo = None;
                            for attr in e.attributes() {
                                let attr = attr.map_err(|err| {
                                    MayaError::Protocol(format!("invalid hash attribute: {err}"))
                                })?;
                                if local_name_string(attr.key.as_ref()) == "type" {
                                    let val = attr
                                        .unescape_value()
                                        .map_err(|err| {
                                            MayaError::Protocol(format!(
                                                "invalid hash type value: {err}"
                                            ))
                                        })?
                                        .into_owned();
                                    pending_hash_algo = parse_metalink_hash_type(&val);
                                }
                            }
                        }
                        "url" if current_file.is_some() => {
                            let mut builder = UrlBuilder::default();
                            for attr in e.attributes() {
                                let attr = attr.map_err(|err| {
                                    MayaError::Protocol(format!("invalid url attribute: {err}"))
                                })?;
                                let key = local_name_string(attr.key.as_ref());
                                let val = attr
                                    .unescape_value()
                                    .map_err(|err| {
                                        MayaError::Protocol(format!(
                                            "invalid url attribute value: {err}"
                                        ))
                                    })?
                                    .into_owned();
                                match key.as_str() {
                                    "priority" => {
                                        if let Ok(p) = val.trim().parse::<u32>() {
                                            builder.priority = Some(p.max(1));
                                        }
                                    }
                                    "preference" => {
                                        // Metalink v3 preference: 0..=100 (higher is better).
                                        if let Ok(pref) = val.trim().parse::<u32>() {
                                            let normalized = 101u32.saturating_sub(pref.min(100));
                                            builder.priority = Some(normalized.max(1));
                                        }
                                    }
                                    "location" => {
                                        let trimmed = val.trim();
                                        if !trimmed.is_empty() {
                                            builder.location = Some(trimmed.to_string());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            pending_url = Some(builder);
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(ref e)) => {
                    let decoded = e.unescape().map_err(|err| {
                        MayaError::Protocol(format!("invalid metalink XML text: {err}"))
                    })?;
                    text_acc.push_str(&decoded);
                }
                Ok(Event::CData(ref e)) => {
                    let text = std::str::from_utf8(e.as_ref()).map_err(|err| {
                        MayaError::Protocol(format!("invalid UTF-8 in metalink CDATA: {err}"))
                    })?;
                    text_acc.push_str(text);
                }
                Ok(Event::End(ref e)) => {
                    let local = local_name_string(e.name().as_ref());
                    let text = text_acc.trim().to_string();
                    text_acc.clear();

                    match local.as_str() {
                        "size" => {
                            if let Some(file) = current_file.as_mut() {
                                if !text.is_empty() {
                                    let size = text.parse::<u64>().map_err(|err| {
                                        MayaError::Protocol(format!(
                                            "invalid metalink file size `{text}`: {err}"
                                        ))
                                    })?;
                                    file.size = Some(size);
                                }
                            }
                        }
                        "identity" => {
                            if let Some(file) = current_file.as_mut() {
                                if file.name.is_empty() && !text.is_empty() {
                                    file.name = text;
                                }
                            }
                        }
                        "hash" => {
                            if let (Some(file), Some(algorithm)) =
                                (current_file.as_mut(), pending_hash_algo.take())
                            {
                                if !text.is_empty() {
                                    file.checksums.push(ChecksumSpec {
                                        algorithm,
                                        expected_hex: text.to_ascii_lowercase(),
                                    });
                                }
                            }
                        }
                        "url" => {
                            if let (Some(file), Some(builder)) =
                                (current_file.as_mut(), pending_url.take())
                            {
                                if !text.is_empty() {
                                    file.urls.push(MirrorSource {
                                        url: text,
                                        priority: builder.priority.unwrap_or(100),
                                        location: builder.location,
                                    });
                                }
                            }
                        }
                        "file" => {
                            if let Some(mut file) = current_file.take() {
                                file.urls.sort_by_key(|u| u.priority);
                                files.push(file);
                            }
                        }
                        _ => {}
                    }

                    tag_stack.pop();
                }
                Ok(Event::Empty(ref e)) => {
                    let local = local_name_string(e.name().as_ref());
                    if local == "file" {
                        let mut name = String::new();
                        for attr in e.attributes() {
                            let attr = attr.map_err(|err| {
                                MayaError::Protocol(format!("invalid metalink attribute: {err}"))
                            })?;
                            if local_name_string(attr.key.as_ref()) == "name" {
                                name = attr
                                    .unescape_value()
                                    .map_err(|err| {
                                        MayaError::Protocol(format!(
                                            "invalid metalink attribute value: {err}"
                                        ))
                                    })?
                                    .into_owned();
                            }
                        }
                        files.push(MetalinkFile {
                            name,
                            size: None,
                            checksums: Vec::new(),
                            urls: Vec::new(),
                        });
                    }
                }
                Ok(Event::Eof) => break,
                Err(err) => {
                    return Err(MayaError::Protocol(format!(
                        "metalink XML parse error: {err}"
                    )));
                }
                _ => {}
            }
            buf.clear();
        }

        if files.is_empty() {
            return Err(MayaError::Protocol(
                "metalink XML contains no <file> entries".to_string(),
            ));
        }

        Ok(Self { files })
    }

    /// Return the first file entry in the parsed Metalink document, if any.
    pub fn primary_file(&self) -> Option<&MetalinkFile> {
        self.files.first()
    }
}

#[derive(Default)]
struct UrlBuilder {
    priority: Option<u32>,
    location: Option<String>,
}

fn local_name_string(raw: &[u8]) -> String {
    let s = std::str::from_utf8(raw).unwrap_or("");
    let local = s.rsplit(':').next().unwrap_or(s);
    local.to_ascii_lowercase()
}

fn parse_metalink_hash_type(raw: &str) -> Option<ChecksumAlgorithm> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sha-256" | "sha256" => Some(ChecksumAlgorithm::Sha256),
        "sha-512" | "sha512" => Some(ChecksumAlgorithm::Sha512),
        "md5" => Some(ChecksumAlgorithm::Md5),
        "blake3" => Some(ChecksumAlgorithm::Blake3),
        _ => None,
    }
}

/// Configuration for [`MirrorPool`] scoring, penalties, and concurrency weighting.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorConfig {
    /// Initial penalty cooldown duration on first failure.
    pub base_penalty: Duration,
    /// Upper bound on exponential failure backoff duration.
    pub max_penalty: Duration,
    /// Effective latency penalty added per concurrent active request on a mirror.
    pub concurrency_penalty_ms: f64,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            base_penalty: DEFAULT_BASE_PENALTY,
            max_penalty: DEFAULT_MAX_PENALTY,
            concurrency_penalty_ms: DEFAULT_CONCURRENCY_PENALTY_MS,
        }
    }
}

/// Summary returned after probing all mirrors in a [`MirrorPool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSummary {
    /// Consensus `Content-Length` across responsive mirrors, if known.
    pub consensus_size: Option<u64>,
    /// Consensus `ETag` across responsive mirrors, if present.
    pub consensus_etag: Option<String>,
    /// Whether range requests are supported by at least one healthy mirror.
    pub supports_ranges: bool,
    /// Number of responsive mirrors consistent with consensus metadata.
    pub responsive_mirrors: usize,
    /// Number of mirrors disqualified due to `Content-Length` or `ETag` mismatch.
    pub mismatched_mirrors: usize,
    /// Number of mirrors whose probe failed with a network or HTTP error.
    pub failed_mirrors: usize,
}

/// Point-in-time snapshot of a mirror's health, latency, and routing score.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorSnapshot {
    pub source: MirrorSource,
    pub latency: Option<Duration>,
    pub score: f64,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub supports_ranges: bool,
    pub failures: u32,
    pub is_penalized: bool,
    pub mismatched: bool,
    pub active_requests: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct MirrorEntry {
    source: MirrorSource,
    latency: Option<Duration>,
    content_length: Option<u64>,
    etag: Option<String>,
    supports_ranges: bool,
    failures: u32,
    penalized_until: Option<Instant>,
    active_requests: usize,
    mismatched: bool,
    last_error: Option<String>,
    last_probe: Option<ProbeResult>,
}

impl MirrorEntry {
    fn new(source: MirrorSource) -> Self {
        Self {
            source,
            latency: None,
            content_length: None,
            etag: None,
            supports_ranges: true,
            failures: 0,
            penalized_until: None,
            active_requests: 0,
            mismatched: false,
            last_error: None,
            last_probe: None,
        }
    }

    fn effective_score(&self, config: &MirrorConfig) -> f64 {
        if self.mismatched {
            return f64::INFINITY;
        }
        let base_ms = self
            .latency
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(DEFAULT_UNKNOWN_LATENCY_MS);
        let priority_bias = (self.source.priority as f64) * 0.5;
        let concurrency_cost = (self.active_requests as f64) * config.concurrency_penalty_ms;
        let failure_multiplier = 2.0_f64.powi(self.failures.min(16) as i32);
        (base_ms + priority_bias + concurrency_cost) * failure_multiplier
    }
}

#[derive(Debug)]
struct PoolInner {
    entries: Vec<MirrorEntry>,
    config: MirrorConfig,
    expected_size: Option<u64>,
    expected_etag: Option<String>,
}

/// Thread-safe pool of mirror URLs with parallel probing, metadata consistency checks,
/// latency-based scoring, and exponential failure/rate-limit backoff.
#[derive(Debug, Clone)]
pub struct MirrorPool {
    inner: Arc<Mutex<PoolInner>>,
}

impl MirrorPool {
    /// Construct a [`MirrorPool`] from a non-empty list of [`MirrorSource`] descriptors.
    pub fn new(sources: Vec<MirrorSource>) -> Result<Self> {
        if sources.is_empty() {
            return Err(MayaError::InvalidConfig(
                "mirror pool requires at least one mirror URL".to_string(),
            ));
        }

        let mut deduped: Vec<MirrorSource> = Vec::with_capacity(sources.len());
        let mut seen: HashMap<String, usize> = HashMap::new();
        for src in sources {
            let trimmed = src.url.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(&idx) = seen.get(trimmed) {
                if src.priority < deduped[idx].priority {
                    deduped[idx].priority = src.priority;
                }
                if deduped[idx].location.is_none() {
                    deduped[idx].location = src.location;
                }
            } else {
                seen.insert(trimmed.to_string(), deduped.len());
                deduped.push(MirrorSource {
                    url: trimmed.to_string(),
                    priority: src.priority,
                    location: src.location,
                });
            }
        }

        if deduped.is_empty() {
            return Err(MayaError::InvalidConfig(
                "mirror pool contains no valid non-empty URLs".to_string(),
            ));
        }

        deduped.sort_by_key(|s| s.priority);
        let entries = deduped.into_iter().map(MirrorEntry::new).collect();

        Ok(Self {
            inner: Arc::new(Mutex::new(PoolInner {
                entries,
                config: MirrorConfig::default(),
                expected_size: None,
                expected_etag: None,
            })),
        })
    }

    /// Construct a [`MirrorPool`] from plain URL strings (assigned default priority `100`).
    pub fn from_urls<I, S>(urls: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let sources = urls.into_iter().map(MirrorSource::new).collect();
        Self::new(sources)
    }

    /// Construct a [`MirrorPool`] from a parsed [`MetalinkFile`], preserving URLs,
    /// priorities, and declared file size.
    pub fn from_metalink_file(file: &MetalinkFile) -> Result<Self> {
        let pool = Self::new(file.urls.clone())?;
        if let Some(size) = file.size {
            let mut guard = pool.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.expected_size = Some(size);
        }
        Ok(pool)
    }

    /// Override scoring and exponential backoff configuration.
    pub fn with_config(self, config: MirrorConfig) -> Self {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.config = config;
        }
        self
    }

    /// Enforce an expected `Content-Length` across all mirrors during probing.
    pub fn with_expected_size(self, size: Option<u64>) -> Self {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.expected_size = size;
        }
        self
    }

    /// Enforce an expected `ETag` across all mirrors during probing.
    pub fn with_expected_etag(self, etag: Option<String>) -> Self {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.expected_etag = etag;
        }
        self
    }

    /// Return the number of mirrors in the pool.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .len()
    }

    /// Return `true` if the pool contains zero mirrors.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Probe all mirrors in parallel using `client` (which sends `HEAD` with
    /// `GET Range: bytes=0-0` fallback), measuring latency and verifying that
    /// `Content-Length` and `ETag` match across mirrors.
    pub async fn probe_all(
        &self,
        client: &dyn ProtocolClient,
        base_request: &DownloadRequest,
    ) -> Result<ProbeSummary> {
        self.probe_with(|url| {
            let mut req = base_request.clone();
            req.source = url;
            async move { client.probe(&req).await }
        })
        .await
    }

    /// Parallel probe implementation parameterized by an async probe closure (useful for
    /// unit testing without real sockets).
    pub async fn probe_with<F, Fut>(&self, probe_fn: F) -> Result<ProbeSummary>
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: Future<Output = Result<ProbeResult>> + Send,
    {
        let urls: Vec<String> = {
            let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.entries.iter().map(|e| e.source.url.clone()).collect()
        };

        let futures = urls.into_iter().enumerate().map(|(idx, url)| {
            let fut = probe_fn(url);
            async move {
                let start = Instant::now();
                let res = fut.await;
                let elapsed = start.elapsed();
                (idx, elapsed, res)
            }
        });

        let outcomes = join_all(futures).await;
        let now = Instant::now();

        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let configured_size = guard.expected_size;
        let configured_etag = guard.expected_etag.clone();

        let mut succeeded: Vec<(usize, Duration, ProbeResult)> = Vec::new();
        let mut failed_mirrors = 0usize;

        let config = guard.config.clone();
        for (idx, elapsed, res) in outcomes {
            match res {
                Ok(probe) => {
                    succeeded.push((idx, elapsed, probe));
                }
                Err(err) => {
                    failed_mirrors += 1;
                    let status = match &err {
                        MayaError::HttpStatus { status, .. } => Some(*status),
                        _ => None,
                    };
                    if let Some(entry) = guard.entries.get_mut(idx) {
                        apply_failure(entry, &config, status, Some(err.to_string()), now);
                    }
                }
            }
        }

        if succeeded.is_empty() {
            return Err(MayaError::Network(
                "all mirrors failed during probe".to_string(),
            ));
        }

        // Determine consensus Content-Length (configured value takes precedence, else majority).
        let consensus_size = configured_size.or_else(|| {
            let mut counts: HashMap<u64, (usize, u32, Duration)> = HashMap::new();
            for (idx, elapsed, probe) in &succeeded {
                if let Some(size) = probe.total_size {
                    let priority = guard.entries[*idx].source.priority;
                    counts
                        .entry(size)
                        .and_modify(|(cnt, best_prio, best_lat)| {
                            *cnt += 1;
                            if priority < *best_prio {
                                *best_prio = priority;
                            }
                            if *elapsed < *best_lat {
                                *best_lat = *elapsed;
                            }
                        })
                        .or_insert((1, priority, *elapsed));
                }
            }
            counts
                .into_iter()
                .max_by(|a, b| {
                    a.1 .0
                        .cmp(&b.1 .0)
                        .then_with(|| b.1 .1.cmp(&a.1 .1))
                        .then_with(|| b.1 .2.cmp(&a.1 .2))
                })
                .map(|(size, _)| size)
        });

        // Determine consensus ETag across mirrors whose size is consistent with consensus_size.
        let consensus_etag = configured_etag.or_else(|| {
            let mut counts: HashMap<String, (usize, u32, Duration)> = HashMap::new();
            for (idx, elapsed, probe) in &succeeded {
                if let (Some(exp), Some(actual)) = (consensus_size, probe.total_size) {
                    if exp != actual {
                        continue;
                    }
                }
                if let Some(etag) = probe
                    .etag
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let priority = guard.entries[*idx].source.priority;
                    counts
                        .entry(etag.to_string())
                        .and_modify(|(cnt, best_prio, best_lat)| {
                            *cnt += 1;
                            if priority < *best_prio {
                                *best_prio = priority;
                            }
                            if *elapsed < *best_lat {
                                *best_lat = *elapsed;
                            }
                        })
                        .or_insert((1, priority, *elapsed));
                }
            }
            counts
                .into_iter()
                .max_by(|a, b| {
                    a.1 .0
                        .cmp(&b.1 .0)
                        .then_with(|| b.1 .1.cmp(&a.1 .1))
                        .then_with(|| b.1 .2.cmp(&a.1 .2))
                })
                .map(|(etag, _)| etag)
        });

        let mut responsive_mirrors = 0usize;
        let mut mismatched_mirrors = 0usize;
        let mut supports_ranges = false;

        for (idx, elapsed, probe) in succeeded {
            let entry = &mut guard.entries[idx];
            let size_mismatch = match (consensus_size, probe.total_size) {
                (Some(expected), Some(actual)) => expected != actual,
                (Some(_), None) => true,
                _ => false,
            };
            let etag_mismatch = match (consensus_etag.as_deref(), probe.etag.as_deref()) {
                (Some(expected), Some(actual)) => expected != actual.trim(),
                _ => false,
            };

            if size_mismatch || etag_mismatch {
                mismatched_mirrors += 1;
                entry.mismatched = true;
                entry.content_length = probe.total_size;
                entry.etag = probe.etag.clone();
                entry.last_error = Some(if size_mismatch {
                    format!(
                        "Content-Length mismatch: expected {:?}, got {:?}",
                        consensus_size, probe.total_size
                    )
                } else {
                    format!(
                        "ETag mismatch: expected {:?}, got {:?}",
                        consensus_etag, probe.etag
                    )
                });
            } else {
                responsive_mirrors += 1;
                if probe.supports_ranges {
                    supports_ranges = true;
                }
                entry.mismatched = false;
                entry.latency = Some(elapsed);
                entry.content_length = probe.total_size;
                entry.etag = probe.etag.clone();
                entry.supports_ranges = probe.supports_ranges;
                entry.failures = 0;
                entry.penalized_until = None;
                entry.last_error = None;
                entry.last_probe = Some(probe);
            }
        }

        guard.expected_size = consensus_size;
        if guard.expected_etag.is_none() {
            guard.expected_etag = consensus_etag.clone();
        }

        if responsive_mirrors == 0 {
            return Err(MayaError::Protocol(
                "all responsive mirrors failed Content-Length / ETag consistency checks"
                    .to_string(),
            ));
        }

        Ok(ProbeSummary {
            consensus_size,
            consensus_etag,
            supports_ranges,
            responsive_mirrors,
            mismatched_mirrors,
            failed_mirrors,
        })
    }

    /// Select the URL of the highest-scoring (fastest, unpenalized, consistent) mirror.
    ///
    /// Returns `None` if all consistent mirrors are currently penalized.
    pub fn select_mirror(&self) -> Option<String> {
        let now = Instant::now();
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        pick_best_index(&guard, &HashSet::new(), now, false)
            .map(|idx| guard.entries[idx].source.url.clone())
    }

    /// Acquire a lease on the best available mirror, incrementing its `active_requests`
    /// counter until the returned [`MirrorLease`] is dropped.
    pub fn acquire_mirror(&self) -> Option<MirrorLease> {
        self.acquire_mirror_excluding(&HashSet::new(), false)
    }

    fn acquire_mirror_excluding(
        &self,
        excluded_urls: &HashSet<String>,
        allow_penalized_fallback: bool,
    ) -> Option<MirrorLease> {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let idx = pick_best_index(&guard, excluded_urls, now, allow_penalized_fallback)?;
        guard.entries[idx].active_requests += 1;
        let url = guard.entries[idx].source.url.clone();
        Some(MirrorLease {
            pool: self.clone(),
            url,
        })
    }

    /// Record a successful response from `url`, resetting its failure penalty and
    /// updating its exponentially smoothed latency estimate.
    pub fn record_success(&self, url: &str, latency: Duration) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.source.url == url) {
            entry.failures = 0;
            entry.penalized_until = None;
            entry.last_error = None;
            entry.latency = Some(match entry.latency {
                Some(prev) => {
                    let smoothed = prev.as_secs_f64() * 0.7 + latency.as_secs_f64() * 0.3;
                    Duration::from_secs_f64(smoothed)
                }
                None => latency,
            });
        }
    }

    /// Record a failed request or rate-limit/service-unavailable status (e.g. HTTP 429/503)
    /// for `url`, applying exponential backoff penalty.
    pub fn record_failure(&self, url: &str, status: Option<u16>, reason: Option<String>) {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let config = guard.config.clone();
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.source.url == url) {
            apply_failure(entry, &config, status, reason, now);
        }
    }

    /// Record a [`MayaError`] encountered while talking to `url`.
    pub fn record_error(&self, url: &str, err: &MayaError) {
        let status = match err {
            MayaError::HttpStatus { status, .. } => Some(*status),
            _ => None,
        };
        self.record_failure(url, status, Some(err.to_string()));
    }

    /// Return snapshots of all mirrors in the pool.
    pub fn snapshots(&self) -> Vec<MirrorSnapshot> {
        let now = Instant::now();
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .entries
            .iter()
            .map(|e| MirrorSnapshot {
                source: e.source.clone(),
                latency: e.latency,
                score: e.effective_score(&guard.config),
                content_length: e.content_length,
                etag: e.etag.clone(),
                supports_ranges: e.supports_ranges,
                failures: e.failures,
                is_penalized: e.penalized_until.is_some_and(|until| until > now),
                mismatched: e.mismatched,
                active_requests: e.active_requests,
                last_error: e.last_error.clone(),
            })
            .collect()
    }

    /// Return healthy, consistent mirrors ranked from best (lowest score) to worst.
    pub fn ranked_mirrors(&self) -> Vec<MirrorSnapshot> {
        let mut snaps: Vec<MirrorSnapshot> = self
            .snapshots()
            .into_iter()
            .filter(|s| !s.mismatched && !s.is_penalized)
            .collect();
        snaps.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        snaps
    }

    /// Build a [`ProbeResult`] from the highest-ranked probed mirror, if any.
    pub fn best_probe_result(&self) -> Option<ProbeResult> {
        let now = Instant::now();
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let idx = pick_best_index(&guard, &HashSet::new(), now, true)?;
        let entry = &guard.entries[idx];
        if let Some(probe) = &entry.last_probe {
            return Some(probe.clone());
        }
        Some(ProbeResult {
            protocol: if entry.source.url.starts_with("https://") {
                ProtocolKind::Https
            } else {
                ProtocolKind::Http
            },
            resolved_url: entry.source.url.clone(),
            total_size: guard.expected_size.or(entry.content_length),
            supports_ranges: entry.supports_ranges,
            suggested_filename: None,
            etag: guard.expected_etag.clone().or_else(|| entry.etag.clone()),
            last_modified: None,
            content_type: None,
        })
    }

    fn release_active(&self, url: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = guard.entries.iter_mut().find(|e| e.source.url == url) {
            entry.active_requests = entry.active_requests.saturating_sub(1);
        }
    }
}

fn pick_best_index(
    guard: &PoolInner,
    excluded_urls: &HashSet<String>,
    now: Instant,
    allow_penalized_fallback: bool,
) -> Option<usize> {
    let best_unpenalized = guard
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            !e.mismatched
                && !excluded_urls.contains(&e.source.url)
                && e.penalized_until.is_none_or(|until| until <= now)
        })
        .min_by(|(_, a), (_, b)| {
            a.effective_score(&guard.config)
                .partial_cmp(&b.effective_score(&guard.config))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| idx);

    if best_unpenalized.is_some() || !allow_penalized_fallback {
        return best_unpenalized;
    }

    // Fallback: pick non-mismatched mirror with earliest cooldown expiry / lowest score.
    guard
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.mismatched && !excluded_urls.contains(&e.source.url))
        .min_by(|(_, a), (_, b)| {
            a.effective_score(&guard.config)
                .partial_cmp(&b.effective_score(&guard.config))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| idx)
}

fn apply_failure(
    entry: &mut MirrorEntry,
    config: &MirrorConfig,
    status: Option<u16>,
    reason: Option<String>,
    now: Instant,
) {
    entry.failures = entry.failures.saturating_add(1);
    let exp = entry.failures.saturating_sub(1).min(16);
    let multiplier = 1u32.checked_shl(exp).unwrap_or(u32::MAX);
    let mut penalty = config
        .base_penalty
        .saturating_mul(multiplier)
        .min(config.max_penalty);

    // Rate-limit / service-unavailable responses (429/503) receive at least 2x base penalty.
    if matches!(status, Some(429 | 503)) {
        penalty = penalty
            .max(config.base_penalty.saturating_mul(2))
            .min(config.max_penalty);
    }

    entry.penalized_until = Some(now + penalty);
    entry.last_error = reason;
}

/// RAII guard representing an in-flight segment request against a mirror.
#[derive(Debug)]
pub struct MirrorLease {
    pool: MirrorPool,
    url: String,
}

impl MirrorLease {
    /// Mirror URL assigned by this lease.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for MirrorLease {
    fn drop(&mut self) {
        self.pool.release_active(&self.url);
    }
}

/// [`ProtocolClient`] adapter that routes segment requests across a [`MirrorPool`]
/// with automatic failover on failure or HTTP 429/503.
pub struct MirrorProtocolClient {
    inner: Arc<dyn ProtocolClient>,
    pool: MirrorPool,
}

impl MirrorProtocolClient {
    /// Wrap `inner` with dynamic mirror routing via `pool`.
    pub fn new(inner: Arc<dyn ProtocolClient>, pool: MirrorPool) -> Self {
        Self { inner, pool }
    }

    /// Access the underlying [`MirrorPool`].
    pub fn pool(&self) -> &MirrorPool {
        &self.pool
    }
}

#[async_trait]
impl ProtocolClient for MirrorProtocolClient {
    fn supports(&self, source: &str) -> bool {
        self.inner.supports(source)
    }

    async fn probe(&self, request: &DownloadRequest) -> Result<ProbeResult> {
        let summary = self.pool.probe_all(&*self.inner, request).await?;
        let mut best = self.pool.best_probe_result().ok_or_else(|| {
            MayaError::Network("no healthy mirrors available after probe".to_string())
        })?;
        if best.total_size.is_none() {
            best.total_size = summary.consensus_size;
        }
        best.supports_ranges = summary.supports_ranges;
        if best.etag.is_none() {
            best.etag = summary.consensus_etag;
        }
        Ok(best)
    }

    async fn fetch_range(
        &self,
        request: &DownloadRequest,
        start: u64,
        end: Option<u64>,
    ) -> Result<ByteStream> {
        let total_mirrors = self.pool.len().max(1);
        let mut tried: HashSet<String> = HashSet::with_capacity(total_mirrors);
        let mut last_err: Option<MayaError> = None;

        for attempt in 0..total_mirrors {
            let allow_penalized = attempt > 0 && tried.is_empty();
            let lease = self
                .pool
                .acquire_mirror_excluding(&tried, allow_penalized)
                .or_else(|| self.pool.acquire_mirror_excluding(&tried, true));

            let Some(lease) = lease else {
                break;
            };

            let mirror_url = lease.url().to_string();
            tried.insert(mirror_url.clone());

            let mut mirror_req = request.clone();
            mirror_req.source = mirror_url.clone();

            let started = Instant::now();
            match self.inner.fetch_range(&mirror_req, start, end).await {
                Ok(stream) => {
                    self.pool.record_success(&mirror_url, started.elapsed());
                    return Ok(Box::pin(TrackedMirrorStream {
                        inner: stream,
                        lease: Some(lease),
                        pool: self.pool.clone(),
                        url: mirror_url,
                        failed: false,
                    }));
                }
                Err(err) => {
                    self.pool.record_error(&mirror_url, &err);
                    last_err = Some(err);
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| MayaError::Network("all mirrors in pool failed".to_string())))
    }
}

struct TrackedMirrorStream {
    inner: ByteStream,
    lease: Option<MirrorLease>,
    pool: MirrorPool,
    url: String,
    failed: bool,
}

impl Stream for TrackedMirrorStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        match &poll {
            Poll::Ready(Some(Err(err))) => {
                if !self.failed {
                    self.failed = true;
                    self.pool.record_error(&self.url, err);
                }
            }
            Poll::Ready(None) => {
                self.lease.take();
            }
            _ => {}
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::stream;

    use crate::types::SecurityPolicy;

    fn test_probe(url: &str, size: Option<u64>, etag: Option<&str>) -> ProbeResult {
        ProbeResult {
            protocol: ProtocolKind::Https,
            resolved_url: url.to_string(),
            total_size: size,
            supports_ranges: true,
            suggested_filename: Some("release.tar.gz".to_string()),
            etag: etag.map(str::to_string),
            last_modified: None,
            content_type: Some("application/octet-stream".to_string()),
        }
    }

    #[test]
    fn metalink_parses_meta4_rfc5854_urls_priorities_and_hashes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="maya-linux-x64.tar.gz">
    <size>1048576</size>
    <hash type="md5">d41d8cd98f00b204e9800998ecf8427e</hash>
    <hash type="sha-256">E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855</hash>
    <hash type="sha-512">cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e</hash>
    <url priority="20" location="de">https://de.example.com/maya.tar.gz</url>
    <url priority="1" location="us">https://us.example.com/maya.tar.gz</url>
    <url priority="5">https://jp.example.com/maya.tar.gz</url>
  </file>
</metalink>"#;

        let doc = Metalink::parse_xml(xml).expect("parse meta4 xml");
        assert_eq!(doc.files.len(), 1);
        let file = &doc.files[0];
        assert_eq!(file.name, "maya-linux-x64.tar.gz");
        assert_eq!(file.size, Some(1_048_576));
        assert_eq!(file.checksums.len(), 3);
        assert_eq!(file.checksums[0].algorithm, ChecksumAlgorithm::Md5);
        assert_eq!(file.checksums[1].algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(
            file.checksums[1].expected_hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(file.checksums[2].algorithm, ChecksumAlgorithm::Sha512);

        assert_eq!(file.urls.len(), 3);
        assert_eq!(file.urls[0].url, "https://us.example.com/maya.tar.gz");
        assert_eq!(file.urls[0].priority, 1);
        assert_eq!(file.urls[0].location.as_deref(), Some("us"));
        assert_eq!(file.urls[1].url, "https://jp.example.com/maya.tar.gz");
        assert_eq!(file.urls[1].priority, 5);
        assert_eq!(file.urls[2].url, "https://de.example.com/maya.tar.gz");
        assert_eq!(file.urls[2].priority, 20);
    }

    #[test]
    fn metalink_parses_v3_verification_and_resources() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<metalink version="3.0" xmlns="http://www.metalinker.org/">
  <files>
    <file name="ubuntu.iso">
      <size>4294967296</size>
      <verification>
        <hash type="sha256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
        <hash type="blake3">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
      </verification>
      <resources>
        <url type="http" preference="50" location="uk">https://uk.example.com/ubuntu.iso</url>
        <url type="http" preference="100" location="us">https://us.example.com/ubuntu.iso</url>
      </resources>
    </file>
  </files>
</metalink>"#;

        let doc = Metalink::parse_xml(xml).expect("parse metalink v3 xml");
        let file = doc.primary_file().expect("primary file");
        assert_eq!(file.name, "ubuntu.iso");
        assert_eq!(file.size, Some(4_294_967_296));
        assert_eq!(file.checksums.len(), 2);
        assert_eq!(file.checksums[0].algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(file.checksums[1].algorithm, ChecksumAlgorithm::Blake3);
        // preference 100 -> priority 1 (first), preference 50 -> priority 51 (second).
        assert_eq!(file.urls[0].url, "https://us.example.com/ubuntu.iso");
        assert_eq!(file.urls[0].priority, 1);
        assert_eq!(file.urls[1].url, "https://uk.example.com/ubuntu.iso");
        assert_eq!(file.urls[1].priority, 51);
    }

    #[tokio::test]
    async fn mirror_pool_probe_checks_content_length_and_etag_consensus() {
        let pool = MirrorPool::from_urls([
            "https://m1.example.com/file.bin",
            "https://m2.example.com/file.bin",
            "https://bad-size.example.com/file.bin",
            "https://bad-etag.example.com/file.bin",
            "https://down.example.com/file.bin",
        ])
        .unwrap();

        let summary = pool
            .probe_with(|url| async move {
                match url.as_str() {
                    "https://m1.example.com/file.bin" => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Ok(test_probe(&url, Some(1024), Some("\"v1\"")))
                    }
                    "https://m2.example.com/file.bin" => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        Ok(test_probe(&url, Some(1024), Some("\"v1\"")))
                    }
                    "https://bad-size.example.com/file.bin" => {
                        Ok(test_probe(&url, Some(999), Some("\"v1\"")))
                    }
                    "https://bad-etag.example.com/file.bin" => {
                        Ok(test_probe(&url, Some(1024), Some("\"v2\"")))
                    }
                    _ => Err(MayaError::HttpStatus {
                        status: 503,
                        message: "Service Unavailable".to_string(),
                    }),
                }
            })
            .await
            .expect("probe summary");

        assert_eq!(summary.consensus_size, Some(1024));
        assert_eq!(summary.consensus_etag.as_deref(), Some("\"v1\""));
        assert_eq!(summary.responsive_mirrors, 2);
        assert_eq!(summary.mismatched_mirrors, 2);
        assert_eq!(summary.failed_mirrors, 1);

        // m2 was faster than m1 so m2 should be selected first.
        let best = pool.select_mirror().expect("best mirror");
        assert_eq!(best, "https://m2.example.com/file.bin");
    }

    #[test]
    fn mirror_scoring_and_exponential_penalty_on_429_503() {
        let pool = MirrorPool::from_urls([
            "https://fast.example.com/a.bin",
            "https://slow.example.com/a.bin",
        ])
        .unwrap()
        .with_config(MirrorConfig {
            base_penalty: Duration::from_secs(1),
            max_penalty: Duration::from_secs(16),
            concurrency_penalty_ms: 25.0,
        });

        pool.record_success("https://fast.example.com/a.bin", Duration::from_millis(10));
        pool.record_success("https://slow.example.com/a.bin", Duration::from_millis(40));

        assert_eq!(
            pool.select_mirror().as_deref(),
            Some("https://fast.example.com/a.bin")
        );

        // Holding an active lease on fast mirror increases its effective score.
        let lease = pool.acquire_mirror().expect("acquire fast");
        assert_eq!(lease.url(), "https://fast.example.com/a.bin");
        // 10ms + 25ms = 35ms < 40ms, so fast still leads with 1 active; 2 active -> 60ms > 40ms.
        let lease2 = pool.acquire_mirror().expect("acquire fast again");
        assert_eq!(lease2.url(), "https://fast.example.com/a.bin");
        assert_eq!(
            pool.select_mirror().as_deref(),
            Some("https://slow.example.com/a.bin")
        );
        drop(lease);
        drop(lease2);

        // Fail fast mirror with HTTP 429 -> penalized and routes to slow mirror.
        pool.record_failure(
            "https://fast.example.com/a.bin",
            Some(429),
            Some("Too Many Requests".to_string()),
        );
        assert_eq!(
            pool.select_mirror().as_deref(),
            Some("https://slow.example.com/a.bin")
        );

        let snaps = pool.snapshots();
        let fast_snap = snaps
            .iter()
            .find(|s| s.source.url == "https://fast.example.com/a.bin")
            .unwrap();
        assert!(fast_snap.is_penalized);
        assert_eq!(fast_snap.failures, 1);
        let score_1 = fast_snap.score;

        // Second 503 failure doubles the failure multiplier exponentially.
        pool.record_failure(
            "https://fast.example.com/a.bin",
            Some(503),
            Some("Service Unavailable".to_string()),
        );
        let snaps2 = pool.snapshots();
        let fast_snap2 = snaps2
            .iter()
            .find(|s| s.source.url == "https://fast.example.com/a.bin")
            .unwrap();
        assert_eq!(fast_snap2.failures, 2);
        assert!(fast_snap2.score > score_1 * 1.9);
    }

    struct MockFailoverClient {
        calls_m1: AtomicUsize,
        calls_m2: AtomicUsize,
    }

    #[async_trait]
    impl ProtocolClient for MockFailoverClient {
        fn supports(&self, source: &str) -> bool {
            source.starts_with("https://")
        }

        async fn probe(&self, request: &DownloadRequest) -> Result<ProbeResult> {
            Ok(test_probe(&request.source, Some(10), Some("\"abc\"")))
        }

        async fn fetch_range(
            &self,
            request: &DownloadRequest,
            _start: u64,
            _end: Option<u64>,
        ) -> Result<ByteStream> {
            if request.source.contains("m1") {
                self.calls_m1.fetch_add(1, Ordering::SeqCst);
                Err(MayaError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".to_string(),
                })
            } else {
                self.calls_m2.fetch_add(1, Ordering::SeqCst);
                Ok(Box::pin(stream::iter(vec![Ok(Bytes::from_static(
                    b"0123456789",
                ))])))
            }
        }
    }

    #[tokio::test]
    async fn mirror_protocol_client_fails_over_on_503() {
        let pool = MirrorPool::new(vec![
            MirrorSource::new("https://m1.example.com/file.bin").with_priority(1),
            MirrorSource::new("https://m2.example.com/file.bin").with_priority(2),
        ])
        .unwrap();

        pool.record_success("https://m1.example.com/file.bin", Duration::from_millis(5));
        pool.record_success("https://m2.example.com/file.bin", Duration::from_millis(20));

        let inner = Arc::new(MockFailoverClient {
            calls_m1: AtomicUsize::new(0),
            calls_m2: AtomicUsize::new(0),
        });
        let client = MirrorProtocolClient::new(inner.clone(), pool.clone());

        let mut req = DownloadRequest::new("https://m1.example.com/file.bin", PathBuf::from("."));
        req.security = SecurityPolicy::default();

        let mut byte_stream = client.fetch_range(&req, 0, Some(10)).await.unwrap();
        use futures::StreamExt;
        let chunk = byte_stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.as_ref(), b"0123456789");
        assert_eq!(inner.calls_m1.load(Ordering::SeqCst), 1);
        assert_eq!(inner.calls_m2.load(Ordering::SeqCst), 1);
        // m1 should now be penalized and m2 should rank #1.
        assert_eq!(
            pool.select_mirror().as_deref(),
            Some("https://m2.example.com/file.bin")
        );
    }
}
