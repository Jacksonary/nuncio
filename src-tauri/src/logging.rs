//! File logging setup.
//!
//! Logs go to `Paths::logs_dir()` (never to stdout in release builds), rotate
//! daily and keep the last seven files. `RUST_LOG` overrides the default level.

use std::path::Path;

use nuncio_core::Result;
use tracing_subscriber::EnvFilter;

/// Third-party crates that are either extremely chatty or can print fragments
/// of message content at debug level (html5ever logs parser tokens, which
/// include body text). Keep them quiet in every build unless the operator
/// explicitly overrides `RUST_LOG`.
const QUIET_DIRECTIVES: &str =
    "html5ever=error,markup5ever=error,ammonia=warn,selectors=warn,cssparser=warn,\
     hyper=info,rustls=info,h2=info,reqwest=info";

fn default_filter() -> String {
    let base = if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    };
    format!("{base},{QUIET_DIRECTIVES}")
}

/// Install the global tracing subscriber. Calling it twice is harmless: the
/// second call is ignored by `tracing`.
pub fn init(log_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(log_dir)
        .map_err(|error| nuncio_core::AppError::io(format!("cannot create log dir: {error}")))?;

    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("nuncio.log")
        .max_log_files(7)
        .build(log_dir)
        .map_err(|error| nuncio_core::AppError::io(format!("cannot open log file: {error}")))?;

    // An explicit RUST_LOG is authoritative; otherwise use our level plus the
    // privacy directives above.
    let filter = if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter()))
    } else {
        EnvFilter::new(default_filter())
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(appender);
    // `try_init` returns an error when a subscriber is already installed; that
    // only happens in tests or when the library entry point runs twice.
    let _ = subscriber.try_init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::default_filter;

    #[test]
    fn default_filter_keeps_noisy_parsers_quiet() {
        let filter = default_filter();
        for directive in [
            "html5ever=error",
            "markup5ever=error",
            "ammonia=warn",
            "selectors=warn",
            "cssparser=warn",
            "hyper=info",
            "rustls=info",
            "h2=info",
            "reqwest=info",
        ] {
            assert!(
                filter.contains(directive),
                "missing {directive} in {filter}"
            );
        }
        assert!(filter.starts_with("debug") || filter.starts_with("info"));
    }
}
