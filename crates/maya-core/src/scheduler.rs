use std::sync::Mutex;

use crate::error::{MayaError, Result};
use crate::types::{DownloadState, Segment, SegmentState};

/// Slice assignment handed to a download worker by [`SegmentScheduler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkAssignment {
    pub segment_index: usize,
    pub start_offset: u64,
    pub end: Option<u64>,
}

/// Result of reserving/recording a received chunk against a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    pub write_offset: u64,
    pub accepted_bytes: u64,
    pub total_downloaded: u64,
    pub segment_completed: bool,
}

/// Thread-safe IDM-style segment scheduler with in-half division work-stealing.
///
/// Supports fresh initialization (both range-capable and single-stream /
/// unknown-length downloads) as well as resuming from [`DownloadState`].
#[derive(Debug)]
pub struct SegmentScheduler {
    inner: Mutex<SchedulerInner>,
}

#[derive(Debug)]
struct SchedulerInner {
    total_size: Option<u64>,
    supports_ranges: bool,
    min_segment_size: u64,
    total_downloaded: u64,
    segments: Vec<Segment>,
}

impl SegmentScheduler {
    /// Initialize a fresh scheduler for a resource.
    pub fn new(
        total_size: Option<u64>,
        supports_ranges: bool,
        connections: usize,
        min_segment_size: u64,
    ) -> Self {
        let connections = connections.max(1);
        let min_segment_size = min_segment_size.max(1);

        let segments = match total_size {
            Some(0) => vec![Segment {
                index: 0,
                start: 0,
                end: Some(0),
                downloaded: 0,
                state: SegmentState::Completed,
            }],
            Some(total) if supports_ranges && connections > 1 && total > min_segment_size => {
                let max_by_min = (total / min_segment_size).max(1) as usize;
                let count = connections.min(max_by_min).max(1);
                let base = total / count as u64;
                let rem = total % count as u64;

                let mut segs = Vec::with_capacity(count);
                let mut cursor = 0u64;
                for idx in 0..count {
                    let len = base + u64::from((idx as u64) < rem);
                    let end = cursor + len;
                    segs.push(Segment {
                        index: idx,
                        start: cursor,
                        end: Some(end),
                        downloaded: 0,
                        state: SegmentState::Pending,
                    });
                    cursor = end;
                }
                segs
            }
            Some(total) => vec![Segment {
                index: 0,
                start: 0,
                end: Some(total),
                downloaded: 0,
                state: SegmentState::Pending,
            }],
            None => vec![Segment {
                index: 0,
                start: 0,
                end: None,
                downloaded: 0,
                state: SegmentState::Pending,
            }],
        };

        Self {
            inner: Mutex::new(SchedulerInner {
                total_size,
                supports_ranges,
                min_segment_size,
                total_downloaded: 0,
                segments,
            }),
        }
    }

    /// Resume scheduling from a persisted [`DownloadState`].
    ///
    /// Resets any incomplete `Active` or `Failed` segments back to `Pending`
    /// while preserving their persisted `downloaded` byte counts.
    pub fn from_state(
        state: &DownloadState,
        supports_ranges: bool,
        _connections: usize,
        min_segment_size: u64,
    ) -> Result<Self> {
        if state.segments.is_empty() {
            return Err(MayaError::ResumeMismatch(
                "saved download state contains no segments".to_string(),
            ));
        }

        let min_segment_size = min_segment_size.max(1);
        let mut segments = state.segments.clone();
        let mut total_downloaded = 0u64;

        for (expected_idx, seg) in segments.iter_mut().enumerate() {
            seg.index = expected_idx;
            if let Some(len) = seg.len() {
                if seg.downloaded > len {
                    seg.downloaded = len;
                }
            }
            if seg.is_complete() {
                seg.state = SegmentState::Completed;
            } else if matches!(seg.state, SegmentState::Active | SegmentState::Failed) {
                seg.state = SegmentState::Pending;
            }
            total_downloaded = total_downloaded.saturating_add(seg.downloaded);
        }

        Ok(Self {
            inner: Mutex::new(SchedulerInner {
                total_size: state.total_size,
                supports_ranges,
                min_segment_size,
                total_downloaded,
                segments,
            }),
        })
    }

    /// Claim the next pending segment, or steal the second half `[mid..end)`
    /// of the active/pending segment with the largest remaining bytes greater
    /// than `min_segment_size`.
    pub fn next_work(&self) -> Option<WorkAssignment> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // 1. Prefer any unstarted/pending segment first.
        for idx in 0..guard.segments.len() {
            if guard.segments[idx].state == SegmentState::Pending {
                if guard.segments[idx].is_complete() {
                    guard.segments[idx].state = SegmentState::Completed;
                    continue;
                }
                guard.segments[idx].state = SegmentState::Active;
                return Some(WorkAssignment {
                    segment_index: idx,
                    start_offset: guard.segments[idx].current_offset(),
                    end: guard.segments[idx].end,
                });
            }
        }

        // 2. Otherwise steal half of the active/pending segment with the largest remaining bytes.
        if !guard.supports_ranges {
            return None;
        }

        let min_seg = guard.min_segment_size;
        let victim = guard
            .segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| matches!(seg.state, SegmentState::Active | SegmentState::Pending))
            .filter_map(|(idx, seg)| {
                let end = seg.end?;
                let remaining = end.saturating_sub(seg.current_offset());
                if remaining > min_seg && remaining >= 2 {
                    Some((idx, remaining, end))
                } else {
                    None
                }
            })
            .max_by_key(|&(_, remaining, _)| remaining);

        let (victim_idx, remaining, victim_end) = victim?;
        let steal_bytes = remaining / 2;
        let mid = victim_end - steal_bytes;

        guard.segments[victim_idx].end = Some(mid);
        let new_idx = guard.segments.len();
        guard.segments.push(Segment {
            index: new_idx,
            start: mid,
            end: Some(victim_end),
            downloaded: 0,
            state: SegmentState::Active,
        });

        Some(WorkAssignment {
            segment_index: new_idx,
            start_offset: mid,
            end: Some(victim_end),
        })
    }

    /// Reserve and record up to `chunk_len` bytes for `segment_index`, clamping
    /// against `seg.end` in case another worker stole the tail of this segment.
    ///
    /// Call this before `StorageWriter::write_at` so concurrent steals cannot
    /// overlap already-accepted bytes.
    pub fn record_write(&self, segment_index: usize, chunk_len: u64) -> Result<WriteOutcome> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let (write_offset, accepted_bytes, segment_completed) = {
            let seg = guard.segments.get_mut(segment_index).ok_or_else(|| {
                MayaError::InvalidConfig(format!("invalid segment index: {segment_index}"))
            })?;

            let write_offset = seg.current_offset();
            if seg.state == SegmentState::Completed {
                return Ok(WriteOutcome {
                    write_offset,
                    accepted_bytes: 0,
                    total_downloaded: guard.total_downloaded,
                    segment_completed: true,
                });
            }

            let accepted = match seg.end {
                Some(end) => chunk_len.min(end.saturating_sub(write_offset)),
                None => chunk_len,
            };

            seg.downloaded = seg.downloaded.saturating_add(accepted);
            let completed = seg.end.is_some_and(|end| seg.current_offset() >= end);
            if completed {
                seg.state = SegmentState::Completed;
            }
            (write_offset, accepted, completed)
        };

        guard.total_downloaded = guard.total_downloaded.saturating_add(accepted_bytes);

        Ok(WriteOutcome {
            write_offset,
            accepted_bytes,
            total_downloaded: guard.total_downloaded,
            segment_completed,
        })
    }

    /// Roll back a previously recorded chunk if the underlying storage write failed.
    pub fn rollback_write(&self, segment_index: usize, bytes: u64) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut rolled_back = 0u64;
        if let Some(seg) = guard.segments.get_mut(segment_index) {
            rolled_back = seg.downloaded.min(bytes);
            seg.downloaded -= rolled_back;
            if seg.state == SegmentState::Completed && !seg.is_complete() {
                seg.state = SegmentState::Active;
            }
        }
        guard.total_downloaded = guard.total_downloaded.saturating_sub(rolled_back);
    }

    /// Mark a segment completed (used for unknown-length streams upon EOF).
    pub fn mark_completed(&self, segment_index: usize) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(seg) = guard.segments.get_mut(segment_index) {
            seg.state = SegmentState::Completed;
        }
    }

    /// Mark a segment failed after exhausting retries.
    pub fn mark_failed(&self, segment_index: usize) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(seg) = guard.segments.get_mut(segment_index) {
            seg.state = SegmentState::Failed;
        }
    }

    /// Prepare a segment for retry.
    ///
    /// When `supports_ranges` is `false`, resets `downloaded` to `0` so the
    /// retry restarts from offset `0`. Returns `None` if the segment was
    /// already completed (for instance via work-stealing) while waiting.
    pub fn prepare_retry(&self, segment_index: usize) -> Option<WorkAssignment> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let supports_ranges = guard.supports_ranges;
        let mut subtracted = 0u64;
        let assignment = {
            let seg = guard.segments.get_mut(segment_index)?;
            if !supports_ranges {
                subtracted = seg.downloaded;
                seg.downloaded = 0;
                seg.state = SegmentState::Active;
                Some(WorkAssignment {
                    segment_index,
                    start_offset: seg.start,
                    end: seg.end,
                })
            } else if seg.is_complete() {
                seg.state = SegmentState::Completed;
                None
            } else {
                seg.state = SegmentState::Active;
                Some(WorkAssignment {
                    segment_index,
                    start_offset: seg.current_offset(),
                    end: seg.end,
                })
            }
        };

        if subtracted > 0 {
            guard.total_downloaded = guard.total_downloaded.saturating_sub(subtracted);
        }
        assignment
    }

    /// Total bytes recorded across all segments so far.
    pub fn total_downloaded(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .total_downloaded
    }

    /// Known total size of the target resource, if any.
    pub fn total_size(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .total_size
    }

    /// Current number of segments (initial + stolen).
    pub fn segment_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .segments
            .len()
    }

    /// Snapshot of all segments for checkpointing or inspection.
    pub fn segments(&self) -> Vec<Segment> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .segments
            .clone()
    }

    /// Return `true` if `segment_index` exists and is completed.
    pub fn is_segment_complete(&self, segment_index: usize) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .segments
            .get(segment_index)
            .is_some_and(|seg| seg.state == SegmentState::Completed || seg.is_complete())
    }

    /// Return `true` when every segment is [`SegmentState::Completed`].
    pub fn is_finished(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .segments
            .iter()
            .all(|seg| seg.state == SegmentState::Completed)
    }

    /// Return `true` if any segment is in [`SegmentState::Failed`].
    pub fn has_failed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .segments
            .iter()
            .any(|seg| seg.state == SegmentState::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn initializes_even_segments_for_range_downloads() {
        let sched = SegmentScheduler::new(Some(1_003), true, 4, 100);
        let segs = sched.segments();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, Some(251));
        assert_eq!(segs[1].start, 251);
        assert_eq!(segs[1].end, Some(502));
        assert_eq!(segs[2].start, 502);
        assert_eq!(segs[2].end, Some(753));
        assert_eq!(segs[3].start, 753);
        assert_eq!(segs[3].end, Some(1_003));
    }

    #[test]
    fn initializes_single_segment_when_ranges_unsupported_or_unknown_size() {
        let s1 = SegmentScheduler::new(Some(10_000), false, 4, 100);
        assert_eq!(s1.segment_count(), 1);
        assert_eq!(s1.segments()[0].end, Some(10_000));

        let s2 = SegmentScheduler::new(None, true, 4, 100);
        assert_eq!(s2.segment_count(), 1);
        assert_eq!(s2.segments()[0].end, None);

        let s3 = SegmentScheduler::new(Some(0), true, 4, 100);
        assert!(s3.is_finished());
        assert_eq!(s3.total_downloaded(), 0);
    }

    #[test]
    fn work_stealing_splits_largest_active_segment_in_half() {
        let sched = SegmentScheduler::new(Some(1_000), true, 1, 100);
        let w1 = sched.next_work().expect("initial segment");
        assert_eq!(
            w1,
            WorkAssignment {
                segment_index: 0,
                start_offset: 0,
                end: Some(1_000),
            }
        );

        // Worker 1 downloads 200 bytes -> 800 remaining > 100.
        let out = sched.record_write(0, 200).unwrap();
        assert_eq!(out.accepted_bytes, 200);
        assert!(!out.segment_completed);

        // Idle worker steals half of remaining [200..1000) -> [600..1000).
        let w2 = sched.next_work().expect("stolen segment");
        assert_eq!(
            w2,
            WorkAssignment {
                segment_index: 1,
                start_offset: 600,
                end: Some(1_000),
            }
        );

        // Worker 1 sends a chunk of 500 bytes, clamped to stolen boundary 600 (400 accepted).
        let clamped = sched.record_write(0, 500).unwrap();
        assert_eq!(clamped.write_offset, 200);
        assert_eq!(clamped.accepted_bytes, 400);
        assert!(clamped.segment_completed);

        // Worker 2 finishes its 400 bytes.
        let w2_out = sched.record_write(1, 400).unwrap();
        assert!(w2_out.segment_completed);
        assert_eq!(w2_out.total_downloaded, 1_000);
        assert!(sched.is_finished());
        assert!(sched.next_work().is_none());
    }

    #[test]
    fn work_stealing_respects_min_segment_size() {
        let sched = SegmentScheduler::new(Some(200), true, 1, 100);
        let _ = sched.next_work().unwrap();
        sched.record_write(0, 100).unwrap();
        // Remaining is 100 == min_segment_size (not strictly greater), so do not steal.
        assert!(sched.next_work().is_none());
    }

    #[test]
    fn from_state_resets_incomplete_active_and_failed_segments() {
        let state = DownloadState {
            version: 1,
            source: "https://example.com/a.bin".to_string(),
            resolved_url: "https://example.com/a.bin".to_string(),
            target_path: PathBuf::from("a.bin"),
            part_path: PathBuf::from("a.bin.maya.part"),
            total_size: Some(1_000),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
            segments: vec![
                Segment {
                    index: 0,
                    start: 0,
                    end: Some(500),
                    downloaded: 500,
                    state: SegmentState::Completed,
                },
                Segment {
                    index: 1,
                    start: 500,
                    end: Some(750),
                    downloaded: 100,
                    state: SegmentState::Active,
                },
                Segment {
                    index: 2,
                    start: 750,
                    end: Some(1_000),
                    downloaded: 50,
                    state: SegmentState::Failed,
                },
            ],
        };

        let sched = SegmentScheduler::from_state(&state, true, 4, 50).unwrap();
        assert_eq!(sched.total_downloaded(), 650);
        let segs = sched.segments();
        assert_eq!(segs[0].state, SegmentState::Completed);
        assert_eq!(segs[1].state, SegmentState::Pending);
        assert_eq!(segs[1].downloaded, 100);
        assert_eq!(segs[2].state, SegmentState::Pending);
        assert_eq!(segs[2].downloaded, 50);

        let a = sched.next_work().unwrap();
        assert_eq!(a.segment_index, 1);
        assert_eq!(a.start_offset, 600);
        assert_eq!(a.end, Some(750));

        let b = sched.next_work().unwrap();
        assert_eq!(b.segment_index, 2);
        assert_eq!(b.start_offset, 800);
        assert_eq!(b.end, Some(1_000));
    }

    #[test]
    fn prepare_retry_resets_to_zero_when_ranges_unsupported() {
        let sched = SegmentScheduler::new(Some(1_000), false, 4, 100);
        let w = sched.next_work().unwrap();
        assert_eq!(w.segment_index, 0);
        sched.record_write(0, 300).unwrap();
        assert_eq!(sched.total_downloaded(), 300);

        let retry = sched.prepare_retry(0).unwrap();
        assert_eq!(retry.start_offset, 0);
        assert_eq!(retry.end, Some(1_000));
        assert_eq!(sched.total_downloaded(), 0);
    }
}
