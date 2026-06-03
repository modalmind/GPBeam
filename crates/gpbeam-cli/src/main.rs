use gpbeam_core::config::Config;
use gpbeam_core::ledger::Ledger;
use gpbeam_core::orchestrator::{run_offload, RunEvent};
use std::path::PathBuf;

fn ledger_path() -> PathBuf {
    // Simple M1 location; M3 will move this under the OS app-data dir.
    std::env::temp_dir().join("gpbeam-ledger.sqlite")
}

fn print_event(e: RunEvent) {
    match e {
        RunEvent::NotGoPro(p) => eprintln!("[skip] not a GoPro card: {}", p.display()),
        RunEvent::CardDetected { model, serial } =>
            println!("[detect] {} (serial {})", model.unwrap_or("GoPro".into()),
                     serial.unwrap_or("unknown".into())),
        RunEvent::Scanned { new_files, total_bytes } =>
            println!("[scan] {new_files} new file(s), {total_bytes} bytes"),
        RunEvent::InsufficientSpace { need, have } =>
            eprintln!("[error] not enough space: need {need}, have {have}"),
        RunEvent::Copying { file, index, total } => println!("[copy {index}/{total}] {file}"),
        RunEvent::Progress { .. } => {}
        RunEvent::Verified { file } => println!("  [ok] {file}"),
        RunEvent::Skipped { file } => println!("  [skip] {file}"),
        RunEvent::Failed { file, error } => eprintln!("  [FAIL] {file}: {error}"),
        RunEvent::RunComplete { copied, skipped, failed, bytes } =>
            println!("[done] copied {copied}, skipped {skipped}, failed {failed}, {bytes} bytes"),
    }
}

fn offload_once(card: PathBuf, dest: PathBuf) -> Result<(), String> {
    let cfg = Config::new(dest);
    let mut ledger = Ledger::open(&ledger_path()).map_err(|e| e.to_string())?;
    run_offload(&card, &cfg, &mut ledger, &mut print_event).map_err(|e| e.to_string())?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: gpbeam-cli offload <card> <dest> | gpbeam-cli watch <dest>";
    match args.get(1).map(|s| s.as_str()) {
        Some("offload") => {
            let (Some(card), Some(dest)) = (args.get(2), args.get(3)) else {
                eprintln!("{usage}"); std::process::exit(2);
            };
            if let Err(e) = offload_once(card.into(), dest.into()) {
                eprintln!("error: {e}"); std::process::exit(1);
            }
        }
        Some("watch") => {
            let Some(dest) = args.get(2) else { eprintln!("{usage}"); std::process::exit(2); };
            let dest = PathBuf::from(dest);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(gpbeam_core::detect::poll_removable_mounts(tx));
            println!("[watch] waiting for a GoPro card... (Ctrl-C to quit)");
            while let Some(mount) = rx.recv().await {
                println!("[watch] volume mounted: {}", mount.display());
                let dest = dest.clone();
                if let Err(e) = offload_once(mount, dest) { eprintln!("error: {e}"); }
            }
        }
        _ => { eprintln!("{usage}"); std::process::exit(2); }
    }
}
