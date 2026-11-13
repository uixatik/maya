mod batch;
mod cli;
mod doctor;
mod progress;
mod runner;
mod tui;

use std::io::{stdin, stdout, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use maya_core::MayaError;
use maya_ipc::{default_ipc_endpoint, IpcClient, JobOptions};
use tokio::io::AsyncWriteExt;

use crate::batch::BatchOptions;
use crate::cli::{
    Cli, Commands, DaemonArgs, DaemonSubcommand, GuiArgs, QueueAddArgs, QueueArgs, QueueSubcommand,
};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Get(args) => {
            let json = args.json;
            let extract_media = args.video;
            let req = args.into_request();
            handle_download_result(runner::run_download_ext(req, false, json, extract_media).await)
        }
        Commands::Torrent(args) => {
            let json = args.json;
            let seed = args.seed;
            let req = args.into_request();
            handle_download_result(runner::run_download(req, seed, json).await)
        }
        Commands::Batch(args) => {
            let opts = BatchOptions {
                concurrency: args.concurrency,
                connections: args.connections,
                dir: args.dir,
                limit: args.limit,
                allow_private_ips: args.allow_private_ips,
                strict_https: args.strict_https,
                json: args.json,
            };
            match batch::run_batch(&args.file, opts).await {
                Ok(summary) => {
                    if summary.failed > 0 {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Tui(args) => match tui::run_tui(args.endpoint).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("tui error: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Gui(args) => handle_gui_command(args).await,
        Commands::Daemon(args) => handle_daemon_command(args).await,
        Commands::Queue(args) => handle_queue_command(args).await,
        Commands::Version => {
            print_version();
            ExitCode::SUCCESS
        }
        Commands::Doctor(args) => {
            if doctor::run_doctor(&args.dir) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn handle_daemon_command(args: DaemonArgs) -> ExitCode {
    match args.action {
        DaemonSubcommand::Start { db, endpoint } => {
            let mayad_exe = find_mayad_executable();
            let mut cmd = tokio::process::Command::new(&mayad_exe);
            cmd.arg("--background");
            if let Some(ref db_path) = db {
                cmd.arg("--db").arg(db_path);
            }
            if let Some(ref ep) = endpoint {
                cmd.arg("--endpoint").arg(ep);
            }
            match cmd.status().await {
                Ok(status) if status.success() => {
                    println!("[maya] Maya daemon started in background.");
                    ExitCode::SUCCESS
                }
                Ok(status) => {
                    eprintln!("[maya] Failed to start daemon (exit status: {status})");
                    ExitCode::FAILURE
                }
                Err(err) => {
                    eprintln!("[maya] Failed to spawn `{}`: {err}", mayad_exe.display());
                    ExitCode::FAILURE
                }
            }
        }
        DaemonSubcommand::Stop { endpoint } => {
            let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
            match IpcClient::connect_with_timeout(&ep, Duration::from_millis(1500)).await {
                Ok(client) => match client.daemon_shutdown().await {
                    Ok(_) => {
                        println!("[maya] Sent shutdown signal to Maya daemon.");
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("[maya] Failed to shutdown daemon: {err}");
                        ExitCode::FAILURE
                    }
                },
                Err(err) => {
                    eprintln!("[maya] Unable to connect to Maya daemon at `{ep}`: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        DaemonSubcommand::Status { endpoint } => {
            let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
            match IpcClient::connect_with_timeout(&ep, Duration::from_millis(1500)).await {
                Ok(client) => match client.daemon_status().await {
                    Ok(status) => {
                        println!("=== Maya Daemon Status ===");
                        println!("Version:          {}", status.version);
                        println!("Protocol Version: {}", status.protocol_version);
                        println!("PID:              {}", status.pid);
                        println!("Uptime:           {}s", status.uptime_secs);
                        println!("Active Downloads: {}", status.active_jobs);
                        println!("Queued Downloads: {}", status.queued_jobs);
                        println!("Total Jobs:       {}", status.total_jobs);
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("[maya] Failed to retrieve daemon status: {err}");
                        ExitCode::FAILURE
                    }
                },
                Err(err) => {
                    eprintln!("[maya] Unable to connect to Maya daemon at `{ep}`: {err}");
                    eprintln!("Is `mayad` running? Use `maya daemon start`.");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn handle_queue_command(args: QueueArgs) -> ExitCode {
    match args.action {
        QueueSubcommand::Add(add_args) => handle_queue_add(add_args).await,
        QueueSubcommand::List { endpoint, json } => handle_queue_list(endpoint, json).await,
        QueueSubcommand::Pause { id, endpoint } => {
            let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
            match connect_ipc(&ep).await {
                Ok(client) => match client.pause_job(&id).await {
                    Ok(job) => {
                        println!("[maya] Paused job {} (status: {})", job.id, job.status);
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("[maya] Failed to pause job: {err}");
                        ExitCode::FAILURE
                    }
                },
                Err(code) => code,
            }
        }
        QueueSubcommand::Resume { id, endpoint } => {
            let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
            match connect_ipc(&ep).await {
                Ok(client) => match client.resume_job(&id).await {
                    Ok(job) => {
                        println!("[maya] Resumed job {} (status: {})", job.id, job.status);
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("[maya] Failed to resume job: {err}");
                        ExitCode::FAILURE
                    }
                },
                Err(code) => code,
            }
        }
        QueueSubcommand::Cancel { id, endpoint } => {
            let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
            match connect_ipc(&ep).await {
                Ok(client) => match client.cancel_job(&id).await {
                    Ok(job) => {
                        println!("[maya] Cancelled job {} (status: {})", job.id, job.status);
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("[maya] Failed to cancel job: {err}");
                        ExitCode::FAILURE
                    }
                },
                Err(code) => code,
            }
        }
    }
}

async fn handle_queue_add(args: QueueAddArgs) -> ExitCode {
    let ep = args.endpoint.unwrap_or_else(default_ipc_endpoint);
    let client = match connect_ipc(&ep).await {
        Ok(c) => c,
        Err(code) => return code,
    };

    let options = JobOptions {
        output_dir: args.dir.map(|p| p.to_string_lossy().into_owned()),
        filename: args.out,
        connections: args.connections,
        max_bytes_per_sec: args.limit,
        headers: Vec::new(),
        checksum: args.verify,
        overwrite: Some(args.overwrite),
        resume: Some(true),
        allow_private_ips: Some(args.allow_private_ips),
    };

    match client.add_job(&args.url, options).await {
        Ok(job) => {
            println!(
                "[maya] Added job {} (status: {}, dest: {})",
                job.id, job.status, job.dest
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[maya] Failed to add job to queue: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn handle_queue_list(endpoint: Option<String>, json: bool) -> ExitCode {
    let ep = endpoint.unwrap_or_else(default_ipc_endpoint);
    let client = match connect_ipc(&ep).await {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_jobs().await {
        Ok(jobs) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&jobs).unwrap_or_default()
                );
            } else if jobs.is_empty() {
                println!("[maya] Queue is empty. No jobs tracked.");
            } else {
                println!(
                    "{:<36} {:<12} {:<14} {:<14} URL",
                    "ID", "STATUS", "DOWNLOADED", "TOTAL"
                );
                println!("{}", "-".repeat(95));
                for job in jobs {
                    let total_str = job
                        .total_bytes
                        .map(tui::format_bytes)
                        .unwrap_or_else(|| "--".to_string());
                    let dl_str = tui::format_bytes(job.downloaded_bytes);
                    println!(
                        "{:<36} {:<12} {:<14} {:<14} {}",
                        job.id,
                        job.status.as_str(),
                        dl_str,
                        total_str,
                        job.url
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[maya] Failed to list jobs: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn connect_ipc(endpoint: &str) -> Result<IpcClient, ExitCode> {
    IpcClient::connect_with_timeout(endpoint, Duration::from_millis(1500))
        .await
        .map_err(|err| {
            eprintln!("[maya] Unable to connect to Maya daemon at `{endpoint}`: {err}");
            eprintln!("Is `mayad` running? Use `maya daemon start`.");
            ExitCode::FAILURE
        })
}

fn find_mayad_executable() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(if cfg!(windows) { "mayad.exe" } else { "mayad" });
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(if cfg!(windows) { "mayad.exe" } else { "mayad" })
}

fn print_version() {
    println!("maya {} (v0.0.01)", env!("CARGO_PKG_VERSION"));
    println!("capabilities: HTTP/1.1 + H2, rustls, torrent=enabled, media=yt-dlp+ffmpeg, tui=ratatui, ipc=jsonrpc");
}

fn handle_download_result(result: maya_core::Result<std::path::PathBuf>) -> ExitCode {
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(MayaError::Cancelled) => {
            eprintln!("error: download cancelled");
            ExitCode::from(130)
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn handle_gui_command(args: GuiArgs) -> ExitCode {
    let is_install_action = args.install
        || args
            .action
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("install"))
            .unwrap_or(false);

    if let Some(gui_exe) = find_gui_executable() {
        if !is_install_action {
            match std::process::Command::new(&gui_exe).spawn() {
                Ok(_) => {
                    println!("[maya] Launched Maya Desktop GUI.");
                    return ExitCode::SUCCESS;
                }
                Err(err) => {
                    eprintln!(
                        "[maya] Failed to launch Maya Desktop GUI (`{}`): {err}",
                        gui_exe.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let is_interactive = stdin().is_terminal();
    if !is_interactive && !is_install_action {
        eprintln!("[?] Maya Desktop GUI is not currently installed.");
        eprintln!("Run `maya gui --install` to download and launch the installer.");
        return ExitCode::FAILURE;
    }

    let confirmed = if is_interactive {
        println!("[?] Maya Desktop GUI is not currently installed.");
        print!("    Download and launch the Desktop GUI installer now? [Y/n]: ");
        let _ = stdout().flush();

        let mut input = String::new();
        if stdin().read_line(&mut input).is_err() {
            eprintln!("[maya] Failed to read confirmation.");
            return ExitCode::FAILURE;
        }
        let trimmed = input.trim();
        trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("y")
            || trimmed.eq_ignore_ascii_case("yes")
    } else {
        true
    };

    if !confirmed {
        println!("[maya] Installation cancelled.");
        return ExitCode::SUCCESS;
    }

    let temp_dir = std::env::temp_dir();
    let installer_path = temp_dir.join("Maya_0.0.1_x64-setup.exe");
    let installer_url =
        "https://github.com/uixatik/maya/releases/download/v0.0.1/Maya_0.0.1_x64-setup.exe";

    println!("[maya] Downloading Maya Desktop GUI installer from `{installer_url}`...");

    let client = reqwest::Client::builder()
        .user_agent("maya-cli/0.0.1")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let response = match client.get(installer_url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("[maya] Failed to connect to download URL `{installer_url}`: {err}");
            return ExitCode::FAILURE;
        }
    };

    if !response.status().is_success() {
        eprintln!(
            "[maya] Failed to download GUI installer: HTTP status {}",
            response.status()
        );
        return ExitCode::FAILURE;
    }

    let total_bytes = response.content_length();
    let mut file = match tokio::fs::File::create(&installer_path).await {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "[maya] Failed to create destination file `{}`: {err}",
                installer_path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_reported: u64 = 0;

    while let Some(chunk_res) = stream.next().await {
        let chunk = match chunk_res {
            Ok(c) => c,
            Err(err) => {
                eprintln!("\n[maya] Download stream error: {err}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(err) = file.write_all(&chunk).await {
            eprintln!("\n[maya] Failed to write installer data: {err}");
            return ExitCode::FAILURE;
        }
        downloaded += chunk.len() as u64;

        if downloaded.saturating_sub(last_reported) >= 256 * 1024 || Some(downloaded) == total_bytes
        {
            last_reported = downloaded;
            if let Some(total) = total_bytes {
                let pct = if total > 0 {
                    (downloaded as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                print!(
                    "\r[maya] Downloading: {} / {} ({:.1}%)",
                    tui::format_bytes(downloaded),
                    tui::format_bytes(total),
                    pct
                );
            } else {
                print!("\r[maya] Downloading: {}", tui::format_bytes(downloaded));
            }
            let _ = stdout().flush();
        }
    }

    if let Err(err) = file.flush().await {
        eprintln!("\n[maya] Failed to flush installer file: {err}");
        return ExitCode::FAILURE;
    }
    println!();

    match std::process::Command::new(&installer_path).spawn() {
        Ok(_) => {
            println!("[+] Maya Desktop GUI installer launched.");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!(
                "[maya] Failed to launch installer `{}`: {err}",
                installer_path.display()
            );
            ExitCode::FAILURE
        }
    }
}

fn find_gui_executable() -> Option<PathBuf> {
    let exe_names: &[&str] = if cfg!(windows) {
        &["Maya.exe", "maya-gui.exe"]
    } else {
        &["Maya", "maya-gui"]
    };

    // 1) %LOCALAPPDATA%\Programs\Maya\Maya.exe
    let mut localappdata_candidates = Vec::new();
    if let Some(val) = std::env::var_os("LOCALAPPDATA") {
        localappdata_candidates.push(PathBuf::from(val));
    }
    if let Some(val) = dirs::data_local_dir() {
        if !localappdata_candidates.contains(&val) {
            localappdata_candidates.push(val);
        }
    }
    for base in &localappdata_candidates {
        for name in exe_names {
            // Check %LOCALAPPDATA%\Programs\Maya\<name>
            let p = base.join("Programs").join("Maya").join(name);
            if p.is_file() {
                return Some(p);
            }
            // Check %LOCALAPPDATA%\Maya\<name>
            let p_alt = base.join("Maya").join(name);
            if p_alt.is_file() {
                return Some(p_alt);
            }
        }
    }

    // 2) Current executable directory, and its parent directory (e.g. `../Maya.exe`)
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            for name in exe_names {
                let p = parent.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
            if let Some(grandparent) = parent.parent() {
                for name in exe_names {
                    let p = grandparent.join(name);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // 3) System PATH
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in exe_names {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    None
}
