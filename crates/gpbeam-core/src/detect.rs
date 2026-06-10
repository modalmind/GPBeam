use std::collections::HashSet;
use std::path::PathBuf;
use sysinfo::Disks;
use tokio::sync::mpsc::UnboundedSender;

/// Pure diff: mounts present in `now` but not in `before`.
pub fn newly_appeared(before: &HashSet<PathBuf>, now: &HashSet<PathBuf>) -> Vec<PathBuf> {
    now.difference(before).cloned().collect()
}

fn snapshot(disks: &Disks) -> HashSet<PathBuf> {
    disks
        .list()
        .iter()
        .filter(|d| d.is_removable())
        .map(|d| d.mount_point().to_path_buf())
        .collect()
}

/// Baseline mount detector: polls removable volumes every ~1.5s and sends each
/// newly-appeared mount path on `tx`. Zero native FFI; identical on macOS+Windows.
/// (Native DiskArbitration / WM_DEVICECHANGE paths are deferred to a later milestone.)
pub async fn poll_removable_mounts(tx: UnboundedSender<PathBuf>) {
    let mut disks = Disks::new_with_refreshed_list();
    let mut seen = snapshot(&disks);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(1500));
    loop {
        ticker.tick().await;
        disks.refresh(true);
        let now = snapshot(&disks);
        for mp in newly_appeared(&seen, &now) {
            if tx.send(mp).is_err() {
                return;
            } // receiver dropped -> stop
        }
        seen = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_newly_appeared_mounts() {
        let before: HashSet<PathBuf> = ["/Volumes/A".into()].into_iter().collect();
        let now: HashSet<PathBuf> = ["/Volumes/A".into(), "/Volumes/GOPRO".into()]
            .into_iter()
            .collect();
        let mut appeared = newly_appeared(&before, &now);
        appeared.sort();
        assert_eq!(appeared, vec![PathBuf::from("/Volumes/GOPRO")]);
    }

    #[test]
    fn no_change_yields_nothing() {
        let s: HashSet<PathBuf> = ["/Volumes/A".into()].into_iter().collect();
        assert!(newly_appeared(&s, &s).is_empty());
    }
}
