//! Structured logging setup.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize tracing. `json` selects structured JSON output; otherwise a
/// human-readable format is used. The `RUST_LOG` environment variable is
/// honored via [`EnvFilter`].
pub fn init(json: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);

    if json {
        registry.with(fmt::layer().json()).init();
    } else {
        registry.with(fmt::layer()).init();
    }
}
