use gpbeam_cli::run_offload_and_mirror;
use std::path::PathBuf;

const USAGE: &str = "usage: gpbeam-cli [--version] [--config <path>] [--delete-after-verify] [--auto-eject] offload <card> <dest> | watch <dest> | mirror <dest> | mirror-status <dest> | retry-cloud <dest>";

/// Print a usage-error message + the usage line, then exit 2.
fn usage_error(msg: &str) -> ! {
    eprintln!("error: {msg}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (after_config, config) = match gpbeam_cli::split_config(&raw) {
        Ok(v) => v,
        Err(msg) => usage_error(&msg),
    };
    let (args, flags) = match gpbeam_cli::parse_safety_flags(&after_config) {
        Ok(v) => v,
        Err(msg) => usage_error(&msg),
    };
    match args.first().map(|s| s.as_str()) {
        Some("--version") | Some("-V") => {
            println!("{}", gpbeam_cli::version_line());
        }
        Some("offload") => {
            let (Some(card), Some(dest)) = (args.get(1), args.get(2)) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let card = PathBuf::from(card);
            let dest = PathBuf::from(dest);
            // Exit 0 ONLY for fully-clean runs: a per-file copy/upload failure is
            // tallied (Ok(n)), not an Err, but scripts/cron still need a non-zero
            // exit to detect it.
            match run_offload_and_mirror(&card, &dest, config.as_deref(), &flags, &mut |l| {
                println!("{l}")
            })
            .await
            {
                Ok(0) => {}
                Ok(failed) => {
                    eprintln!("error: {failed} file(s) failed");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("watch") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let dest = PathBuf::from(dest);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(gpbeam_core::detect::poll_removable_mounts(tx));
            println!("[watch] waiting for a GoPro card... (Ctrl-C to quit)");
            while let Some(mount) = rx.recv().await {
                println!("[watch] volume mounted: {}", mount.display());
                let dest = dest.clone();
                let config = config.clone();
                // Per-card failures are reported but never exit: watch keeps
                // looping for the next card.
                match run_offload_and_mirror(&mount, &dest, config.as_deref(), &flags, &mut |l| {
                    println!("{l}")
                })
                .await
                {
                    Ok(0) => {}
                    Ok(failed) => eprintln!("error: {failed} file(s) failed"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        Some("mirror") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let dest = PathBuf::from(dest);
            // As with offload: terminal upload failures return Ok(n) — exit 1 so
            // a scripted `mirror` can detect that jobs permanently failed.
            match gpbeam_cli::run_mirror(&dest, config.as_deref(), &flags, &mut |l| println!("{l}"))
                .await
            {
                Ok(0) => {}
                Ok(failed) => {
                    eprintln!("error: {failed} file(s) failed");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("mirror-status") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            match gpbeam_cli::mirror_status_lines(&PathBuf::from(dest)) {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("retry-cloud") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            match gpbeam_cli::retry_cloud(&PathBuf::from(dest)) {
                Ok(n) => println!("[retry] re-queued {n} failed job(s)"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}
