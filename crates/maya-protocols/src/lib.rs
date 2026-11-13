//! Protocol clients (HTTP/HTTPS, BitTorrent/Magnet, HLS, DASH, Media Extractor) for Maya downloader.

pub mod dash;
pub mod extractor;
pub mod hls;
pub mod http;
pub mod torrent;

pub use dash::{
    expand_template_url, parse_iso8601_duration, DashAdaptationSet, DashContentType, DashManifest,
    DashPeriod, DashRepresentation, DashResolvedSegments, DashSegmentList, DashSegmentLocation,
    DashSegmentResource, DashSegmentTemplate, DashSegmentUrl, DashTimelineEntry,
};
pub use extractor::{
    parse_ytdlp_json, MediaExtractor, MediaFormatStream, MediaStreamInfo, SelectedStreams,
    YtDlpCommand,
};
pub use hls::{
    decrypt_aes128_cbc, media_sequence_to_iv, parse_hex_iv, ByteRange, HlsInitSegment, HlsKey,
    HlsKeyMethod, HlsMasterPlaylist, HlsMediaPlaylist, HlsPlaylist, HlsSegment, HlsSegmentFetcher,
    HlsSegmentStream, HlsVariant,
};
pub use http::HttpProtocolClient;
pub use torrent::TorrentClient;
