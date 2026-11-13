//! Maya background download daemon (`mayad`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use maya_ipc::{default_ipc_endpoint, IpcClient, IpcServer};
use tokio::sync::watch;

mod db;
mod manager;

use db::{default_db_path, JobStore};
use manager::QueueManager;

#[derive(Parser, Debug)]
#[command(
    name = "mayad",
    version,
    about = "Background download manager daemon for Maya"
)]
struct Cli {
    /// Run daemon in background (detached process)
    #[arg(long)]
    background: bool,

    /// Stop running daemon instance via IPC
    #[arg(long)]
    stop: bool,

    /// Query running daemon status and queue summary via IPC
    #[arg(long)]
    status: bool,

    /// Custom path to SQLite queue database (default: ~/.maya/queue.db)
    #[arg(long)]
    db: Option<PathBuf>,

    /// Custom IPC endpoint (Windows named pipe or Unix domain socket)
    #[arg(long)]
    endpoint: Option<String>,

    /// Maximum concurrent downloads
    #[arg(long, default_value = "3")]
    max_concurrent: usize,

    /// Internal flag used when daemon is spawned in background
    #[arg(long, hide = true)]
    daemon_child: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let endpoint = cli.endpoint.clone().unwrap_or_else(default_ipc_endpoint);

    if cli.status {
        return run_status_client(&endpoint).await;
    }

    if cli.stop {
        return run_stop_client(&endpoint).await;
    }

    if cli.background && !cli.daemon_child {
        return spawn_background(&cli);
    }

    run_daemon_server(cli, endpoint).await
}

async fn run_status_client(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = match IpcClient::connect_with_timeout(endpoint, Duration::from_millis(1500)).await
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Error: unable to connect to Maya daemon at `{endpoint}`: {err}");
            eprintln!("Is `mayad` running?");
            std::process::exit(1);
        }
    };

    let status = client.daemon_status().await?;
    println!("=== Maya Daemon Status ===");
    println!("Version:          {}", status.version);
    println!("Protocol Version: {}", status.protocol_version);
    println!("PID:              {}", status.pid);
    println!("Uptime:           {}s", status.uptime_secs);
    println!("Active Downloads: {}", status.active_jobs);
    println!("Queued Downloads: {}", status.queued_jobs);
    println!("Total Jobs:       {}", status.total_jobs);

    if let Ok(jobs) = client.list_jobs().await {
        if !jobs.is_empty() {
            println!("\n=== Tracked Jobs ===");
            for job in jobs {
                let total_str = job
                    .total_bytes
                    .map(|b| format!("{b} B"))
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "[{}] {:<12} {:>10} / {:<10} {} -> {}",
                    &job.id[..job.id.len().min(8)],
                    job.status.as_str(),
                    format!("{} B", job.downloaded_bytes),
                    total_str,
                    job.url,
                    job.dest
                );
            }
        }
    }

    Ok(())
}

async fn run_stop_client(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = match IpcClient::connect_with_timeout(endpoint, Duration::from_millis(1500)).await
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Error: unable to connect to Maya daemon at `{endpoint}`: {err}");
            eprintln!("Is `mayad` running?");
            std::process::exit(1);
        }
    };

    println!("Sending shutdown signal to Maya daemon at `{endpoint}`...");
    client.daemon_shutdown().await?;
    println!("Maya daemon shutdown acknowledged.");
    Ok(())
}

fn spawn_background(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("--daemon-child");
    if let Some(db) = &cli.db {
        cmd.arg("--db").arg(db);
    }
    if let Some(ep) = &cli.endpoint {
        cmd.arg("--endpoint").arg(ep);
    }
    cmd.arg("--max-concurrent")
        .arg(cli.max_concurrent.to_string());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    let child = cmd.spawn()?;
    println!("Maya daemon started in background (PID: {})", child.id());
    Ok(())
}

async fn run_daemon_server(cli: Cli, endpoint: String) -> Result<(), Box<dyn std::error::Error>> {
    // Check if another daemon is already running on the same endpoint
    if IpcClient::connect_with_timeout(&endpoint, Duration::from_millis(300))
        .await
        .is_ok()
    {
        eprintln!("Error: another Maya daemon instance is already active on `{endpoint}`");
        std::process::exit(1);
    }

    let db_path = cli.db.unwrap_or_else(default_db_path);
    let store = JobStore::open(&db_path)?;
    println!(
        "mayad: Maya Daemon v{} (PID {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    println!("Queue DB: {}", db_path.display());
    println!("IPC Endpoint: {endpoint}");

    let server = IpcServer::bind(&endpoint).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let queue = QueueManager::new(store, cli.max_concurrent);
    let scheduler_task = queue.start_scheduler(shutdown_rx.clone());

    // Wire OS Ctrl+C signal
    let sig_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            println!("\nmayad: received shutdown signal, draining active jobs...");
            let _ = sig_shutdown.send(true);
        }
    });

    let srv_queue = Arc::clone(&queue);
    let srv_shutdown = shutdown_tx.clone();
    let events = queue.events_sender();

    server
        .serve(events, shutdown_rx, move |req| {
            let q = Arc::clone(&srv_queue);
            let s_tx = srv_shutdown.clone();
            async move { q.handle_rpc(req, &s_tx).await }
        })
        .await?;

    println!("mayad: waiting for active downloads to pause/finish...");
    queue.shutdown_and_drain().await;
    let _ = scheduler_task.await;
    println!("mayad: shutdown complete.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maya_ipc::{JobOptions, JobState};
    use uuid::Uuid;

    fn temp_test_paths(tag: &str) -> (PathBuf, String) {
        let unique = Uuid::new_v4();
        let db_path = std::env::temp_dir().join(format!("maya-daemon-test-{tag}-{unique}.db"));
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\maya-daemon-test-{tag}-{unique}");
        #[cfg(unix)]
        let endpoint = std::env::temp_dir()
            .join(format!("maya-daemon-test-{tag}-{unique}.sock"))
            .to_string_lossy()
            .into_owned();
        (db_path, endpoint)
    }

    struct CleanupPaths {
        db_path: PathBuf,
        #[allow(dead_code)]
        endpoint: String,
    }

    impl Drop for CleanupPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.db_path);
            let _ = std::fs::remove_file(format!("{}-wal", self.db_path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.db_path.display()));
            #[cfg(unix)]
            let _ = std::fs::remove_file(&self.endpoint);
        }
    }

    #[test]
    fn job_store_crud_and_crash_recovery() {
        let (db_path, endpoint) = temp_test_paths("store-crud");
        let _guard = CleanupPaths {
            db_path: db_path.clone(),
            endpoint,
        };

        let store = JobStore::open(&db_path).expect("open db");

        let job1 = store
            .insert_job(
                "https://example.com/file1.iso",
                "/tmp/file1.iso",
                &JobOptions::default(),
            )
            .expect("insert job 1");
        let job2 = store
            .insert_job(
                "https://example.com/file2.iso",
                "/tmp/file2.iso",
                &JobOptions {
                    filename: Some("renamed.iso".to_string()),
                    connections: Some(8),
                    ..Default::default()
                },
            )
            .expect("insert job 2");

        let fetched = store
            .get_job(&job1.info.id)
            .expect("get job")
            .expect("found");
        assert_eq!(fetched.info.url, "https://example.com/file1.iso");
        assert_eq!(fetched.info.status, JobState::Queued);

        let list = store.list_jobs().expect("list jobs");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].info.id, job1.info.id);
        assert_eq!(list[1].info.id, job2.info.id);

        let queued = store.next_queued(10).expect("next queued");
        assert_eq!(queued.len(), 2);

        store
            .update_status(&job1.info.id, JobState::Downloading, None)
            .expect("update status downloading");
        store
            .update_progress(&job1.info.id, 1024, Some(2048))
            .expect("update progress");

        let updated1 = store
            .get_job(&job1.info.id)
            .expect("get updated")
            .expect("found");
        assert_eq!(updated1.info.status, JobState::Downloading);
        assert_eq!(updated1.info.downloaded_bytes, 1024);
        assert_eq!(updated1.info.total_bytes, Some(2048));

        // Crash recovery: downloading jobs must be recovered to queued
        let recovered = store.recover_interrupted().expect("recover interrupted");
        assert_eq!(recovered, 1);
        let recovered_job = store
            .get_job(&job1.info.id)
            .expect("get recovered")
            .expect("found");
        assert_eq!(recovered_job.info.status, JobState::Queued);
    }

    #[tokio::test]
    async fn daemon_queue_manager_rpc_flow() {
        let (db_path, endpoint) = temp_test_paths("manager-rpc");
        let _guard = CleanupPaths {
            db_path: db_path.clone(),
            endpoint: endpoint.clone(),
        };

        let store = JobStore::open(&db_path).expect("open db");
        let queue = QueueManager::new(store, 2);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let srv_queue = Arc::clone(&queue);
        let srv_shutdown = shutdown_tx.clone();
        let srv_events = queue.events_sender();
        let server = IpcServer::bind(&endpoint).await.expect("bind server");

        let server_task = tokio::spawn(async move {
            server
                .serve(srv_events, shutdown_rx, move |req| {
                    let q = Arc::clone(&srv_queue);
                    let s_tx = srv_shutdown.clone();
                    async move { q.handle_rpc(req, &s_tx).await }
                })
                .await
                .expect("serve");
        });

        let client = IpcClient::connect_with_timeout(&endpoint, Duration::from_secs(2))
            .await
            .expect("connect client");

        let daemon_status = client.daemon_status().await.expect("status");
        assert_eq!(daemon_status.total_jobs, 0);

        let added = client
            .add_job(
                "https://example.com/archive.zip",
                JobOptions {
                    filename: Some("archive.zip".to_string()),
                    connections: Some(4),
                    ..Default::default()
                },
            )
            .await
            .expect("add job");
        assert_eq!(added.status, JobState::Queued);

        let list = client.list_jobs().await.expect("list jobs");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, added.id);

        let single = client.job_status(&added.id).await.expect("job status");
        assert_eq!(single.id, added.id);

        let paused = client.pause_job(&added.id).await.expect("pause job");
        assert_eq!(paused.status, JobState::Paused);

        let resumed = client.resume_job(&added.id).await.expect("resume job");
        assert_eq!(resumed.status, JobState::Queued);

        let cancelled = client.cancel_job(&added.id).await.expect("cancel job");
        assert_eq!(cancelled.status, JobState::Cancelled);

        // Verify shutdown command
        client.daemon_shutdown().await.expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server timeout")
            .expect("clean exit");
    }

    #[test]
    fn in_memory_job_store_initialization() {
        let store = JobStore::open_in_memory().expect("open in memory db");
        let list = store.list_jobs().expect("list jobs from empty db");
        assert!(list.is_empty());
    }
}
