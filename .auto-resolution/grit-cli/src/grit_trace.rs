//! Install a minimal stderr tracing subscriber when `GRIT_TRACE` is set.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::time::Instant;

use tracing_core::span::{Attributes, Id};
use tracing_core::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// When `GRIT_TRACE` is `1`, `true`, or `yes`, print a span tree with close durations to stderr.
pub fn maybe_init_from_env() {
    let Some(raw) = std::env::var("GRIT_TRACE").ok() else {
        return;
    };
    let enabled = matches!(raw.trim(), "1" | "true" | "yes");
    if !enabled {
        return;
    }

    let ansi = io::stderr().is_terminal();
    let layer = TreeLayer {
        writer: Mutex::new(io::stderr()),
        ansi,
    };
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

struct SpanStart(Instant);

struct TreeLayer {
    writer: Mutex<io::Stderr>,
    ansi: bool,
}

impl TreeLayer {
    fn depth<S>(&self, span: &tracing_subscriber::registry::SpanRef<'_, S>) -> usize
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let mut depth = 0usize;
        let mut current = span.parent();
        while let Some(parent) = current {
            depth = depth.saturating_add(1);
            current = parent.parent();
        }
        depth
    }

    fn write_line(&self, depth: usize, message: &str) {
        let Ok(mut w) = self.writer.lock() else {
            return;
        };
        let indent = depth.saturating_mul(2);
        let body = format!("{:indent$}{message}", "", indent = indent);
        if self.ansi {
            let _ = writeln!(w, "\x1b[2m{body}\x1b[0m");
        } else {
            let _ = writeln!(w, "{body}");
        }
        let _ = w.flush();
    }
}

impl<S> Layer<S> for TreeLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanStart(Instant::now()));
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let depth = self.depth(&span);
        self.write_line(depth, &format!("{}: new", span.metadata().name()));
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let depth = self.depth(&span);
        let elapsed = span
            .extensions()
            .get::<SpanStart>()
            .map(|start| start.0.elapsed())
            .unwrap_or_default();
        self.write_line(
            depth,
            &format!(
                "{}: close time.busy={} time.idle=0ns",
                span.metadata().name(),
                format_duration(elapsed)
            ),
        );
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let micros = d.as_micros();
    if micros >= 1_000 {
        format!("{:.2}ms", micros as f64 / 1_000.0)
    } else {
        format!("{}µs", micros)
    }
}
