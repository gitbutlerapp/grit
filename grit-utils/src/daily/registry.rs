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

/// The 17 headline scenarios: ids and short names fixed by the performance census roadmap.
pub const HEADLINE_SCENARIOS: [HeadlineScenario; 17] = [
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
        id: "push/incremental@git-git",
        area: Area::Remotes,
        headline: true,
        name: Some("push"),
        description: "Incremental push vs git push",
    },
    ScenarioSpec {
        id: "clone/local@synth-10k",
        area: Area::Remotes,
        headline: true,
        name: Some("clone"),
        description: "Local clone vs git clone",
    },
];

/// Matrix scenarios (roadmap item 2358): branching, refs, maintenance, and misc areas.
pub const MATRIX_SCENARIOS: [ScenarioSpec; 28] = [
    ScenarioSpec {
        id: "switch/branch@synth-100k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Switch branch on 100k-file synthetic vs git switch",
    },
    ScenarioSpec {
        id: "branch/create@many-refs",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Create one branch vs git branch",
    },
    ScenarioSpec {
        id: "branch/delete@many-refs",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Delete one branch vs git branch -D",
    },
    ScenarioSpec {
        id: "branch/list@many-refs-loose",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "List branches on loose many-refs fixture vs git branch",
    },
    ScenarioSpec {
        id: "tag/list@many-refs",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "List tags vs git tag -l",
    },
    ScenarioSpec {
        id: "tag/create@many-refs",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Create lightweight tag vs git tag",
    },
    ScenarioSpec {
        id: "merge/clean@synth-100k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Merge branch on 100k-file synthetic vs git merge",
    },
    ScenarioSpec {
        id: "revert/one@synth-10k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Revert one commit vs git revert --no-edit",
    },
    ScenarioSpec {
        id: "pick/one@synth-100k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "Cherry-pick one commit on 100k-file synthetic vs git cherry-pick",
    },
    ScenarioSpec {
        id: "history/reword@synth-10k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "History reword vs git history reword or scripted rebase",
    },
    ScenarioSpec {
        id: "stash/list@synth-10k",
        area: Area::Branching,
        headline: false,
        name: None,
        description: "List stash entries vs git stash list",
    },
    ScenarioSpec {
        id: "gc/pack-refs@synth-10k",
        area: Area::Refs,
        headline: false,
        name: None,
        description: "Pack loose tags via grit gc pack-refs vs git pack-refs",
    },
    ScenarioSpec {
        id: "gc/default@synth-10k",
        area: Area::Maintenance,
        headline: false,
        name: None,
        description: "Full grit gc vs git gc on synth-10k with loose objects",
    },
    ScenarioSpec {
        id: "repack/full@git-git",
        area: Area::Maintenance,
        headline: false,
        name: None,
        description: "grit gc repack vs git repack -ad on git.git worktree",
    },
    ScenarioSpec {
        id: "fsck/full@git-git",
        area: Area::Maintenance,
        headline: false,
        name: None,
        description: "Full fsck on git.git worktree vs git fsck",
    },
    ScenarioSpec {
        id: "fsck/full@big-blobs",
        area: Area::Maintenance,
        headline: false,
        name: None,
        description: "Full fsck on big-blobs fixture vs git fsck",
    },
    ScenarioSpec {
        id: "repack/commit-graph@git-git",
        area: Area::Maintenance,
        headline: false,
        name: None,
        description: "grit gc commit-graph vs git commit-graph write on git.git",
    },
    ScenarioSpec {
        id: "config/get@small",
        area: Area::Config,
        headline: false,
        name: None,
        description: "Read one config key vs git config",
    },
    ScenarioSpec {
        id: "config/set@small",
        area: Area::Config,
        headline: false,
        name: None,
        description: "Set one config key vs git config",
    },
    ScenarioSpec {
        id: "ignore/check-1000@synth-10k",
        area: Area::Ignore,
        headline: false,
        name: None,
        description: "Check 1000 paths against ignore rules vs git check-ignore",
    },
    ScenarioSpec {
        id: "hooks/list@small",
        area: Area::Hooks,
        headline: false,
        name: None,
        description: "List hook scripts vs listing .git/hooks",
    },
    ScenarioSpec {
        id: "notes/add@synth-10k",
        area: Area::Notes,
        headline: false,
        name: None,
        description: "Add commit note vs git notes add",
    },
    ScenarioSpec {
        id: "notes/list@synth-10k",
        area: Area::Notes,
        headline: false,
        name: None,
        description: "List notes vs git notes list",
    },
    ScenarioSpec {
        id: "bundle/create@synth-10k",
        area: Area::Bundles,
        headline: false,
        name: None,
        description: "Create bundle file vs git bundle create",
    },
    ScenarioSpec {
        id: "bundle/verify@synth-10k",
        area: Area::Bundles,
        headline: false,
        name: None,
        description: "Verify bundle vs git bundle verify",
    },
    ScenarioSpec {
        id: "worktree/add@synth-10k",
        area: Area::Worktrees,
        headline: false,
        name: None,
        description: "Add linked worktree vs git worktree add",
    },
    ScenarioSpec {
        id: "worktree/remove@synth-10k",
        area: Area::Worktrees,
        headline: false,
        name: None,
        description: "Remove linked worktree vs git worktree remove",
    },
    ScenarioSpec {
        id: "remote/list@small",
        area: Area::Remotes,
        headline: false,
        name: None,
        description: "List remotes vs git remote",
    },
];

/// Look up a registered daily scenario id (headline or matrix).
pub fn scenario_spec(id: &str) -> &'static ScenarioSpec {
    HEADLINE_SCENARIOS
        .iter()
        .chain(MATRIX_SCENARIOS.iter())
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("unknown daily scenario id {id}"))
}
