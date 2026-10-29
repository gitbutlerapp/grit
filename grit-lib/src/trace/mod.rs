//! Optional operation-level tracing spans (Cargo feature `tracing`).
//!
//! When the `tracing` feature is disabled, [`trace_span!`] expands to nothing.
//! Embedders install a `tracing` subscriber; `grit-lib` does not set global tracing state.

/// Emit a tracing span for the current scope when feature `tracing` is enabled.
///
/// The span `name` must be a string literal (required by the `tracing` crate).
macro_rules! trace_span {
    ($name:literal) => {
        #[cfg(feature = "tracing")]
        let _grit_trace_span_guard = tracing::info_span!($name).entered();
        #[cfg(not(feature = "tracing"))]
        let _grit_trace_span_guard = ();
    };
}

pub(crate) use trace_span;

#[cfg(all(test, feature = "tracing"))]
mod tests {
    use crate::environment::RepositoryOptions;
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::repo::{InitOptions, Repository};
    use crate::rev_list::RevListOptions;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::Layer;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SpanEnter {
        name: &'static str,
        parent: Option<&'static str>,
    }

    struct SpanHierarchyCollector(Arc<Mutex<Vec<SpanEnter>>>);

    impl<S> Layer<S> for SpanHierarchyCollector
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
            let Some(span) = ctx.span(id) else {
                return;
            };
            let name = span.metadata().name();
            let parent = span
                .parent()
                .map(|parent_span| parent_span.metadata().name());
            self.0.lock().unwrap().push(SpanEnter { name, parent });
        }
    }

    #[test]
    fn trace_spans_emitted() {
        let events: Arc<Mutex<Vec<SpanEnter>>> = Arc::new(Mutex::new(Vec::new()));
        let collector = SpanHierarchyCollector(Arc::clone(&events));
        let subscriber = tracing_subscriber::registry().with(collector);

        tracing::subscriber::with_default(subscriber, || {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
            let ident = "T <t@example.com> 1 +0000".to_string();
            let sig = ident.parse().unwrap();
            let mut req = crate::porcelain::commit::CommitRequest::new("trace test");
            req.author = Some(sig.clone());
            req.committer = Some(sig);
            req.allow_empty = true;
            let _ = crate::porcelain::commit::create_commit(
                &repo,
                &req,
                &mut NullProgress,
                &NeverCancel,
            )
            .unwrap();

            let opened =
                Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
            let _ = status(
                &opened,
                &StatusOptions::default(),
                &mut NullProgress,
                &NeverCancel,
            )
            .unwrap();
            let _ = opened
                .rev_walk()
                .walk("HEAD", &RevListOptions::default())
                .unwrap();
        });

        let seen = events.lock().unwrap();
        assert!(
            seen.iter().any(|e| e.name == "grit.repository.open"),
            "expected grit.repository.open in {:?}",
            *seen
        );
        assert!(
            seen.iter().any(|e| e.name == "grit.status.scan"),
            "expected grit.status.scan in {:?}",
            *seen
        );
        assert!(
            seen.iter()
                .any(|e| { e.name == "grit.index.read" && e.parent == Some("grit.status.scan") }),
            "expected grit.index.read nested under grit.status.scan in {:?}",
            *seen
        );
    }
}
