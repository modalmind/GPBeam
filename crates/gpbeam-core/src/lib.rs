pub mod error;
pub mod config;
pub mod gopro;
pub mod capture;
pub mod naming;
pub mod diskguard;
pub mod copy;
pub mod ledger;
pub mod scanner;
pub mod detect;
pub mod orchestrator;

pub use error::{CoreError, Result};
