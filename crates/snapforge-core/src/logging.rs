//! Structured logging setup.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize tracing. `json` selects structured JSON output; otherwise a
/// human-readable format is used. The `RUST_LOG` environment variable is
/// honored via [`EnvFilter`].
///
/// Logs are always written to **stderr** so that machine-readable command
/// output on stdout (`--json`) stays clean and parseable.
pub fn init(json: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);

    if json {
        registry
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .init();
    } else {
        registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}
