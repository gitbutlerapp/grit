//! Scenario registry: repo shapes, functional areas, and headline scenario descriptors.

use serde::{Deserialize, Serialize};

/// Cached fixture identity for the daily matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepoShape {
    Small,
    #[serde(rename = "synth-10k")]
    Synth10k,
    #[serde(rename = "synth-100k")]
    Synth100k,
    #[serde(rename = "git-git")]
    GitGit,
    #[serde(rename = "many-refs")]
    ManyRefs,
    #[serde(rename = "many-refs-loose")]
    ManyRefsLoose,
    #[serde(rename = "big-blobs")]
    BigBlobs,
    #[serde(rename = "deep-tree")]
    DeepTree,
}

impl RepoShape {
    /// Stable string id used in scenario ids (`@<repo>` suffix).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Synth10k => "synth-10k",
            Self::Synth100k => "synth-100k",
            Self::GitGit => "git-git",
            Self::ManyRefs => "many-refs",
            Self::ManyRefsLoose => "many-refs-loose",
            Self::BigBlobs => "big-blobs",
            Self::DeepTree => "deep-tree",
        }
    }
}

/// Functional area label for matrix grouping and `--area` filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Area {
    Odb,
    Packs,
    Refs,
    Index,
    Worktree,
    Diff,
    Revwalk,
    Blame,
    Branching,
    Remotes,
    Maintenance,
    Config,
    Ignore,
    Hooks,
    Notes,
    Bundles,
    Worktrees,
}

/// Static descriptor for one benchmark scenario (registry entry, not a timed result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioSpec {
    pub id: &'static str,
    pub area: Area,
    pub headline: bool,
    pub name: Option<&'static str>,
    pub description: &'static str,
}

/// Headline scenario on the cooking page (roadmap item 2347).
pub type HeadlineScenario = ScenarioSpec;

/// Headline scenarios: ids and short names fixed by the performance census roadmap.
pub const HEADLINE_SCENARIOS: [HeadlineScenario; 22] = [
    ScenarioSpec {
        id: "status/clean@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("status-clean"),
        description: "Clean worktree status vs git status -s",
    },
    ScenarioSpec {
        id: "status/dirty@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("status-dirty"),
        description: "Dirty worktree status vs git status -s",
    },
    ScenarioSpec {
        id: "add/modified@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("add"),
        description: "Stage modified files vs git add -A",
    },
    ScenarioSpec {
        id: "commit/small@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("commit"),
        description: "Commit staged changes vs git commit",
    },
    ScenarioSpec {
        id: "diff/worktree@synth-10k",
        area: Area::Diff,
        headline: true,
        name: Some("diff"),
        description: "Worktree diff vs git diff",
    },
    ScenarioSpec {
        id: "log/oneline-100@git-git",
        area: Area::Revwalk,
        headline: true,
        name: Some("log"),
        description: "Last 100 commits one line each vs git log --oneline",
    },
    ScenarioSpec {
        id: "log/json-100@git-git",
        area: Area::Revwalk,
        headline: true,
        name: Some("log"),
        description: "Last 100 commits as JSON vs git log with a verbose --format",
    },
    ScenarioSpec {
        id: "show/head@git-git",
        area: Area::Revwalk,
        headline: true,
        name: Some("show"),
        description: "Show HEAD commit vs git show",
    },
    ScenarioSpec {
        id: "blame/file@git-git",
        area: Area::Blame,
        headline: true,
        name: Some("blame"),
        description: "Blame one tracked file vs git blame",
    },
    ScenarioSpec {
        id: "switch/branch@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("switch"),
        description: "Switch branch vs git switch",
    },
    ScenarioSpec {
        id: "branch/list@many-refs",
        area: Area::Branching,
        headline: true,
        name: Some("branch"),
        description: "List branches vs git branch",
    },
    ScenarioSpec {
        id: "branch/list-json@many-refs",
        area: Area::Branching,
        headline: true,
        name: Some("branch"),
        description: "List branches with JSON vs git branch -vv",
    },
    ScenarioSpec {
        id: "branch/merged@many-refs",
        area: Area::Branching,
        headline: true,
        name: Some("branch"),
        description: "Branch --merged vs git branch --merged",
    },
    ScenarioSpec {
        id: "merge/clean@synth-10k",
        area: Area::Branching,
        headline: true,
        name: Some("merge"),
        description: "Merge branch vs git merge",
    },
    ScenarioSpec {
        id: "pick/one@synth-10k",
        area: Area::Branching,
        headline: true,
        name: Some("pick"),
        description: "Cherry-pick one commit vs git cherry-pick",
    },
    ScenarioSpec {
        id: "stash/push-pop@synth-10k",
        area: Area::Worktree,
        headline: true,
        name: Some("stash"),
        description: "Stash push and pop vs git stash",
    },
    ScenarioSpec {
        id: "fetch/incremental@git-git",
        area: Area::Remotes,
        headline: true,
        name: Some("fetch"),
        description: "Incremental fetch vs git fetch",
    },
    ScenarioSpec {
        id: "pull/fast-forward@git-git",
        area: Area::Remotes,
        headline: true,
        name: Some("pull"),
        description: "Fast-forward pull vs git pull --ff-only",
    },
    ScenarioSpec {
        id: "pull/rebase@synth-10k",
        area: Area::Remotes,
        headline: true,
        name: Some("pull"),
        description: "Rebase pull (3 local onto 5 upstream) vs git pull --rebase",
    },
    ScenarioSpec {
        id: "push/incremental@git-git",
        area: Area::Remotes,
        headline: true,
        name: Some("push"),
        description: "Incremental push vs git push",
    },
    ScenarioSpec {
        id: "push/force-with-lease@git-git",
        area: Area::Remotes,
        headline: true,
        name: Some("push"),
        description: "Amended commit force-with-lease push vs git push --force-with-lease",
    },
    ScenarioSpec {
        id: "clone/local@synth-10k",
        area: Area::Remotes,
        headline: true,
        name: Some("clone"),
        description: "Clone over file:// (not git clone --local hardlink shortcut) vs git clone",
    },
];

/// Remote matrix scenarios (non-headline) for the daily census.
pub const REMOTE_MATRIX_SCENARIOS: [ScenarioSpec; 11] = [
    ScenarioSpec {
        id: "fetch/noop@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "No-op fetch when client already matches remote (file:// git-git)",
    },
    ScenarioSpec {
        id: "ls-remote/all@many-refs",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "List all refs on many-refs bare remote vs git ls-remote",
    },
    ScenarioSpec {
        id: "remote/show@many-refs",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Remote show on many-refs client vs git remote show origin",
    },
    ScenarioSpec {
        id: "clone/http@synth-10k",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Clone synth-10k over grit smart HTTP vs git clone",
    },
    ScenarioSpec {
        id: "fetch/incremental-http@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Incremental fetch over grit smart HTTP (git-git bare remote)",
    },
    ScenarioSpec {
        id: "push/incremental-http@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Incremental push over grit smart HTTP (git-git bare remote)",
    },
    ScenarioSpec {
        id: "push/refspec@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Explicit refspec push HEAD:refs/heads/bench vs git push",
    },
    ScenarioSpec {
        id: "fetch/one-branch@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Fetch one branch refspec vs git fetch origin main",
    },
    ScenarioSpec {
        id: "fetch/dry-run@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Fetch dry-run reports updates without writing refs or objects",
    },
    ScenarioSpec {
        id: "fetch/deepen@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Deepen a depth-1 client by 50 commits vs git fetch --deepen 50",
    },
    ScenarioSpec {
        id: "pull/check@git-git",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "Pull --check reports fast-forward possibility without changing HEAD",
    },
];
