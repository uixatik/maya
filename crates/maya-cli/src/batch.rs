//! Batch downloading support for Maya Downloader.
//!
//! Parses URLs from text files, skipping comments (`#`) and empty lines, and
//! executes downloads with concurrency controls and summary reporting.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use maya_core::{DownloadRequest, MayaError, SecurityPolicy};
use tokio::sync::Semaphore;

use crate::runner;

/// Options controlling batch download execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOptions {
    /// Maximum simultaneous active downloads.
    pub concurrency: usize,
    /// Number of concurrent connection segments per download.
    pub connections: usize,
    /// Destination directory for output files.
    pub dir: PathBuf,
    /// Optional throughput limit in bytes per second.
    pub limit: Option<u64>,
    /// Allow connections to loopback/private/internal IP addresses.
    pub allow_private_ips: bool,
    /// Enforce HTTPS and disallow plaintext HTTP.
    pub strict_https: bool,
    /// Emit progress as JSON.
    pub json: bool,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            concurrency: 3,
            connections: 4,
            dir: PathBuf::from("."),
            limit: None,
            allow_private_ips: false,
            strict_https: false,
            json: false,
        }
    }
}

/// Outcome of a single URL download in a batch run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResult {
    pub url: String,
    pub success: bool,
    pub output_path: Option<PathBuf>,
    pub error: Option<String>,
}

/// Summary report of a completed batch download job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSummary {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub results: Vec<BatchResult>,
}

/// Parse URLs from a text file or string, stripping `#` comments and empty lines.
pub fn parse_batch_urls(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| {
            // Strip comments starting with '#'
            if let Some((clean, _comment)) = line.split_once('#') {
                clean.trim()
            } else {
                line.trim()
            }
        })
        .filter(|line| !line.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Read and parse URLs from a file on disk.
pub fn parse_batch_file(path: &Path) -> std::io::Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(parse_batch_urls(&content))
}

/// Run batch downloading for a list of URLs with concurrency control.
pub async fn run_batch_downloads(urls: Vec<String>, options: BatchOptions) -> BatchSummary {
    let total = urls.len();
    if total == 0 {
        if !options.json {
            println!("[maya] No valid URLs found in batch list.");
        }
        return BatchSummary {
            total: 0,
            succeeded: 0,
            failed: 0,
            results: Vec::new(),
        };
    }

    if !options.json {
        println!(
            "[maya] Starting batch download of {} URLs (concurrency: {})...",
            total, options.concurrency
        );
    }

    let concurrency = options.concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let completed_counter = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::with_capacity(total);

    for (index, url) in urls.into_iter().enumerate() {
        let sem = Arc::clone(&semaphore);
        let counter = Arc::clone(&completed_counter);
        let opts = options.clone();

        let task = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            let current_idx = index + 1;
            if !opts.json {
                println!("[maya] [{current_idx}/{total}] Starting download: {url}");
            }

            let mut req = DownloadRequest::new(&url, &opts.dir);
            req.connections = opts.connections;
            req.max_bytes_per_sec = opts.limit;
            req.resume = true;
            req.security = if opts.strict_https {
                SecurityPolicy::strict()
            } else {
                SecurityPolicy::default()
            };
            req.security.allow_private_ips = opts.allow_private_ips;

            let res = runner::run_download_ext(req, false, opts.json, false).await;
            let _done_idx = counter.fetch_add(1, Ordering::Relaxed) + 1;

            match res {
                Ok(path) => {
                    if !opts.json {
                        println!(
                            "[maya] [{current_idx}/{total}] Succeeded: {url} -> {}",
                            path.display()
                        );
                    }
                    BatchResult {
                        url,
                        success: true,
                        output_path: Some(path),
                        error: None,
                    }
                }
                Err(err) => {
                    let err_msg = err.to_string();
                    if !opts.json {
                        eprintln!("[maya] [{current_idx}/{total}] Failed: {url} - {err_msg}");
                    }
                    BatchResult {
                        url,
                        success: false,
                        output_path: None,
                        error: Some(err_msg),
                    }
                }
            }
        });

        tasks.push(task);
    }

    let mut results = Vec::with_capacity(total);
    let mut succeeded = 0;
    let mut failed = 0;

    for task in tasks {
        match task.await {
            Ok(item) => {
                if item.success {
                    succeeded += 1;
                } else {
                    failed += 1;
                }
                results.push(item);
            }
            Err(join_err) => {
                failed += 1;
                results.push(BatchResult {
                    url: "<task panicked>".to_string(),
                    success: false,
                    output_path: None,
                    error: Some(format!("task error: {join_err}")),
                });
            }
        }
    }

    if !options.json {
        println!(
            "[maya] Batch download complete: {succeeded} succeeded, {failed} failed (out of {total} total)"
        );
        if failed > 0 {
            eprintln!("[maya] Failed URLs:");
            for r in &results {
                if !r.success {
                    eprintln!(
                        "  - {}: {}",
                        r.url,
                        r.error.as_deref().unwrap_or("unknown error")
                    );
                }
            }
        }
    }

    BatchSummary {
        total,
        succeeded,
        failed,
        results,
    }
}

/// Execute batch download command from CLI arguments.
pub async fn run_batch(path: &Path, options: BatchOptions) -> maya_core::Result<BatchSummary> {
    let urls = parse_batch_file(path).map_err(|e| {
        MayaError::InvalidConfig(format!(
            "failed to read batch file `{}`: {e}",
            path.display()
        ))
    })?;

    Ok(run_batch_downloads(urls, options).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_batch_urls() {
        let content = r#"
        # Sample batch file
        https://example.com/file1.zip
        https://example.com/file2.tar.gz # inline comment
        
        # Another section
           https://example.com/file3.iso   
        "#;

        let urls = parse_batch_urls(content);
        assert_eq!(
            urls,
            vec![
                "https://example.com/file1.zip",
                "https://example.com/file2.tar.gz",
                "https://example.com/file3.iso",
            ]
        );
    }

    #[test]
    fn test_parse_batch_empty() {
        let content = "# only comments\n   \n# and blanks\n";
        let urls = parse_batch_urls(content);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_batch_options_default() {
        let opts = BatchOptions::default();
        assert_eq!(opts.concurrency, 3);
        assert_eq!(opts.connections, 4);
        assert_eq!(opts.dir, PathBuf::from("."));
        assert!(!opts.allow_private_ips);
        assert!(!opts.strict_https);
    }
}
