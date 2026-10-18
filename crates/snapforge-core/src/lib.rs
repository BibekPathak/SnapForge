//! SnapForge core: shared errors, configuration, logging, and host probing.

pub mod config;
pub mod error;
pub mod host;
pub mod logging;
pub mod runner;
pub mod workload;

pub use config::{Config, DEFAULT_STORE_DIR};
pub use error::{Error, Result};
