//! Optional operation-level tracing spans (Cargo feature `tracing`).
//!
//! When the `tracing` feature is disabled, [`trace_span!`] expands to nothing.
//! Embedders install a `tracing` subscriber; `grit-lib` does not set global tracing state.

pub mod names;

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
    use super::names;
    use crate::environment::RepositoryOptions;
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::NullProgress;
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
        let _guard = tracing_subscriber::registry().with(collector).set_default();

        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
        let req = crate::porcelain::commit::CommitRequest {
            message: "trace test".into(),
            author: "T <t@example.com>".into(),
            committer: "T <t@example.com>".into(),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        };
        let _ = crate::porcelain::commit::create_commit(&repo, &req, &mut NullProgress).unwrap();

        let opened = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
        let _ = status(&opened, &StatusOptions::default(), &mut NullProgress).unwrap();
        let _ = opened
            .rev_walk()
            .walk("HEAD", &RevListOptions::default())
            .unwrap();

        let seen = events.lock().unwrap();
        assert!(
            seen.iter().any(|e| e.name == names::REPOSITORY_OPEN),
            "expected {} in {:?}",
            names::REPOSITORY_OPEN,
            *seen
        );
        assert!(
            seen.iter().any(|e| e.name == names::STATUS_SCAN),
            "expected {} in {:?}",
            names::STATUS_SCAN,
            *seen
        );
        assert!(
            seen.iter().any(|e| e.name == names::REVWALK_SETUP),
            "expected {} in {:?}",
            names::REVWALK_SETUP,
            *seen
        );
        assert!(
            seen.iter()
                .any(|e| { e.name == names::INDEX_READ && e.parent == Some(names::STATUS_SCAN) }),
            "expected {} nested under {} in {:?}",
            names::INDEX_READ,
            names::STATUS_SCAN,
            *seen
        );
    }
}
