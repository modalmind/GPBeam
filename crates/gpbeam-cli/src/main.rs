use gpbeam_cli::run_offload_and_mirror;
use std::path::PathBuf;

/// Pull `--config <path>` out of argv, returning (remaining positional args, config).
fn split_config(args: &[String]) -> (Vec<String>, Option<PathBuf>) {
    let mut positional = Vec::new();
    let mut config = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" {
            if let Some(p) = args.get(i + 1) {
                config = Some(PathBuf::from(p));
                i += 2;
                continue;
            }
        }
        positional.push(args[i].clone());
        i += 1;
    }
    (positional, config)
}

#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (after_config, config) = split_config(&raw);
    let (args, flags) = gpbeam_cli::parse_safety_flags(&after_config);
    let usage = "usage: gpbeam-cli [--version] [--config <path>] [--delete-after-verify] [--auto-eject] offload <card> <dest> | watch <dest> | mirror <dest> | mirror-status <dest> | retry-cloud <dest>";

    match args.first().map(|s| s.as_str()) {
        Some("--version") | Some("-V") => {
            println!("{}", gpbeam_cli::version_line());
        }
        Some("offload") => {
            let (Some(card), Some(dest)) = (args.get(1), args.get(2)) else {
                eprintln!("{usage}");
                std::process::exit(2);
            };
            let card = PathBuf::from(card);
            let dest = PathBuf::from(dest);
            if let Err(e) = run_offload_and_mirror(&card, &dest, config.as_deref(), &flags, &mut |l| {
                println!("{l}")
            })
            .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some("watch") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{usage}");
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
                if let Err(e) =
                    run_offload_and_mirror(&mount, &dest, config.as_deref(), &flags, &mut |l| {
                        println!("{l}")
                    })
                    .await
                {
                    eprintln!("error: {e}");
                }
            }
        }
        Some("mirror") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{usage}");
                std::process::exit(2);
            };
            let dest = PathBuf::from(dest);
            if let Err(e) =
                gpbeam_cli::run_mirror(&dest, config.as_deref(), &flags, &mut |l| println!("{l}"))
                    .await
            {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some("mirror-status") => {
            let Some(dest) = args.get(1) else {
                eprintln!("{usage}");
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
                eprintln!("{usage}");
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
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
