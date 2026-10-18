//! CRIU orchestration for SnapForge.
//!
//! This crate owns the *workflow* around CRIU: discovering the binary,
//! validating prerequisites, invoking `criu` with explicit argument arrays,
//! capturing output/exit/duration, staging artifacts, and cleaning up after
//! failures. CRIU itself owns the actual process-state capture and restore.
//!
//! A hard rule enforced throughout: **never report success when CRIU reports
//! failure**, and never produce a checkpoint without a CRIU-capable host.

pub mod criu;
pub mod engine;
pub mod error;

pub use criu::{Criu, CriuCommand, CriuVersion};
pub use engine::{CheckpointOutcome, Engine, RestoreOutcome};
pub use error::{EngineError, Result};
