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
    let (args, config) = split_config(&raw);
    let usage = "usage: gpbeam-cli [--config <path>] offload <card> <dest> | watch <dest>";

    match args.first().map(|s| s.as_str()) {
        Some("offload") => {
            let (Some(card), Some(dest)) = (args.get(1), args.get(2)) else {
                eprintln!("{usage}");
                std::process::exit(2);
            };
            let card = PathBuf::from(card);
            let dest = PathBuf::from(dest);
            if let Err(e) = run_offload_and_mirror(&card, &dest, config.as_deref(), &mut |l| {
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
                if let Err(e) = run_offload_and_mirror(&mount, &dest, config.as_deref(), &mut |l| {
                    println!("{l}")
                })
                .await
                {
                    eprintln!("error: {e}");
                }
            }
        }
        _ => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
