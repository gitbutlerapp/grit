//! Install a stderr tracing subscriber when `GRIT_TRACE` is set.

/// When `GRIT_TRACE` is `1`, `true`, or `yes`, print a compact span tree with close durations to stderr.
pub fn maybe_init_from_env() {
    let Some(raw) = std::env::var("GRIT_TRACE").ok() else {
        return;
    };
    let enabled = matches!(raw.trim(), "1" | "true" | "yes");
    if !enabled {
        return;
    }

    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_level(false)
        .with_ansi(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .compact()
        .try_init();
}
