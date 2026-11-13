use std::io::{self, Write};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use maya_core::ProgressEvent;
use tokio::sync::mpsc;

/// Renders [`ProgressEvent`] updates either as terminal progress bars (human mode)
/// or newline-delimited JSON lines on stdout (`--json` mode).
pub struct ProgressRenderer;

impl ProgressRenderer {
    /// Consume events from `rx` until the channel closes, rendering them according to `json_mode`.
    pub async fn run(mut rx: mpsc::Receiver<ProgressEvent>, json_mode: bool) {
        if json_mode {
            let mut stdout = io::BufWriter::new(io::stdout());
            while let Some(event) = rx.recv().await {
                if let Ok(line) = serde_json::to_string(&event) {
                    let _ = writeln!(stdout, "{line}");
                    let _ = stdout.flush();
                }
            }
            return;
        }

        let mut bar: Option<ProgressBar> = None;
        let mut known_total: Option<u64> = None;
        let mut label = String::from("downloading");

        while let Some(event) = rx.recv().await {
            match event {
                ProgressEvent::Probed {
                    total_size,
                    filename,
                    ..
                } => {
                    if let Some(name) = filename {
                        label = name;
                    }
                    known_total = total_size;
                    if let Some(pb) = &bar {
                        pb.set_message(label.clone());
                    }
                }
                ProgressEvent::Started {
                    total_size,
                    segment_count,
                } => {
                    known_total = total_size.or(known_total);
                    let pb = create_progress_bar(known_total, &label, segment_count);
                    bar = Some(pb);
                }
                ProgressEvent::SegmentProgress {
                    total_downloaded,
                    total_size,
                    ..
                } => {
                    let pb = bar.get_or_insert_with(|| {
                        known_total = total_size.or(known_total);
                        create_progress_bar(known_total, &label, 1)
                    });

                    if known_total.is_none() {
                        if let Some(total) = total_size {
                            known_total = Some(total);
                            pb.set_length(total);
                            pb.set_style(sized_bar_style());
                        }
                    }

                    pb.set_position(total_downloaded);
                }
                ProgressEvent::SegmentCompleted { .. } => {}
                ProgressEvent::Verifying { algorithm } => {
                    if let Some(pb) = &bar {
                        let algo_name = algorithm.as_str();
                        pb.set_message(format!("verifying ({algo_name})"));
                    }
                }
                ProgressEvent::Completed {
                    output_path,
                    total_bytes,
                } => {
                    if let Some(pb) = bar.take() {
                        pb.finish_and_clear();
                    }
                    println!(
                        "Downloaded {} ({})",
                        output_path.display(),
                        format_bytes(total_bytes)
                    );
                }
                ProgressEvent::Failed { .. } => {
                    if let Some(pb) = bar.take() {
                        pb.finish_and_clear();
                    }
                }
            }
        }

        if let Some(pb) = bar.take() {
            pb.finish_and_clear();
        }
    }
}

fn create_progress_bar(total_size: Option<u64>, label: &str, segment_count: usize) -> ProgressBar {
    let pb = match total_size {
        Some(total) => {
            let pb = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr());
            pb.set_style(sized_bar_style());
            pb
        }
        None => {
            let pb = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
            pb.set_style(spinner_style());
            pb
        }
    };
    pb.enable_steady_tick(Duration::from_millis(120));
    if segment_count > 1 {
        pb.set_message(format!("{label} ({segment_count}x)"));
    } else {
        pb.set_message(label.to_string());
    }
    pb
}

fn sized_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("#>-")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] {bytes} ({bytes_per_sec}) {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Format a byte count into a human-readable IEC string.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.2} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn serializes_progress_events_as_tagged_ndjson() {
        let ev = ProgressEvent::Completed {
            output_path: PathBuf::from("archive.tar.gz"),
            total_bytes: 2048,
        };
        let json = serde_json::to_string(&ev).expect("serialize ProgressEvent");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["event"], "completed");
        assert_eq!(parsed["total_bytes"], 2048);
    }

    #[test]
    fn formats_human_readable_byte_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_572_864), "1.50 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
    }

    #[tokio::test]
    async fn progress_renderer_exits_when_channel_closes() {
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(ProgressRenderer::run(rx, false));
        tx.send(ProgressEvent::Started {
            total_size: Some(100),
            segment_count: 2,
        })
        .await
        .unwrap();
        tx.send(ProgressEvent::SegmentProgress {
            segment_index: 0,
            bytes_delta: 50,
            total_downloaded: 50,
            total_size: Some(100),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.expect("renderer task should finish cleanly");
    }
}
