use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use maya_core::{MayaError, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use url::Url;

const DEFAULT_YTDLP_PATHS: &[&str] = &[
    r"C:\Python315\Scripts\yt-dlp.exe",
    r"C:\Users\uixat\AppData\Local\Microsoft\WinGet\Links\yt-dlp.exe",
];

const FALLBACK_PYTHON_PATH: &str = r"C:\Python315\python.exe";

const RECOGNIZED_MEDIA_DOMAINS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "vimeo.com",
    "dailymotion.com",
    "dai.ly",
    "soundcloud.com",
    "tiktok.com",
    "twitch.tv",
];

/// Resolved command invocation for `yt-dlp` (either a standalone binary or `python -m yt_dlp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDlpCommand {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

impl YtDlpCommand {
    /// Construct a command backed by a standalone `yt-dlp` binary.
    pub fn executable(path: impl Into<PathBuf>) -> Self {
        Self {
            program: path.into(),
            prefix_args: Vec::new(),
        }
    }

    /// Construct a command backed by `python -m yt_dlp`.
    pub fn python_module(python_path: impl Into<PathBuf>) -> Self {
        Self {
            program: python_path.into(),
            prefix_args: vec!["-m".to_string(), "yt_dlp".to_string()],
        }
    }
}

/// Metadata and resolved stream URL(s) for a single media format entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFormatStream {
    pub url: String,
    pub format_id: Option<String>,
    pub ext: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub bitrate_kbps: Option<f64>,
}

/// Selected stream layout returned by [`MediaExtractor::extract_info`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectedStreams {
    /// Pre-merged single stream containing audio+video (or audio-only).
    Muxed(MediaFormatStream),
    /// Separate best video and optional best audio streams.
    Separate {
        video: MediaFormatStream,
        audio: Option<MediaFormatStream>,
    },
}

/// Structured media metadata extracted via `yt-dlp --dump-single-json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaStreamInfo {
    pub title: String,
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    pub format_id: Option<String>,
    pub ext: String,
    pub direct_url: Option<String>,
    pub video_url: Option<String>,
    pub audio_url: Option<String>,
    pub streams: SelectedStreams,
    pub http_headers: BTreeMap<String, String>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub cookies: Option<String>,
}

/// Extractor that discovers `yt-dlp` and resolves streaming platform URLs into direct streams.
#[derive(Debug, Clone, Default)]
pub struct MediaExtractor {
    command: Option<YtDlpCommand>,
}

impl MediaExtractor {
    /// Construct a [`MediaExtractor`] that discovers `yt-dlp` automatically on demand.
    pub fn new() -> Self {
        Self { command: None }
    }

    /// Construct a [`MediaExtractor`] pinned to an explicit [`YtDlpCommand`].
    pub fn with_command(command: YtDlpCommand) -> Self {
        Self {
            command: Some(command),
        }
    }

    /// Discover the `yt-dlp` executable using the standard search order:
    /// 1. `C:\Python315\Scripts\yt-dlp.exe`
    /// 2. `C:\Users\uixat\AppData\Local\Microsoft\WinGet\Links\yt-dlp.exe`
    /// 3. `yt-dlp` / `yt-dlp.exe` on `PATH`
    /// 4. Fallback `C:\Python315\python.exe -m yt_dlp`
    pub fn discover_yt_dlp() -> Result<YtDlpCommand> {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join(r"Microsoft\WinGet\Links\yt-dlp.exe"));
        let path_env = std::env::var_os("PATH");
        discover_yt_dlp_with(
            DEFAULT_YTDLP_PATHS.iter().map(Path::new),
            local_app_data.as_deref(),
            path_env.as_deref(),
            Path::new(FALLBACK_PYTHON_PATH),
        )
    }

    /// Return `true` if `url` points to a recognized video/streaming platform or `.m3u8`/`.mpd` manifest.
    pub fn is_media_url(url: &str) -> bool {
        let Ok(parsed) = Url::parse(url.trim()) else {
            return false;
        };
        if !parsed.scheme().eq_ignore_ascii_case("http")
            && !parsed.scheme().eq_ignore_ascii_case("https")
        {
            return false;
        }

        let path_lower = parsed.path().to_ascii_lowercase();
        if path_lower.ends_with(".m3u8") || path_lower.ends_with(".mpd") {
            return true;
        }

        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host_lower = host.trim_end_matches('.').to_ascii_lowercase();

        RECOGNIZED_MEDIA_DOMAINS
            .iter()
            .any(|domain| host_lower == *domain || host_lower.ends_with(&format!(".{domain}")))
    }

    /// Extract media metadata and best direct stream URL(s) for `url` using `yt-dlp`.
    pub async fn extract_info(&self, url: &str) -> Result<MediaStreamInfo> {
        let trimmed = url.trim();
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(MayaError::InvalidConfig(
                "URL contains control characters".to_string(),
            ));
        }

        let parsed = Url::parse(trimmed)?;
        if !parsed.scheme().eq_ignore_ascii_case("http")
            && !parsed.scheme().eq_ignore_ascii_case("https")
        {
            return Err(MayaError::UnsupportedScheme(parsed.scheme().to_string()));
        }

        let cmd = match &self.command {
            Some(c) => c.clone(),
            None => Self::discover_yt_dlp()?,
        };

        let mut command = Command::new(&cmd.program);
        command.args(&cmd.prefix_args);
        command.args([
            "--dump-single-json",
            "--no-warnings",
            "--no-call-home",
            "--prefer-free-formats",
            "--",
            parsed.as_str(),
        ]);

        let output = command.output().await.map_err(|err| {
            MayaError::Protocol(format!(
                "failed to execute `{}`: {err}",
                cmd.program.display()
            ))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            return Err(MayaError::Protocol(if detail.is_empty() {
                format!("yt-dlp exited with status {}", output.status)
            } else {
                format!("yt-dlp failed ({}): {detail}", output.status)
            }));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|err| {
            MayaError::Protocol(format!("yt-dlp stdout was not valid UTF-8: {err}"))
        })?;

        parse_ytdlp_json(&stdout)
    }
}

fn discover_yt_dlp_with<'a>(
    explicit_candidates: impl Iterator<Item = &'a Path>,
    winget_local_appdata: Option<&Path>,
    path_env: Option<&std::ffi::OsStr>,
    fallback_python: &Path,
) -> Result<YtDlpCommand> {
    for candidate in explicit_candidates {
        if candidate.is_file() {
            return Ok(YtDlpCommand::executable(candidate));
        }
    }

    if let Some(winget_path) = winget_local_appdata {
        if winget_path.is_file() {
            return Ok(YtDlpCommand::executable(winget_path));
        }
    }

    if let Some(paths) = path_env {
        let exe_names: &[&str] = if cfg!(windows) {
            &["yt-dlp.exe", "yt-dlp"]
        } else {
            &["yt-dlp"]
        };
        for dir in std::env::split_paths(paths) {
            for name in exe_names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(YtDlpCommand::executable(candidate));
                }
            }
        }
    }

    if fallback_python.is_file() {
        return Ok(YtDlpCommand::python_module(fallback_python));
    }

    Err(MayaError::Unsupported(
        "yt-dlp executable was not found in known paths, PATH, or Python 3.15".to_string(),
    ))
}

#[derive(Debug, Deserialize)]
struct RawYtDlpInfo {
    title: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    url: Option<String>,
    format_id: Option<String>,
    ext: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    cookies: Option<String>,
    #[serde(default)]
    requested_formats: Vec<RawYtDlpFormat>,
    #[serde(default)]
    formats: Vec<RawYtDlpFormat>,
    #[serde(default)]
    entries: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawYtDlpFormat {
    url: Option<String>,
    format_id: Option<String>,
    ext: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    tbr: Option<f64>,
    vbr: Option<f64>,
    abr: Option<f64>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    cookies: Option<String>,
}

impl RawYtDlpFormat {
    fn has_video(&self) -> bool {
        !matches!(self.vcodec.as_deref(), Some("none"))
    }

    fn has_audio(&self) -> bool {
        !matches!(self.acodec.as_deref(), Some("none"))
    }

    fn is_muxed(&self) -> bool {
        self.vcodec
            .as_deref()
            .is_some_and(|v| !v.eq_ignore_ascii_case("none"))
            && self
                .acodec
                .as_deref()
                .is_some_and(|a| !a.eq_ignore_ascii_case("none"))
    }

    fn is_video_only(&self) -> bool {
        self.vcodec
            .as_deref()
            .is_some_and(|v| !v.eq_ignore_ascii_case("none"))
            && matches!(self.acodec.as_deref(), Some("none"))
    }

    fn is_audio_only(&self) -> bool {
        matches!(self.vcodec.as_deref(), Some("none"))
            && self
                .acodec
                .as_deref()
                .is_some_and(|a| !a.eq_ignore_ascii_case("none"))
    }

    fn video_score(&self) -> (u64, u64) {
        let h = self.height.unwrap_or(0);
        let br = self.vbr.or(self.tbr).unwrap_or(0.0).max(0.0);
        (h, (br * 1000.0) as u64)
    }

    fn audio_score(&self) -> u64 {
        let br = self.abr.or(self.tbr).unwrap_or(0.0).max(0.0);
        (br * 1000.0) as u64
    }

    fn into_stream(self, fallback_headers: &BTreeMap<String, String>) -> Option<MediaFormatStream> {
        let url = self.url.filter(|u| !u.trim().is_empty())?;
        let mut headers = fallback_headers.clone();
        for (k, v) in self.http_headers {
            headers.insert(k, v);
        }
        if let Some(cookies) = self.cookies.filter(|c| !c.trim().is_empty()) {
            insert_header_if_absent(&mut headers, "Cookie", cookies);
        }
        Some(MediaFormatStream {
            url,
            format_id: self.format_id,
            ext: self.ext,
            http_headers: headers,
            width: self.width,
            height: self.height,
            bitrate_kbps: self.tbr.or(self.vbr).or(self.abr),
        })
    }
}

/// Parse the JSON stdout produced by `yt-dlp --dump-single-json` into [`MediaStreamInfo`].
pub fn parse_ytdlp_json(raw_json: &str) -> Result<MediaStreamInfo> {
    let mut raw: RawYtDlpInfo = serde_json::from_str(raw_json)?;

    // If yt-dlp returned a playlist wrapper for a single item, unwrap the first valid entry.
    if raw.url.is_none()
        && raw.requested_formats.is_empty()
        && raw.formats.is_empty()
        && !raw.entries.is_empty()
    {
        for entry in std::mem::take(&mut raw.entries) {
            if entry.is_null() {
                continue;
            }
            if let Ok(inner) = serde_json::from_value::<RawYtDlpInfo>(entry) {
                raw = inner;
                break;
            }
        }
    }

    let mut base_headers = raw.http_headers.clone();
    if let Some(cookies) = raw.cookies.as_ref().filter(|c| !c.trim().is_empty()) {
        insert_header_if_absent(&mut base_headers, "Cookie", cookies.clone());
    }

    let title = raw
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("untitled")
        .to_string();

    let (streams, chosen_format_id, chosen_ext, merged_headers) =
        select_streams_from_raw(&raw, &base_headers)?;

    let (direct_url, video_url, audio_url) = match &streams {
        SelectedStreams::Muxed(s) => (Some(s.url.clone()), None, None),
        SelectedStreams::Separate { video, audio } => (
            None,
            Some(video.url.clone()),
            audio.as_ref().map(|a| a.url.clone()),
        ),
    };

    let format_id = raw.format_id.or(chosen_format_id);
    let ext = raw
        .ext
        .filter(|e| !e.trim().is_empty())
        .or(chosen_ext)
        .unwrap_or_else(|| "mp4".to_string());

    let user_agent = find_header_ci(&merged_headers, "user-agent");
    let referer = find_header_ci(&merged_headers, "referer");
    let cookies = find_header_ci(&merged_headers, "cookie").or(raw.cookies);

    Ok(MediaStreamInfo {
        title,
        duration: raw.duration,
        thumbnail: raw.thumbnail,
        format_id,
        ext,
        direct_url,
        video_url,
        audio_url,
        streams,
        http_headers: merged_headers,
        user_agent,
        referer,
        cookies,
    })
}

type SelectedStreamOutcome = (
    SelectedStreams,
    Option<String>,
    Option<String>,
    BTreeMap<String, String>,
);

fn select_streams_from_raw(
    raw: &RawYtDlpInfo,
    base_headers: &BTreeMap<String, String>,
) -> Result<SelectedStreamOutcome> {
    if !raw.requested_formats.is_empty() {
        let mut video_fmt: Option<RawYtDlpFormat> = None;
        let mut audio_fmt: Option<RawYtDlpFormat> = None;

        for fmt in &raw.requested_formats {
            if fmt.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                continue;
            }
            if fmt.has_video() && video_fmt.is_none() {
                video_fmt = Some(fmt.clone());
            } else if fmt.has_audio() && audio_fmt.is_none() {
                audio_fmt = Some(fmt.clone());
            }
        }

        if let (Some(vf), Some(af)) = (video_fmt.clone(), audio_fmt.clone()) {
            let v_stream = vf
                .into_stream(base_headers)
                .ok_or_else(|| MayaError::Protocol("missing video stream URL".to_string()))?;
            let a_stream = af
                .into_stream(base_headers)
                .ok_or_else(|| MayaError::Protocol("missing audio stream URL".to_string()))?;
            let fmt_id = match (&v_stream.format_id, &a_stream.format_id) {
                (Some(v), Some(a)) => Some(format!("{v}+{a}")),
                (Some(v), None) => Some(v.clone()),
                (None, Some(a)) => Some(a.clone()),
                (None, None) => None,
            };
            let ext = v_stream.ext.clone();
            let mut headers = base_headers.clone();
            headers.extend(v_stream.http_headers.clone());
            return Ok((
                SelectedStreams::Separate {
                    video: v_stream,
                    audio: Some(a_stream),
                },
                fmt_id,
                ext,
                headers,
            ));
        }

        if let Some(single) = raw
            .requested_formats
            .iter()
            .find_map(|f| f.clone().into_stream(base_headers))
        {
            let fmt_id = single.format_id.clone();
            let ext = single.ext.clone();
            let headers = single.http_headers.clone();
            return Ok((SelectedStreams::Muxed(single), fmt_id, ext, headers));
        }
    }

    if let Some(url) = raw.url.as_ref().filter(|u| !u.trim().is_empty()) {
        let stream = MediaFormatStream {
            url: url.clone(),
            format_id: raw.format_id.clone(),
            ext: raw.ext.clone(),
            http_headers: base_headers.clone(),
            width: None,
            height: None,
            bitrate_kbps: None,
        };
        return Ok((
            SelectedStreams::Muxed(stream),
            raw.format_id.clone(),
            raw.ext.clone(),
            base_headers.clone(),
        ));
    }

    let valid_formats: Vec<RawYtDlpFormat> = raw
        .formats
        .iter()
        .filter(|f| f.url.as_deref().is_some_and(|u| !u.trim().is_empty()))
        .cloned()
        .collect();

    if valid_formats.is_empty() {
        return Err(MayaError::Protocol(
            "yt-dlp JSON contained no playable stream URLs".to_string(),
        ));
    }

    let best_muxed = valid_formats
        .iter()
        .filter(|f| f.is_muxed())
        .max_by_key(|f| f.video_score())
        .cloned();

    let best_video_only = valid_formats
        .iter()
        .filter(|f| f.is_video_only())
        .max_by_key(|f| f.video_score())
        .cloned();

    let best_audio_only = valid_formats
        .iter()
        .filter(|f| f.is_audio_only())
        .max_by_key(|f| f.audio_score())
        .cloned();

    // Prefer separate streams if a higher-resolution video-only + audio-only pair exists,
    // or if no pre-merged format exists.
    match (best_muxed, best_video_only, best_audio_only) {
        (Some(muxed), Some(vid), Some(aud)) if vid.video_score() > muxed.video_score() => {
            let v_stream = vid.into_stream(base_headers).unwrap();
            let a_stream = aud.into_stream(base_headers).unwrap();
            let fmt_id = match (&v_stream.format_id, &a_stream.format_id) {
                (Some(v), Some(a)) => Some(format!("{v}+{a}")),
                _ => v_stream.format_id.clone(),
            };
            let ext = v_stream.ext.clone();
            let headers = v_stream.http_headers.clone();
            Ok((
                SelectedStreams::Separate {
                    video: v_stream,
                    audio: Some(a_stream),
                },
                fmt_id,
                ext,
                headers,
            ))
        }
        (Some(muxed), _, _) => {
            let stream = muxed.into_stream(base_headers).unwrap();
            let fmt_id = stream.format_id.clone();
            let ext = stream.ext.clone();
            let headers = stream.http_headers.clone();
            Ok((SelectedStreams::Muxed(stream), fmt_id, ext, headers))
        }
        (None, Some(vid), aud_opt) => {
            let v_stream = vid.into_stream(base_headers).unwrap();
            let a_stream = aud_opt.and_then(|a| a.into_stream(base_headers));
            let fmt_id = match (
                &v_stream.format_id,
                a_stream.as_ref().and_then(|a| a.format_id.as_ref()),
            ) {
                (Some(v), Some(a)) => Some(format!("{v}+{a}")),
                (Some(v), None) => Some(v.clone()),
                _ => None,
            };
            let ext = v_stream.ext.clone();
            let headers = v_stream.http_headers.clone();
            Ok((
                SelectedStreams::Separate {
                    video: v_stream,
                    audio: a_stream,
                },
                fmt_id,
                ext,
                headers,
            ))
        }
        (None, None, Some(aud)) => {
            let stream = aud.into_stream(base_headers).unwrap();
            let fmt_id = stream.format_id.clone();
            let ext = stream.ext.clone();
            let headers = stream.http_headers.clone();
            Ok((SelectedStreams::Muxed(stream), fmt_id, ext, headers))
        }
        (None, None, None) => {
            // Formats without explicit vcodec/acodec tags (e.g. generic direct streams)
            let fallback = valid_formats
                .into_iter()
                .max_by_key(|f| f.video_score())
                .and_then(|f| f.into_stream(base_headers))
                .ok_or_else(|| {
                    MayaError::Protocol("yt-dlp JSON contained no valid stream URL".to_string())
                })?;
            let fmt_id = fallback.format_id.clone();
            let ext = fallback.ext.clone();
            let headers = fallback.http_headers.clone();
            Ok((SelectedStreams::Muxed(fallback), fmt_id, ext, headers))
        }
    }
}

fn insert_header_if_absent(headers: &mut BTreeMap<String, String>, key: &str, value: String) {
    let exists = headers.keys().any(|k| k.eq_ignore_ascii_case(key));
    if !exists {
        headers.insert(key.to_string(), value);
    }
}

fn find_header_ci(headers: &BTreeMap<String, String>, key: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_media_url_recognizes_platforms_and_manifests() {
        assert!(MediaExtractor::is_media_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(MediaExtractor::is_media_url("https://youtu.be/dQw4w9WgXcQ"));
        assert!(MediaExtractor::is_media_url("https://vimeo.com/123456"));
        assert!(MediaExtractor::is_media_url(
            "https://www.dailymotion.com/video/x7xyz"
        ));
        assert!(MediaExtractor::is_media_url(
            "https://soundcloud.com/artist/track"
        ));
        assert!(MediaExtractor::is_media_url(
            "https://www.tiktok.com/@user/video/123"
        ));
        assert!(MediaExtractor::is_media_url(
            "https://www.twitch.tv/videos/12345"
        ));
        assert!(MediaExtractor::is_media_url(
            "https://cdn.example.com/live/stream.m3u8?token=abc"
        ));
        assert!(MediaExtractor::is_media_url(
            "https://cdn.example.com/vod/manifest.MPD"
        ));

        // Non-media or lookalike domains must return false
        assert!(!MediaExtractor::is_media_url(
            "https://notyoutube.com/watch?v=1"
        ));
        assert!(!MediaExtractor::is_media_url(
            "https://example.com/archive.tar.gz"
        ));
        assert!(!MediaExtractor::is_media_url("ftp://youtube.com/watch?v=1"));
        assert!(!MediaExtractor::is_media_url("not-a-url"));
    }

    #[test]
    fn discover_yt_dlp_prefers_explicit_then_path_then_python_fallback() {
        let base = std::env::temp_dir().join(format!("maya_ytdlp_disc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir_explicit = base.join("explicit");
        let dir_path = base.join("path");
        let dir_py = base.join("py");
        std::fs::create_dir_all(&dir_explicit).unwrap();
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::create_dir_all(&dir_py).unwrap();

        let explicit_exe = dir_explicit.join("yt-dlp.exe");
        let path_exe = dir_path.join(if cfg!(windows) {
            "yt-dlp.exe"
        } else {
            "yt-dlp"
        });
        let py_exe = dir_py.join("python.exe");

        // 1. Only python fallback exists
        std::fs::write(&py_exe, b"stub").unwrap();
        let cmd1 = discover_yt_dlp_with(
            [explicit_exe.as_path()].into_iter(),
            None,
            Some(dir_path.as_os_str()),
            &py_exe,
        )
        .unwrap();
        assert_eq!(cmd1, YtDlpCommand::python_module(&py_exe));

        // 2. PATH entry beats python fallback
        std::fs::write(&path_exe, b"stub").unwrap();
        let cmd2 = discover_yt_dlp_with(
            [explicit_exe.as_path()].into_iter(),
            None,
            Some(dir_path.as_os_str()),
            &py_exe,
        )
        .unwrap();
        assert_eq!(cmd2, YtDlpCommand::executable(&path_exe));

        // 3. Explicit path beats PATH entry
        std::fs::write(&explicit_exe, b"stub").unwrap();
        let cmd3 = discover_yt_dlp_with(
            [explicit_exe.as_path()].into_iter(),
            None,
            Some(dir_path.as_os_str()),
            &py_exe,
        )
        .unwrap();
        assert_eq!(cmd3, YtDlpCommand::executable(&explicit_exe));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parses_muxed_ytdlp_json() {
        let json = r#"{
            "title": "Sample Video",
            "duration": 125.5,
            "thumbnail": "https://img.example.com/thumb.jpg",
            "format_id": "22",
            "ext": "mp4",
            "url": "https://cdn.example.com/video.mp4",
            "http_headers": {
                "User-Agent": "Mozilla/5.0",
                "Referer": "https://www.youtube.com/"
            },
            "cookies": "SID=abc; HSID=xyz"
        }"#;

        let info = parse_ytdlp_json(json).unwrap();
        assert_eq!(info.title, "Sample Video");
        assert_eq!(info.duration, Some(125.5));
        assert_eq!(
            info.thumbnail.as_deref(),
            Some("https://img.example.com/thumb.jpg")
        );
        assert_eq!(info.format_id.as_deref(), Some("22"));
        assert_eq!(info.ext, "mp4");
        assert_eq!(
            info.direct_url.as_deref(),
            Some("https://cdn.example.com/video.mp4")
        );
        assert!(info.video_url.is_none());
        assert!(info.audio_url.is_none());
        assert!(matches!(info.streams, SelectedStreams::Muxed(_)));
        assert_eq!(info.user_agent.as_deref(), Some("Mozilla/5.0"));
        assert_eq!(info.referer.as_deref(), Some("https://www.youtube.com/"));
        assert_eq!(info.cookies.as_deref(), Some("SID=abc; HSID=xyz"));
    }

    #[test]
    fn parses_separate_requested_formats_ytdlp_json() {
        let json = r#"{
            "title": "4K Clip",
            "duration": 60.0,
            "thumbnail": "https://img.example.com/4k.webp",
            "format_id": "248+251",
            "ext": "webm",
            "requested_formats": [
                {
                    "format_id": "248",
                    "ext": "webm",
                    "vcodec": "vp9",
                    "acodec": "none",
                    "width": 1920,
                    "height": 1080,
                    "tbr": 2500.0,
                    "url": "https://cdn.example.com/video_1080p.webm",
                    "http_headers": {
                        "User-Agent": "CustomUA/1.0"
                    }
                },
                {
                    "format_id": "251",
                    "ext": "webm",
                    "vcodec": "none",
                    "acodec": "opus",
                    "abr": 160.0,
                    "url": "https://cdn.example.com/audio_opus.webm"
                }
            ]
        }"#;

        let info = parse_ytdlp_json(json).unwrap();
        assert_eq!(info.title, "4K Clip");
        assert_eq!(info.ext, "webm");
        assert_eq!(info.format_id.as_deref(), Some("248+251"));
        assert!(info.direct_url.is_none());
        assert_eq!(
            info.video_url.as_deref(),
            Some("https://cdn.example.com/video_1080p.webm")
        );
        assert_eq!(
            info.audio_url.as_deref(),
            Some("https://cdn.example.com/audio_opus.webm")
        );
        assert_eq!(info.user_agent.as_deref(), Some("CustomUA/1.0"));
        match info.streams {
            SelectedStreams::Separate { video, audio } => {
                assert_eq!(video.height, Some(1080));
                assert!(audio.is_some());
            }
            SelectedStreams::Muxed(_) => panic!("expected Separate streams"),
        }
    }

    #[test]
    fn selects_best_formats_when_only_formats_array_present() {
        let json = r#"{
            "title": "Fallback Selection",
            "formats": [
                {
                    "format_id": "18",
                    "ext": "mp4",
                    "vcodec": "avc1",
                    "acodec": "mp4a",
                    "height": 360,
                    "tbr": 500.0,
                    "url": "https://cdn.example.com/360p.mp4"
                },
                {
                    "format_id": "137",
                    "ext": "mp4",
                    "vcodec": "avc1",
                    "acodec": "none",
                    "height": 1080,
                    "vbr": 4000.0,
                    "url": "https://cdn.example.com/1080p_video.mp4"
                },
                {
                    "format_id": "140",
                    "ext": "m4a",
                    "vcodec": "none",
                    "acodec": "mp4a",
                    "abr": 128.0,
                    "url": "https://cdn.example.com/audio.m4a"
                }
            ]
        }"#;

        let info = parse_ytdlp_json(json).unwrap();
        assert_eq!(info.format_id.as_deref(), Some("137+140"));
        assert_eq!(
            info.video_url.as_deref(),
            Some("https://cdn.example.com/1080p_video.mp4")
        );
        assert_eq!(
            info.audio_url.as_deref(),
            Some("https://cdn.example.com/audio.m4a")
        );
    }
}
