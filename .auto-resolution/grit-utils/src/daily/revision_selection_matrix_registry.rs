//! Daily matrix rows for revision-selection hyperfine scenarios (factory step 2399).

use super::registry::{Area, ScenarioSpec};

/// Revision-selection filter scenarios (separate group from the step-7 history matrix).
pub const REVISION_SELECTION_MATRIX_SCENARIOS: &[ScenarioSpec] = &[
    ScenarioSpec {
        id: "revs/count-since@deep-history",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "Bounded rev-list count with --since on deep-history vs grit rev-walk driver",
    },
    ScenarioSpec {
        id: "revs/left-right-count@deep-history",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "Symmetric-range left/right counts on deep-history",
    },
    ScenarioSpec {
        id: "revs/count-author@deep-history",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "rev-list --count --author=. HEAD on deep-history (no commit-graph)",
    },
    ScenarioSpec {
        id: "revs/count-author@deep-history-cg",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "rev-list --count --author=. HEAD on deep-history (commit-graph written)",
    },
    ScenarioSpec {
        id: "revs/author-n50@deep-history",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "rev-list -n 50 --author=. HEAD on deep-history (no commit-graph)",
    },
    ScenarioSpec {
        id: "revs/author-n50@deep-history-cg",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "rev-list -n 50 --author=. HEAD on deep-history (commit-graph written)",
    },
    ScenarioSpec {
        id: "log/author-n50@deep-history",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "grit log --author=. -n 50 vs git log on deep-history (no commit-graph)",
    },
    ScenarioSpec {
        id: "log/author-n50@deep-history-cg",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "grit log --author=. -n 50 vs git log on deep-history (commit-graph written)",
    },
    ScenarioSpec {
        id: "log/author@git-git",
        area: Area::Revwalk,
        headline: false,
        name: None,
        description: "grit log --author=. -n 50 vs git log on git.git",
    },
];
