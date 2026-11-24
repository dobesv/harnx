//! Keep the shared log configuration while retaining tracing's structured fields.

use anyhow::Result;
use harnx_core::logging::{LogFormat, LogSettings, LogSink};
use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, Layer};

pub(crate) fn init() -> Result<()> {
    harnx_runtime::bootstrap::setup_logger(LogSink::Stderr)?;
    let settings = harnx_core::logging::settings(LogSink::Stderr);
    let subscriber = subscriber(&settings, std::io::stderr)?;
    // setup_logger owns the log facade. Don't install LogTracer (fmt().init()),
    // which would try to claim it again. Tracing events go to this subscriber;
    // log records still use the shared logger, with the same settings.
    tracing::subscriber::set_global_default(subscriber)?;
    warn_legacy_rust_log();
    Ok(())
}

fn warn_legacy_rust_log() {
    if ["LEVEL", "FILTER", "FORMAT", "PATH", "MAX_BYTES"]
        .iter()
        .any(|suffix| std::env::var_os(format!("HARNX_LOG_{suffix}")).is_some())
    {
        return;
    }
    let Ok(value) = std::env::var("RUST_LOG") else {
        return;
    };
    if !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "off" | "error" | "warn" | "info" | "debug" | "trace"
    ) {
        // Don't echo directive values, which may contain sensitive data.
        tracing::warn!("Ignoring non-bare-level RUST_LOG; use HARNX_LOG_LEVEL and HARNX_LOG_FILTER (target prefix), with HARNX_LOG_FORMAT=text or json");
    }
}

fn subscriber<W>(
    settings: &LogSettings,
    writer: W,
) -> Result<impl tracing::Subscriber + Send + Sync>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let level: tracing::level_filters::LevelFilter = settings.level.as_str().parse()?;
    let prefix = settings.filter.clone();
    let filter = filter_fn(move |metadata| {
        level >= *metadata.level() && metadata.target().starts_with(&prefix)
    });
    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);
    let fmt = match settings.format {
        LogFormat::Text => fmt.boxed(),
        LogFormat::Json => fmt.json().boxed(),
    };
    Ok(tracing_subscriber::registry().with(filter).with(fmt))
}

#[cfg(test)]
mod tests;
