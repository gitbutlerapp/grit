//! Shared-branch report types used by `push`, `sync`, `share` and `status` outcomes.

use schemars::JsonSchema;
use serde::Serialize;

/// How another member's head compares with your branch here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadRelation {
    /// The same commit as yours.
    Same,
    /// It has commits yours doesn't, and yours is in its history: `grit
    /// sync` fast-forwards to it.
    Ahead,
    /// Yours has commits it doesn't, and it is in yours.
    Behind,
    /// Each has commits the other lacks.
    Diverged,
    /// You have no branch here, or its commits aren't fetched.
    Unknown,
}

/// One commit of a divergence: what each side did to it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DivergenceChangeJson {
    /// Its change id, or `patch:<patch id>` for a commit without one.
    pub change: String,
    /// Its subject line.
    pub subject: String,
    /// What you did to it: `unchanged`, `moved`, `reworded`, `amended`,
    /// `dropped`, `squashed`, `split`, `new`, `split_piece` or `absent`.
    pub you: String,
    /// What they did to it, in the same words.
    pub them: String,
    /// The paths its versions change.
    pub paths: Vec<String>,
}

/// One way out of a divergence.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResolveOptionJson {
    /// Its stable id: `combine`, `keep_mine`, `keep_theirs`, `keep_both`,
    /// `keep_amended`, `drop`, `my_order`, `their_order`, `my_split`,
    /// `their_split`, `restore` or `resolve`.
    pub id: String,
    /// What it does.
    pub label: String,
    /// What it leaves out (kept in the share's log), or `null` when it keeps
    /// every change: such an option is safe to choose without asking.
    pub loses: Option<String>,
    /// The command that applies it.
    pub command: String,
}

/// How another member's head diverged from yours
/// ([`grit_lib::divergence`]).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DivergenceJson {
    /// The scenario's stable code, such as `amended_both_conflicting`.
    pub scenario: String,
    /// What happened, in a sentence.
    pub summary: String,
    /// Whether someone has to decide what to do. When `false`, the heads can
    /// be put together without losing anything.
    pub needs_decision: bool,
    /// The last head you both had, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The commits either side changed or added, oldest first.
    pub changes: Vec<DivergenceChangeJson>,
    /// Paths where the two heads' changes conflict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflicts: Vec<String>,
    /// Other scenarios that also apply, most pressing first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub also: Vec<String>,
    /// The ways out, the recommended one first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub options: Vec<ResolveOptionJson>,
}

/// A branch pushed (or synced) again after its share was integrated,
/// bringing integrated commits: scenario `pushed_to_integrated`. It is a
/// branch nobody shares now.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IntegratedJson {
    /// Always `pushed_to_integrated`.
    pub scenario: String,
    /// What happened, in a sentence.
    pub summary: String,
    /// Who integrated it.
    pub by: String,
    /// When, RFC 3339.
    pub at: String,
    /// The project it was merged into, `<owner>/<project>`.
    pub into: String,
    /// The branch it was merged into.
    pub target_branch: String,
    /// The pull request it went through, for an organization's project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull: Option<u64>,
    /// The pull request's page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// How many of the branch's commits were integrated already.
    pub integrated: usize,
    /// How many are new.
    pub new: usize,
    /// The ways forward, recommended first.
    pub options: Vec<ResolveOptionJson>,
}

/// Another member's head of a shared branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SharedHead {
    /// Their user name.
    pub user: String,
    /// Their head's tip.
    pub tip: String,
    /// How it compares with your branch here.
    pub relation: HeadRelation,
    /// For a head that diverged from yours: what happened, and whether
    /// someone has to decide what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub divergence: Option<DivergenceJson>,
}
