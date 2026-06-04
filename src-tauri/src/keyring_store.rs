//! `CredentialStore` implementation backed by the OS keychain.
//!
//! Precedence for the app-password is **env > keychain > fallback**. Only the
//! app-password lives in the keychain; the username comes from
//! `CloudConfig.username` at call sites (the design keeps the core crate free
//! of any keychain dependency — `keyring` is a `src-tauri`-only dep).

#![allow(dead_code)]
