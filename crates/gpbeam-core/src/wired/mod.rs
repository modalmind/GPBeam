//! Wired (USB) GoPro ingest over the Open GoPro HTTP API.
//!
//! `client` talks to the camera; `detect` finds it on the IP-over-USB interface
//! (Phase 3); `offload` drives the verify→ledger→cloud pipeline (Phase 4).

pub mod client;
pub mod detect;
pub mod offload;
