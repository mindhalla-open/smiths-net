//! Tracing subscriber setup with a hot-reloadable filter.

use anyhow::Context as _;
use smiths_core::LogFormat;
use tracing_subscriber::{EnvFilter, fmt, prelude::*, reload};

/// Type-erased handle the `observability.log_level` read-through
/// adapter uses to swap the live `EnvFilter` without naming the
/// subscriber's concrete `Layered<…>` type. `Ok()` on a successful
/// reload; `Err(msg)` carries a human string for the log line.
pub(crate) type LogReloader = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Install the global subscriber. In MCP stdio mode stdout is the
/// JSON-RPC wire, so logs go to stderr regardless of `format`.
pub(crate) fn init_tracing(
    level: &str,
    format: LogFormat,
    mcp_stdio: bool,
) -> anyhow::Result<LogReloader> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .or_else(|_| EnvFilter::try_new("info"))
        .context("constructing tracing EnvFilter")?;

    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter_layer);
    if mcp_stdio {
        registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .init();
    } else {
        match format {
            LogFormat::Json => registry.with(fmt::layer().json()).init(),
            LogFormat::Pretty => registry.with(fmt::layer()).init(),
        }
    }

    // `EnvFilter::try_new` rejects unparseable directives before the
    // swap so a bad config doesn't take out logging.
    let reloader: LogReloader = Box::new(move |new_level: &str| -> Result<(), String> {
        let new_filter = EnvFilter::try_new(new_level).map_err(|e| e.to_string())?;
        filter_handle.reload(new_filter).map_err(|e| e.to_string())
    });
    Ok(reloader)
}
