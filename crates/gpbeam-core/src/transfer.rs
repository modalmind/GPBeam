//! Shared leaf helpers used by BOTH the filesystem offload (`orchestrator.rs`)
//! and the wired GoPro offload (`wired/offload.rs`, added in M4 Phase 4):
//!
//! * [`stream_hash_to_part`] — stream a reader into a `.part` file (append-aware
//!   for resume), hashing every on-disk byte with BLAKE3.
//! * [`commit_imported`] — after a verified file exists at its destination,
//!   record it in the ledger and (per the config's mirror mode) enqueue a cloud
//!   job. Returns the `imported` row id.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired() {
        // Compile-level smoke test: this file is reachable from the crate and
        // the test harness picks it up. Real helper tests follow in 1.2 / 1.3.
        assert_eq!(2 + 2, 4);
    }
}
