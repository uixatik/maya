//! Maya Native Messaging Host (`maya-nmh`) binary.

use maya_nmh::{
    generate_chrome_manifest, generate_firefox_manifest, handle_message, install_manifests,
    read_message, resolve_host_binary, uninstall_manifests, write_message,
};
use std::env;
use std::path::PathBuf;

fn print_help() {
    eprintln!(
        r#"Maya Native Messaging Host (maya-nmh) v{}

USAGE:
    maya-nmh [OPTIONS]

OPTIONS:
    --install-manifest, -i       Install native messaging manifests and register with
                                 Windows Registry (Chrome, Edge, Mozilla) or Unix config dirs.
    --uninstall-manifest         Remove installed native messaging manifests and registry keys.
    --print-manifest             Print Chrome and Firefox manifest JSONs to stdout.
    --extension-id <ID>          Allow specific browser extension ID in manifest.
    --host-path <PATH>           Specify explicit path to maya-nmh executable in manifest.
    -h, --help                   Print this help message.
    -V, --version                Print version information.

When launched without options (or invoked by Chrome/Edge/Firefox), maya-nmh communicates
over standard input/output using 32-bit length-prefixed JSON messages.
"#,
        env!("CARGO_PKG_VERSION")
    );
}

fn print_version() {
    eprintln!("maya-nmh {}", env!("CARGO_PKG_VERSION"));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    // Check CLI flags
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    if args.iter().any(|a| a == "-V" || a == "--version") {
        print_version();
        return Ok(());
    }

    if args.iter().any(|a| a == "--print-manifest") {
        let binary = resolve_host_binary(None);
        let chrome = generate_chrome_manifest(&binary, &[]);
        let firefox = generate_firefox_manifest(&binary, &[]);

        println!("=== Chromium (Chrome / Edge) Manifest ===");
        println!("{}", serde_json::to_string_pretty(&chrome)?);
        println!("\n=== Mozilla Firefox Manifest ===");
        println!("{}", serde_json::to_string_pretty(&firefox)?);
        return Ok(());
    }

    if args.iter().any(|a| a == "--uninstall-manifest") {
        uninstall_manifests()?;
        eprintln!("[maya-nmh] Native messaging manifests and registry entries uninstalled.");
        return Ok(());
    }

    if args.iter().any(|a| a == "--install-manifest" || a == "-i") {
        let mut custom_origins = Vec::new();
        let mut custom_binary: Option<PathBuf> = None;

        let mut i = 1;
        while i < args.len() {
            if (args[i] == "--extension-id" || args[i] == "-e") && i + 1 < args.len() {
                let id = &args[i + 1];
                let origin = if id.starts_with("chrome-extension://") {
                    id.clone()
                } else {
                    format!("chrome-extension://{id}/")
                };
                custom_origins.push(origin);
                i += 1;
            } else if args[i] == "--host-path" && i + 1 < args.len() {
                custom_binary = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            i += 1;
        }

        let report = install_manifests(custom_binary.as_deref(), &custom_origins)?;

        eprintln!("[maya-nmh] Manifest installation successful!");
        eprintln!("  Host binary: {}", report.host_binary.display());
        eprintln!("  Manifest files created:");
        for file in &report.manifest_files {
            eprintln!("    - {}", file.display());
        }
        if !report.registry_keys.is_empty() {
            eprintln!("  Windows Registry keys registered:");
            for key in &report.registry_keys {
                eprintln!("    - {key}");
            }
        }
        return Ok(());
    }

    // Standard Native Messaging Host mode: read stdin, write stdout.
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    loop {
        match read_message(&mut stdin).await {
            Ok(Some(msg)) => {
                let response = handle_message(&msg).await;
                if let Err(e) = write_message(&mut stdout, &response).await {
                    eprintln!("[maya-nmh] Error writing response: {e}");
                    break;
                }
            }
            Ok(None) => {
                // Stdin reached EOF: browser closed connection cleanly.
                break;
            }
            Err(e) => {
                eprintln!("[maya-nmh] Protocol framing error: {e}");
                // Return an error response if possible
                let err_resp = serde_json::json!({
                    "status": "error",
                    "success": false,
                    "error": format!("Protocol framing error: {e}")
                });
                let _ = write_message(&mut stdout, &err_resp).await;
                break;
            }
        }
    }

    Ok(())
}
