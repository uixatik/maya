use maya_core::{MayaError, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

/// Content classification for a DASH [`DashAdaptationSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashContentType {
    Video,
    Audio,
    Subtitles,
    Other(String),
}

/// Entry `<S t="..." d="..." r="..."/>` inside a DASH `<SegmentTimeline>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashTimelineEntry {
    pub t: Option<u64>,
    pub d: u64,
    pub r: i64,
}

/// Parsed `<SegmentTemplate>` element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashSegmentTemplate {
    pub media: Option<String>,
    pub initialization: Option<String>,
    pub timescale: u64,
    pub duration: Option<u64>,
    pub start_number: u64,
    pub presentation_time_offset: u64,
    pub timeline: Vec<DashTimelineEntry>,
}

impl Default for DashSegmentTemplate {
    fn default() -> Self {
        Self {
            media: None,
            initialization: None,
            timescale: 1,
            duration: None,
            start_number: 1,
            presentation_time_offset: 0,
            timeline: Vec::new(),
        }
    }
}

/// Parsed `<Initialization>` element inside `<SegmentList>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashSegmentResource {
    pub source_url: Option<String>,
    pub range: Option<String>,
}

/// Parsed `<SegmentURL>` element inside `<SegmentList>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashSegmentUrl {
    pub media: Option<String>,
    pub media_range: Option<String>,
    pub index_range: Option<String>,
}

/// Parsed `<SegmentList>` element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashSegmentList {
    pub timescale: u64,
    pub duration: Option<u64>,
    pub start_number: u64,
    pub initialization: Option<DashSegmentResource>,
    pub segment_urls: Vec<DashSegmentUrl>,
}

impl Default for DashSegmentList {
    fn default() -> Self {
        Self {
            timescale: 1,
            duration: None,
            start_number: 1,
            initialization: None,
            segment_urls: Vec::new(),
        }
    }
}

/// Parsed `<Representation>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct DashRepresentation {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub mime_type: Option<String>,
    pub frame_rate: Option<String>,
    pub audio_sampling_rate: Option<u32>,
    pub base_urls: Vec<String>,
    pub segment_template: Option<DashSegmentTemplate>,
    pub segment_list: Option<DashSegmentList>,
}

/// Parsed `<AdaptationSet>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct DashAdaptationSet {
    pub id: Option<String>,
    pub content_type: Option<DashContentType>,
    pub mime_type: Option<String>,
    pub codecs: Option<String>,
    pub lang: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<String>,
    pub base_urls: Vec<String>,
    pub segment_template: Option<DashSegmentTemplate>,
    pub segment_list: Option<DashSegmentList>,
    pub representations: Vec<DashRepresentation>,
}

impl DashAdaptationSet {
    /// Effective content type considering `contentType`, `mimeType`, and child representations.
    pub fn effective_content_type(&self) -> Option<DashContentType> {
        if let Some(ct) = &self.content_type {
            return Some(ct.clone());
        }
        if let Some(mime) = &self.mime_type {
            return Some(classify_mime(mime));
        }
        self.representations
            .iter()
            .find_map(|r| r.mime_type.as_deref().map(classify_mime))
    }

    /// Select the highest-bandwidth representation in this adaptation set.
    pub fn best_representation(&self) -> Option<&DashRepresentation> {
        self.representations.iter().max_by_key(|r| {
            let pixels = r
                .width
                .zip(r.height)
                .map(|(w, h)| u64::from(w) * u64::from(h))
                .unwrap_or(0);
            (r.bandwidth, pixels)
        })
    }
}

/// Parsed `<Period>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct DashPeriod {
    pub id: Option<String>,
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub base_urls: Vec<String>,
    pub adaptation_sets: Vec<DashAdaptationSet>,
}

/// Resolved concrete segment URL + optional byte range string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashSegmentLocation {
    pub url: String,
    pub byte_range: Option<String>,
}

/// Resolved initialization and media segment sequence for a representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashResolvedSegments {
    pub initialization: Option<DashSegmentLocation>,
    pub segments: Vec<DashSegmentLocation>,
}

/// Parsed MPEG-DASH `.mpd` manifest.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DashManifest {
    pub manifest_type: Option<String>,
    pub media_presentation_duration: Option<f64>,
    pub min_buffer_time: Option<f64>,
    pub profiles: Option<String>,
    pub base_urls: Vec<String>,
    pub periods: Vec<DashPeriod>,
}

impl DashManifest {
    /// Parse an MPEG-DASH `.mpd` XML document.
    pub fn parse(xml: &str) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut manifest: Option<DashManifest> = None;
        let mut current_period: Option<DashPeriod> = None;
        let mut current_adaptation: Option<DashAdaptationSet> = None;
        let mut current_rep: Option<DashRepresentation> = None;
        let mut current_template: Option<DashSegmentTemplate> = None;
        let mut current_list: Option<DashSegmentList> = None;
        let mut in_base_url = false;
        let mut in_timeline = false;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    match local.as_str() {
                        "MPD" => {
                            let mut m = DashManifest::default();
                            for attr in e.attributes() {
                                let attr = attr.map_err(xml_err)?;
                                let k = String::from_utf8_lossy(attr.key.local_name().as_ref())
                                    .into_owned();
                                let v = attr.unescape_value().map_err(xml_err)?.into_owned();
                                match k.as_str() {
                                    "type" => m.manifest_type = Some(v),
                                    "mediaPresentationDuration" => {
                                        m.media_presentation_duration = parse_iso8601_duration(&v);
                                    }
                                    "minBufferTime" => {
                                        m.min_buffer_time = parse_iso8601_duration(&v);
                                    }
                                    "profiles" => m.profiles = Some(v),
                                    _ => {}
                                }
                            }
                            manifest = Some(m);
                        }
                        "Period" => {
                            let mut p = DashPeriod {
                                id: None,
                                start: None,
                                duration: None,
                                base_urls: Vec::new(),
                                adaptation_sets: Vec::new(),
                            };
                            for attr in e.attributes() {
                                let attr = attr.map_err(xml_err)?;
                                let k = String::from_utf8_lossy(attr.key.local_name().as_ref())
                                    .into_owned();
                                let v = attr.unescape_value().map_err(xml_err)?.into_owned();
                                match k.as_str() {
                                    "id" => p.id = Some(v),
                                    "start" => p.start = parse_iso8601_duration(&v),
                                    "duration" => p.duration = parse_iso8601_duration(&v),
                                    _ => {}
                                }
                            }
                            current_period = Some(p);
                        }
                        "AdaptationSet" => {
                            let mut a = DashAdaptationSet {
                                id: None,
                                content_type: None,
                                mime_type: None,
                                codecs: None,
                                lang: None,
                                width: None,
                                height: None,
                                frame_rate: None,
                                base_urls: Vec::new(),
                                segment_template: None,
                                segment_list: None,
                                representations: Vec::new(),
                            };
                            for attr in e.attributes() {
                                let attr = attr.map_err(xml_err)?;
                                let k = String::from_utf8_lossy(attr.key.local_name().as_ref())
                                    .into_owned();
                                let v = attr.unescape_value().map_err(xml_err)?.into_owned();
                                match k.as_str() {
                                    "id" => a.id = Some(v),
                                    "contentType" => {
                                        a.content_type = Some(classify_content_type(&v))
                                    }
                                    "mimeType" => a.mime_type = Some(v),
                                    "codecs" => a.codecs = Some(v),
                                    "lang" => a.lang = Some(v),
                                    "width" => a.width = v.parse::<u32>().ok(),
                                    "height" => a.height = v.parse::<u32>().ok(),
                                    "frameRate" => a.frame_rate = Some(v),
                                    _ => {}
                                }
                            }
                            current_adaptation = Some(a);
                        }
                        "Representation" => {
                            current_rep = Some(parse_representation_attrs(&e)?);
                        }
                        "SegmentTemplate" => {
                            current_template = Some(parse_segment_template_attrs(&e)?);
                        }
                        "SegmentTimeline" => {
                            in_timeline = true;
                        }
                        "SegmentList" => {
                            current_list = Some(parse_segment_list_attrs(&e)?);
                        }
                        "Initialization" => {
                            if let Some(list) = current_list.as_mut() {
                                list.initialization = Some(parse_init_attrs(&e)?);
                            }
                        }
                        "SegmentURL" => {
                            if let Some(list) = current_list.as_mut() {
                                list.segment_urls.push(parse_segment_url_attrs(&e)?);
                            }
                        }
                        "BaseURL" => {
                            in_base_url = true;
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(e)) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    match local.as_str() {
                        "SegmentTemplate" => {
                            let tpl = parse_segment_template_attrs(&e)?;
                            attach_template(tpl, current_rep.as_mut(), current_adaptation.as_mut());
                        }
                        "S" if in_timeline => {
                            if let Some(tpl) = current_template.as_mut() {
                                tpl.timeline.push(parse_timeline_entry(&e)?);
                            }
                        }
                        "Initialization" => {
                            if let Some(list) = current_list.as_mut() {
                                list.initialization = Some(parse_init_attrs(&e)?);
                            }
                        }
                        "SegmentURL" => {
                            if let Some(list) = current_list.as_mut() {
                                list.segment_urls.push(parse_segment_url_attrs(&e)?);
                            }
                        }
                        "Representation" => {
                            let rep = parse_representation_attrs(&e)?;
                            if let Some(adap) = current_adaptation.as_mut() {
                                adap.representations.push(rep);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(e)) => {
                    if in_base_url {
                        let text = e.unescape().map_err(xml_err)?.into_owned();
                        if !text.is_empty() {
                            if let Some(rep) = current_rep.as_mut() {
                                rep.base_urls.push(text);
                            } else if let Some(adap) = current_adaptation.as_mut() {
                                adap.base_urls.push(text);
                            } else if let Some(period) = current_period.as_mut() {
                                period.base_urls.push(text);
                            } else if let Some(m) = manifest.as_mut() {
                                m.base_urls.push(text);
                            }
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    match local.as_str() {
                        "BaseURL" => in_base_url = false,
                        "SegmentTimeline" => in_timeline = false,
                        "SegmentTemplate" => {
                            if let Some(tpl) = current_template.take() {
                                attach_template(
                                    tpl,
                                    current_rep.as_mut(),
                                    current_adaptation.as_mut(),
                                );
                            }
                        }
                        "SegmentList" => {
                            if let Some(list) = current_list.take() {
                                if let Some(rep) = current_rep.as_mut() {
                                    rep.segment_list = Some(list);
                                } else if let Some(adap) = current_adaptation.as_mut() {
                                    adap.segment_list = Some(list);
                                }
                            }
                        }
                        "Representation" => {
                            if let Some(rep) = current_rep.take() {
                                if let Some(adap) = current_adaptation.as_mut() {
                                    adap.representations.push(rep);
                                }
                            }
                        }
                        "AdaptationSet" => {
                            if let Some(adap) = current_adaptation.take() {
                                if let Some(period) = current_period.as_mut() {
                                    period.adaptation_sets.push(adap);
                                }
                            }
                        }
                        "Period" => {
                            if let Some(period) = current_period.take() {
                                if let Some(m) = manifest.as_mut() {
                                    m.periods.push(period);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(xml_err(err)),
                _ => {}
            }
        }

        manifest.ok_or_else(|| {
            MayaError::Protocol("missing <MPD> root element in DASH manifest".to_string())
        })
    }

    /// Return the best video `(AdaptationSet, Representation)` in the given period.
    pub fn best_video_representation(
        &self,
        period_index: usize,
    ) -> Option<(&DashAdaptationSet, &DashRepresentation)> {
        let period = self.periods.get(period_index)?;
        period
            .adaptation_sets
            .iter()
            .filter(|a| a.effective_content_type() == Some(DashContentType::Video))
            .filter_map(|a| a.best_representation().map(|r| (a, r)))
            .max_by_key(|(_, r)| r.bandwidth)
    }

    /// Return the best audio `(AdaptationSet, Representation)` in the given period.
    pub fn best_audio_representation(
        &self,
        period_index: usize,
    ) -> Option<(&DashAdaptationSet, &DashRepresentation)> {
        let period = self.periods.get(period_index)?;
        period
            .adaptation_sets
            .iter()
            .filter(|a| a.effective_content_type() == Some(DashContentType::Audio))
            .filter_map(|a| a.best_representation().map(|r| (a, r)))
            .max_by_key(|(_, r)| r.bandwidth)
    }

    /// Resolve concrete initialization and media segment URLs for a representation.
    pub fn resolve_segments(
        &self,
        manifest_url: Option<&str>,
        period_index: usize,
        adaptation_set: &DashAdaptationSet,
        representation: &DashRepresentation,
    ) -> Result<DashResolvedSegments> {
        let period = self.periods.get(period_index).ok_or_else(|| {
            MayaError::InvalidConfig(format!("invalid DASH period index {period_index}"))
        })?;

        let mut base = manifest_url.map(Url::parse).transpose()?;
        for seg_base in [
            self.base_urls.first(),
            period.base_urls.first(),
            adaptation_set.base_urls.first(),
            representation.base_urls.first(),
        ]
        .into_iter()
        .flatten()
        {
            base = Some(resolve_base(base.as_ref(), seg_base)?);
        }

        if let Some(tpl) = representation
            .segment_template
            .as_ref()
            .or(adaptation_set.segment_template.as_ref())
        {
            let initialization = tpl
                .initialization
                .as_deref()
                .map(|init| {
                    let expanded = expand_template_url(
                        init,
                        &representation.id,
                        None,
                        representation.bandwidth,
                        None,
                    );
                    Ok::<_, MayaError>(DashSegmentLocation {
                        url: join_with_base(base.as_ref(), &expanded)?,
                        byte_range: None,
                    })
                })
                .transpose()?;

            let mut segments = Vec::new();
            if let Some(media_tpl) = tpl.media.as_deref() {
                if !tpl.timeline.is_empty() {
                    let mut time = tpl.timeline[0].t.unwrap_or(0);
                    let mut number = tpl.start_number;
                    for entry in &tpl.timeline {
                        if let Some(explicit_t) = entry.t {
                            time = explicit_t;
                        }
                        let repeat = entry.r.max(0) as u64;
                        for _ in 0..=repeat {
                            let expanded = expand_template_url(
                                media_tpl,
                                &representation.id,
                                Some(number),
                                representation.bandwidth,
                                Some(time),
                            );
                            segments.push(DashSegmentLocation {
                                url: join_with_base(base.as_ref(), &expanded)?,
                                byte_range: None,
                            });
                            time = time.saturating_add(entry.d);
                            number = number.saturating_add(1);
                        }
                    }
                } else if let Some(seg_dur) = tpl.duration {
                    let total_dur = period.duration.or(self.media_presentation_duration);
                    if let Some(total_secs) = total_dur {
                        let timescale = tpl.timescale.max(1) as f64;
                        let seg_secs = (seg_dur as f64) / timescale;
                        let count = if seg_secs > 0.0 {
                            (total_secs / seg_secs).ceil() as u64
                        } else {
                            0
                        };
                        for idx in 0..count {
                            let number = tpl.start_number.saturating_add(idx);
                            let time = idx.saturating_mul(seg_dur);
                            let expanded = expand_template_url(
                                media_tpl,
                                &representation.id,
                                Some(number),
                                representation.bandwidth,
                                Some(time),
                            );
                            segments.push(DashSegmentLocation {
                                url: join_with_base(base.as_ref(), &expanded)?,
                                byte_range: None,
                            });
                        }
                    }
                }
            }

            return Ok(DashResolvedSegments {
                initialization,
                segments,
            });
        }

        if let Some(list) = representation
            .segment_list
            .as_ref()
            .or(adaptation_set.segment_list.as_ref())
        {
            let initialization = list
                .initialization
                .as_ref()
                .and_then(|init| {
                    init.source_url.as_deref().map(|u| {
                        Ok::<_, MayaError>(DashSegmentLocation {
                            url: join_with_base(base.as_ref(), u)?,
                            byte_range: init.range.clone(),
                        })
                    })
                })
                .transpose()?;

            let mut segments = Vec::with_capacity(list.segment_urls.len());
            for seg in &list.segment_urls {
                if let Some(media) = seg.media.as_deref() {
                    segments.push(DashSegmentLocation {
                        url: join_with_base(base.as_ref(), media)?,
                        byte_range: seg.media_range.clone(),
                    });
                }
            }

            return Ok(DashResolvedSegments {
                initialization,
                segments,
            });
        }

        // Single-file BaseURL representation
        if let Some(b) = base {
            return Ok(DashResolvedSegments {
                initialization: None,
                segments: vec![DashSegmentLocation {
                    url: b.to_string(),
                    byte_range: None,
                }],
            });
        }

        Err(MayaError::Protocol(format!(
            "DASH representation `{}` has no SegmentTemplate, SegmentList, or BaseURL",
            representation.id
        )))
    }
}

/// Expand DASH template placeholders (`$RepresentationID$`, `$Number$`, `$Number%0Nd$`, `$Bandwidth$`, `$Time$`, `$$`).
pub fn expand_template_url(
    template: &str,
    rep_id: &str,
    number: Option<u64>,
    bandwidth: u64,
    time: Option<u64>,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        if let Some(end) = after_start.find('$') {
            let token = &after_start[..end];
            if token.is_empty() {
                out.push('$');
            } else if token == "RepresentationID" {
                out.push_str(rep_id);
            } else if token == "Bandwidth" {
                out.push_str(&bandwidth.to_string());
            } else if let Some(fmt_spec) = token.strip_prefix("Number") {
                if let Some(n) = number {
                    out.push_str(&format_dash_number(n, fmt_spec));
                }
            } else if let Some(fmt_spec) = token.strip_prefix("Time") {
                if let Some(t) = time {
                    out.push_str(&format_dash_number(t, fmt_spec));
                }
            } else {
                out.push('$');
                out.push_str(token);
                out.push('$');
            }
            rest = &after_start[end + 1..];
        } else {
            out.push('$');
            out.push_str(after_start);
            return out;
        }
    }

    out.push_str(rest);
    out
}

fn format_dash_number(value: u64, spec: &str) -> String {
    if spec.is_empty() {
        return value.to_string();
    }
    if let Some(width_part) = spec.strip_prefix("%0").and_then(|s| s.strip_suffix('d')) {
        if let Ok(width) = width_part.parse::<usize>() {
            return format!("{value:0>width$}");
        }
    }
    value.to_string()
}

/// Parse an ISO 8601 duration such as `PT1H2M3.5S` or `P0DT0H0M10S` into seconds.
pub fn parse_iso8601_duration(raw: &str) -> Option<f64> {
    let s = raw.trim();
    let after_p = s.strip_prefix('P')?;
    let (date_part, time_part) = match after_p.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (after_p, None),
    };

    let mut total_secs = 0.0;
    if !date_part.is_empty() {
        let mut num_buf = String::new();
        for ch in date_part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                num_buf.push(ch);
            } else {
                let val = num_buf.parse::<f64>().ok()?;
                num_buf.clear();
                match ch {
                    'D' => total_secs += val * 86_400.0,
                    'W' => total_secs += val * 604_800.0,
                    'M' => total_secs += val * 2_592_000.0,
                    'Y' => total_secs += val * 31_536_000.0,
                    _ => return None,
                }
            }
        }
    }

    if let Some(tp) = time_part {
        let mut num_buf = String::new();
        for ch in tp.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                num_buf.push(ch);
            } else {
                let val = num_buf.parse::<f64>().ok()?;
                num_buf.clear();
                match ch {
                    'H' => total_secs += val * 3600.0,
                    'M' => total_secs += val * 60.0,
                    'S' => total_secs += val,
                    _ => return None,
                }
            }
        }
    }

    Some(total_secs)
}

fn attach_template(
    tpl: DashSegmentTemplate,
    rep: Option<&mut DashRepresentation>,
    adap: Option<&mut DashAdaptationSet>,
) {
    if let Some(r) = rep {
        r.segment_template = Some(tpl);
    } else if let Some(a) = adap {
        a.segment_template = Some(tpl);
    }
}

fn parse_representation_attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<DashRepresentation> {
    let mut id = String::new();
    let mut bandwidth = 0u64;
    let mut width = None;
    let mut height = None;
    let mut codecs = None;
    let mut mime_type = None;
    let mut frame_rate = None;
    let mut audio_sampling_rate = None;

    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "id" => id = v,
            "bandwidth" => bandwidth = v.parse::<u64>().unwrap_or(0),
            "width" => width = v.parse::<u32>().ok(),
            "height" => height = v.parse::<u32>().ok(),
            "codecs" => codecs = Some(v),
            "mimeType" => mime_type = Some(v),
            "frameRate" => frame_rate = Some(v),
            "audioSamplingRate" => audio_sampling_rate = v.parse::<u32>().ok(),
            _ => {}
        }
    }

    Ok(DashRepresentation {
        id,
        bandwidth,
        width,
        height,
        codecs,
        mime_type,
        frame_rate,
        audio_sampling_rate,
        base_urls: Vec::new(),
        segment_template: None,
        segment_list: None,
    })
}

fn parse_segment_template_attrs(
    e: &quick_xml::events::BytesStart<'_>,
) -> Result<DashSegmentTemplate> {
    let mut tpl = DashSegmentTemplate::default();
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "media" => tpl.media = Some(v),
            "initialization" => tpl.initialization = Some(v),
            "timescale" => tpl.timescale = v.parse::<u64>().unwrap_or(1),
            "duration" => tpl.duration = v.parse::<u64>().ok(),
            "startNumber" => tpl.start_number = v.parse::<u64>().unwrap_or(1),
            "presentationTimeOffset" => {
                tpl.presentation_time_offset = v.parse::<u64>().unwrap_or(0)
            }
            _ => {}
        }
    }
    Ok(tpl)
}

fn parse_timeline_entry(e: &quick_xml::events::BytesStart<'_>) -> Result<DashTimelineEntry> {
    let mut t = None;
    let mut d = 0u64;
    let mut r = 0i64;
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "t" => t = v.parse::<u64>().ok(),
            "d" => d = v.parse::<u64>().unwrap_or(0),
            "r" => r = v.parse::<i64>().unwrap_or(0),
            _ => {}
        }
    }
    Ok(DashTimelineEntry { t, d, r })
}

fn parse_segment_list_attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<DashSegmentList> {
    let mut list = DashSegmentList::default();
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "timescale" => list.timescale = v.parse::<u64>().unwrap_or(1),
            "duration" => list.duration = v.parse::<u64>().ok(),
            "startNumber" => list.start_number = v.parse::<u64>().unwrap_or(1),
            _ => {}
        }
    }
    Ok(list)
}

fn parse_init_attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<DashSegmentResource> {
    let mut source_url = None;
    let mut range = None;
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "sourceURL" => source_url = Some(v),
            "range" => range = Some(v),
            _ => {}
        }
    }
    Ok(DashSegmentResource { source_url, range })
}

fn parse_segment_url_attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<DashSegmentUrl> {
    let mut media = None;
    let mut media_range = None;
    let mut index_range = None;
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let k = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let v = attr.unescape_value().map_err(xml_err)?.into_owned();
        match k.as_str() {
            "media" => media = Some(v),
            "mediaRange" => media_range = Some(v),
            "indexRange" => index_range = Some(v),
            _ => {}
        }
    }
    Ok(DashSegmentUrl {
        media,
        media_range,
        index_range,
    })
}

fn classify_content_type(raw: &str) -> DashContentType {
    match raw.trim().to_ascii_lowercase().as_str() {
        "video" => DashContentType::Video,
        "audio" => DashContentType::Audio,
        "text" | "subtitles" => DashContentType::Subtitles,
        other => DashContentType::Other(other.to_string()),
    }
}

fn classify_mime(mime: &str) -> DashContentType {
    let lower = mime.trim().to_ascii_lowercase();
    if lower.starts_with("video/") {
        DashContentType::Video
    } else if lower.starts_with("audio/") {
        DashContentType::Audio
    } else if lower.starts_with("text/") || lower.contains("ttml") || lower.contains("vtt") {
        DashContentType::Subtitles
    } else {
        DashContentType::Other(lower)
    }
}

fn resolve_base(current: Option<&Url>, next: &str) -> Result<Url> {
    match current {
        Some(c) => Ok(c.join(next)?),
        None => Ok(Url::parse(next)?),
    }
}

fn join_with_base(base: Option<&Url>, rel: &str) -> Result<String> {
    match base {
        Some(b) => Ok(b.join(rel)?.to_string()),
        None => Ok(rel.to_string()),
    }
}

fn xml_err(err: impl std::fmt::Display) -> MayaError {
    MayaError::Protocol(format!("DASH MPD XML parse error: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601_durations() {
        assert_eq!(parse_iso8601_duration("PT10S"), Some(10.0));
        assert_eq!(parse_iso8601_duration("PT2M30.5S"), Some(150.5));
        assert_eq!(parse_iso8601_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_iso8601_duration("P0DT0H1M0S"), Some(60.0));
        assert_eq!(parse_iso8601_duration("invalid"), None);
    }

    #[test]
    fn parses_mpd_with_segment_template_and_timeline() {
        let mpd = r#"<?xml version="1.0" encoding="utf-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT12S" minBufferTime="PT2S">
  <BaseURL>https://cdn.example.com/dash/</BaseURL>
  <Period id="p0" duration="PT12S">
    <AdaptationSet id="1" contentType="video" mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate timescale="1000" initialization="$RepresentationID$/init.m4s" media="$RepresentationID$/seg-$Number%04d$.m4s" startNumber="1">
        <SegmentTimeline>
          <S t="0" d="4000" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="720p" bandwidth="1500000" width="1280" height="720" frameRate="30"/>
      <Representation id="1080p" bandwidth="4000000" width="1920" height="1080" frameRate="60"/>
    </AdaptationSet>
    <AdaptationSet id="2" mimeType="audio/mp4" lang="en">
      <Representation id="aac" bandwidth="128000" audioSamplingRate="48000" codecs="mp4a.40.2">
        <SegmentTemplate timescale="1000" duration="4000" startNumber="1" initialization="audio/init.m4s" media="audio/seg-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = DashManifest::parse(mpd).unwrap();
        assert_eq!(manifest.manifest_type.as_deref(), Some("static"));
        assert_eq!(manifest.media_presentation_duration, Some(12.0));
        assert_eq!(manifest.periods.len(), 1);

        let (v_adap, v_rep) = manifest.best_video_representation(0).unwrap();
        assert_eq!(v_rep.id, "1080p");
        assert_eq!(v_rep.bandwidth, 4_000_000);
        assert_eq!(v_rep.width, Some(1920));

        let v_resolved = manifest.resolve_segments(None, 0, v_adap, v_rep).unwrap();
        assert_eq!(
            v_resolved.initialization.unwrap().url,
            "https://cdn.example.com/dash/1080p/init.m4s"
        );
        assert_eq!(v_resolved.segments.len(), 3);
        assert_eq!(
            v_resolved.segments[0].url,
            "https://cdn.example.com/dash/1080p/seg-0001.m4s"
        );
        assert_eq!(
            v_resolved.segments[2].url,
            "https://cdn.example.com/dash/1080p/seg-0003.m4s"
        );

        let (a_adap, a_rep) = manifest.best_audio_representation(0).unwrap();
        assert_eq!(a_rep.id, "aac");
        assert_eq!(a_rep.audio_sampling_rate, Some(48_000));

        let a_resolved = manifest.resolve_segments(None, 0, a_adap, a_rep).unwrap();
        assert_eq!(
            a_resolved.initialization.unwrap().url,
            "https://cdn.example.com/dash/audio/init.m4s"
        );
        assert_eq!(a_resolved.segments.len(), 3);
        assert_eq!(
            a_resolved.segments[1].url,
            "https://cdn.example.com/dash/audio/seg-2.m4s"
        );
    }

    #[test]
    fn parses_mpd_with_segment_list() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT8S">
  <Period>
    <AdaptationSet mimeType="video/webm">
      <Representation id="vp9" bandwidth="2200000" width="1280" height="720">
        <SegmentList timescale="1000" duration="4000">
          <Initialization sourceURL="init.webm" range="0-511"/>
          <SegmentURL media="part1.webm" mediaRange="512-2047"/>
          <SegmentURL media="part2.webm" mediaRange="2048-4095"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

        let manifest = DashManifest::parse(mpd).unwrap();
        let (adap, rep) = manifest.best_video_representation(0).unwrap();
        assert_eq!(rep.id, "vp9");

        let resolved = manifest
            .resolve_segments(
                Some("https://media.example.com/vod/stream.mpd"),
                0,
                adap,
                rep,
            )
            .unwrap();
        let init = resolved.initialization.unwrap();
        assert_eq!(init.url, "https://media.example.com/vod/init.webm");
        assert_eq!(init.byte_range.as_deref(), Some("0-511"));
        assert_eq!(resolved.segments.len(), 2);
        assert_eq!(
            resolved.segments[0].url,
            "https://media.example.com/vod/part1.webm"
        );
        assert_eq!(resolved.segments[0].byte_range.as_deref(), Some("512-2047"));
        assert_eq!(
            resolved.segments[1].url,
            "https://media.example.com/vod/part2.webm"
        );
    }
}
