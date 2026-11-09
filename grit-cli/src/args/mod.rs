//! Clap argument definitions for `grit`.

mod argv;

pub use argv::{normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli};

mod add;
mod auth;
mod blame;
mod branch;
mod bundle;
mod cat;
mod checkin;
mod checkout;
mod clean;
mod clone;
mod commit;
mod completions;
mod config;
mod describe;
mod diff;
mod fetch;
mod fsck;
mod gc;
mod history;
mod hooks;
mod ignore;
mod init;
mod integrate;
mod log;
mod manager;
mod merge;
mod merge_base;
mod mv;
mod notes;
mod patch_id;
mod pick;
mod pull;
mod push;
mod rebase;
mod receive_pack;
mod reflog;
mod remote;
mod repack;
mod reset;
mod resolve;
mod restore;
mod revert;
mod revs;
mod rm;
mod serve;
mod share;
mod shortlog;
mod show;
mod skill;
mod slice;
mod stash;
mod status;
mod switch;
mod sync;
mod tag;
mod update;
mod upload_pack;
mod worktree;

pub(crate) use add::AddArgs;
pub(crate) use auth::AuthArgs;
pub(crate) use blame::BlameArgs;
pub(crate) use branch::BranchArgs;
pub(crate) use bundle::BundleArgs;
pub(crate) use cat::CatArgs;
pub(crate) use checkin::CheckinArgs;
pub(crate) use checkout::CheckoutArgs;
pub(crate) use clean::CleanArgs;
pub(crate) use clone::CloneArgs;
pub(crate) use commit::CommitArgs;
pub(crate) use completions::CompletionsArgs;
pub(crate) use config::ConfigArgs;
pub(crate) use describe::DescribeArgs;
pub(crate) use diff::DiffArgs;
pub(crate) use fetch::FetchArgs;
pub(crate) use fsck::FsckArgs;
pub(crate) use gc::GcArgs;
pub(crate) use history::HistoryArgs;
pub(crate) use hooks::HooksArgs;
pub(crate) use ignore::IgnoreArgs;
pub(crate) use init::InitArgs;
pub(crate) use integrate::IntegrateArgs;
pub(crate) use log::LogArgs;
pub(crate) use manager::ManagerArgs;
pub(crate) use merge::MergeArgs;
pub(crate) use merge_base::MergeBaseArgs;
pub(crate) use mv::MvArgs;
pub(crate) use notes::NotesArgs;
pub(crate) use patch_id::PatchIdArgs;
pub(crate) use pick::PickArgs;
pub(crate) use pull::PullArgs;
pub(crate) use push::PushArgs;
pub(crate) use rebase::RebaseArgs;
pub(crate) use receive_pack::ReceivePackArgs;
pub(crate) use reflog::ReflogArgs;
pub(crate) use remote::RemoteArgs;
pub(crate) use repack::RepackArgs;
pub(crate) use reset::ResetArgs;
pub(crate) use resolve::ResolveArgs;
pub(crate) use restore::RestoreArgs;
pub(crate) use revert::RevertArgs;
pub(crate) use revs::RevsArgs;
pub(crate) use rm::RmArgs;
pub(crate) use serve::ServeCommandArgs;
pub(crate) use share::ShareArgs;
pub(crate) use shortlog::ShortlogArgs;
pub(crate) use show::ShowArgs;
pub(crate) use skill::SkillArgs;
pub(crate) use slice::SliceArgs;
pub(crate) use stash::StashArgs;
pub(crate) use status::StatusArgs;
pub(crate) use switch::SwitchArgs;
pub(crate) use sync::SyncArgs;
pub(crate) use tag::TagArgs;
pub(crate) use update::UpdateArgs;
pub(crate) use upload_pack::UploadPackArgs;
pub(crate) use worktree::WorktreeArgs;

pub(crate) use auth::AuthAction;
pub(crate) use bundle::BundleAction;
pub(crate) use gc::GcAction;
pub(crate) use history::HistoryAction;
pub(crate) use hooks::HooksAction;
pub(crate) use ignore::IgnoreAction;
pub(crate) use log::LogGroupByArg;
pub(crate) use notes::NotesAction;
pub(crate) use reflog::ReflogAction;
pub(crate) use remote::{RemoteAction, RemoteRefSortArg};
pub(crate) use serve::ServeAction;
pub(crate) use share::ShareAction;
pub(crate) use slice::SliceAction;
pub(crate) use stash::StashAction;
pub(crate) use worktree::WorktreeAction;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::branch_list_args;
use crate::clap_value_args;
use crate::diff_args;
use crate::diff_format;
use crate::global_args;
use crate::limit_args;
use crate::merge_strategy_args;
use crate::message_input;
use crate::pathspec_args;
use crate::quiet_args;
use crate::rev_select;

fn parse_init_object_format(value: &str) -> Result<grit_lib::objects::HashAlgo, String> {
    grit_lib::objects::HashAlgo::from_name(value)
        .ok_or_else(|| format!("unknown object format '{value}' (expected sha1 or sha256)"))
}


/// A small, opinionated Git client built on `grit-lib`.
#[derive(Debug, Parser)]
#[command(
    name = "grit",
    version,
    about = "A simple Grit-powered CLI",
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    pub(crate) version: (),
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "markdown")]
    pub(crate) json: bool,
    /// Emit agent-friendly Markdown instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "json")]
    pub(crate) markdown: bool,
    /// jq-like expression applied to JSON output (requires `--json`).
    ///
    /// Iterator-style filters (for example `.commits[].oid`) always emit a JSON
    /// array; singular filters (for example `.branch`) emit one value, including
    /// `null` when a field is missing.
    #[arg(long, global = true, value_name = "EXPR")]
    pub(crate) filter: Option<String>,
    /// Worker threads for parallel library work (`0` = auto, `1` = serial).
    #[arg(long, global = true, value_name = "N")]
    pub(crate) threads: Option<usize>,
    #[command(flatten)]
    pub(crate) repo: global_args::GlobalRepoArgs,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

























/// The snapshot view `grit share add --no-history --path …` asks for.
pub(crate) fn api_view(paths: Vec<String>) -> grit_lib::remote::grit_server_share::ShareView {
    grit_lib::remote::grit_server_share::ShareView { paths }
}



/// Top-level `grit` commands.
///
/// In the binary, each subcommand's arguments are built only when that subcommand is parsed,
/// so startup does not pay for the whole tree. Help output and completions go through
/// [`cli_command::full_cli_command`] so deferred building cannot replace a subcommand's
/// description. Unit tests keep the tree eager: the contract, docs and schema checks walk each
/// command's declared arguments, which `Command::build` would mix with globals inherited from
/// the parent.
#[derive(Debug, Subcommand)]
#[cfg_attr(not(test), command(defer = true))]
pub(crate) enum Command {
    /// Create a new, empty repository.
    Init {
        #[command(flatten)]
        args: InitArgs,
    },
    /// Start work on a repository from a snapshot of its main branch (or some of its paths) instead of a clone.
    Checkout {
        #[command(flatten)]
        args: CheckoutArgs,
    },
    /// Send a snapshot checkout's branches back to the server, grafted onto its real history.
    Checkin {
        #[command(flatten)]
        args: CheckinArgs,
    },
    /// Merge a branch into the server's target branch (checking it in first), then refresh this checkout.
    Integrate {
        #[command(flatten)]
        args: IntegrateArgs,
    },
    /// Push and pull every active branch between this clone and your space on the server (grithub).
    Sync {
        #[command(flatten)]
        args: SyncArgs,
    },
    /// Work on a branch together with other users of the server: each has their own head of it.
    Share {
        #[command(flatten)]
        args: ShareArgs,
    },
    /// Named sets of paths to check out together, kept on the server's target branch.
    Slice {
        #[command(flatten)]
        args: SliceArgs,
    },
    /// Copy a remote repository into a new directory.
    ///
    /// Submodule gitlinks are checked out as empty directories only; grit does not fetch submodule
    /// repositories or provide a `submodule` command.
    Clone {
        #[command(flatten)]
        args: CloneArgs,
    },
    /// List remotes, or add one.
    Remote {
        #[command(flatten)]
        args: RemoteArgs,
    },
    /// Show recent commits reachable from HEAD.
    Log {
        #[command(flatten)]
        args: LogArgs,
    },
    /// List or count commits matching revision ranges.
    Revs {
        #[command(flatten)]
        args: RevsArgs,
    },
    /// Find merge bases or test whether one commit is an ancestor of another.
    #[command(name = "merge-base")]
    MergeBase {
        #[command(flatten)]
        args: MergeBaseArgs,
    },
    /// Show, delete, or expire reflog entries.
    Reflog {
        #[command(flatten)]
        args: ReflogArgs,
    },
    /// Show who last modified each line of a file.
    Blame {
        #[command(flatten)]
        args: BlameArgs,
    },
    /// Nearest reachable tag, exact commit distance, and abbreviated id for a revision.
    #[command(name = "describe")]
    Describe {
        #[command(flatten)]
        args: DescribeArgs,
    },
    /// Show changes as a diff. No argument: uncommitted changes; with a commit:
    /// the change that commit introduced; with two commits (or `A..B`) diff their trees.
    Diff {
        #[command(flatten)]
        args: DiffArgs,
    },
    /// Show information about a commit, tag, or branch (defaults to HEAD).
    Show {
        #[command(flatten)]
        args: ShowArgs,
    },
    /// Show what's changed and where you are (this is the default).
    #[command(alias = "st")]
    Status {
        #[command(flatten)]
        args: StatusArgs,
    },
    /// List the commits on this branch that aren't on the target branch yet.
    #[command(alias = "sl", visible_alias = "ahead")]
    Shortlog {
        #[command(flatten)]
        args: ShortlogArgs,
    },
    /// Stage changes. With no paths, stages everything.
    Add {
        #[command(flatten)]
        args: AddArgs,
    },
    /// Remove tracked paths from the working tree and index.
    Rm {
        #[command(flatten)]
        args: RmArgs,
    },
    /// Rename or move tracked paths.
    Mv {
        #[command(flatten)]
        args: MvArgs,
    },
    /// Remove untracked files from the working tree.
    Clean {
        #[command(flatten)]
        args: CleanArgs,
    },
    /// Pack reachable objects and remove redundant packs.
    Repack {
        #[command(flatten)]
        args: RepackArgs,
    },
    /// Pack refs, expire reflogs, repack, prune, and refresh commit-graph.
    Gc {
        #[command(flatten)]
        args: GcArgs,
    },
    /// Check refs, objects, and pack indexes for problems.
    #[command(alias = "verify")]
    Fsck {
        #[command(flatten)]
        args: FsckArgs,
    },
    /// Query exclude (gitignore) rules.
    Ignore {
        #[command(flatten)]
        args: IgnoreArgs,
    },
    /// List hook scripts or run one by name.
    Hooks {
        #[command(flatten)]
        args: HooksArgs,
    },
    /// Print stable patch-IDs for commits or revision ranges.
    #[command(name = "patch-id")]
    PatchId {
        #[command(flatten)]
        args: PatchIdArgs,
    },
    /// Resolve revision specs to object ids and types.
    Resolve {
        #[command(flatten)]
        args: ResolveArgs,
    },
    /// Show blob, tree, commit, or tag contents for a revision spec.
    Cat {
        #[command(flatten)]
        args: CatArgs,
    },
    /// Stage every change and record a new commit.
    Commit {
        #[command(flatten)]
        args: CommitArgs,
    },
    /// List branches, or create / delete / rename / configure one.
    Branch {
        #[command(flatten)]
        args: BranchArgs,
    },
    /// Switch to another branch, remote branch, or detached commit.
    #[command(alias = "co")]
    Switch {
        #[command(flatten)]
        args: SwitchArgs,
    },
    /// Move the branch tip and optionally reset the index or worktree.
    Reset {
        #[command(flatten)]
        args: ResetArgs,
    },
    /// Merge another branch, tag or commit into the current branch.
    Merge {
        #[command(flatten)]
        args: MergeArgs,
    },
    /// Cherry-pick commits onto the current branch.
    Pick {
        #[command(flatten)]
        args: PickArgs,
    },
    /// Undo commits with new inverse commits.
    Revert {
        #[command(flatten)]
        args: RevertArgs,
    },
    /// Rewrite commits without a sequencer (plan, then apply refs atomically).
    History {
        #[command(flatten)]
        args: HistoryArgs,
    },
    /// Save or restore shelved work.
    Stash {
        #[command(flatten)]
        args: StashArgs,
    },
    /// Restore working tree and/or index paths.
    Restore {
        #[command(flatten)]
        args: RestoreArgs,
    },
    /// Download refs and objects from a remote.
    Fetch {
        #[command(flatten)]
        args: FetchArgs,
    },
    /// Fetch from the remote and integrate it into the current branch.
    ///
    /// In a snapshot checkout, refresh it onto the server's newest snapshot;
    /// `grit pull <branch>` brings `<branch>` onto the server's branch of that
    /// name, as the checkout sees it.
    Pull {
        #[command(flatten)]
        args: PullArgs,
    },
    /// Replay local commits onto a new base (atomic ref update).
    Rebase {
        #[command(flatten)]
        args: RebaseArgs,
    },
    /// Publish refs to a remote.
    Push {
        #[command(flatten)]
        args: PushArgs,
    },
    /// Create, verify, or list git bundle files.
    Bundle {
        #[command(flatten)]
        args: BundleArgs,
    },
    /// List, show, add, or remove Git notes on commits.
    Notes {
        #[command(flatten)]
        args: NotesArgs,
    },
    /// List tags, or create / delete one.
    Tag {
        #[command(flatten)]
        args: TagArgs,
    },
    /// List, add, or remove linked worktrees.
    Worktree {
        #[command(flatten)]
        args: WorktreeArgs,
    },
    /// Sign in to grithub (GitHub inside a github.com repository), a `grit serve` host, or store a username and token for any HTTPS host.
    #[command(args_conflicts_with_subcommands = true, visible_alias = "login")]
    Auth {
        #[command(flatten)]
        args: AuthArgs,
    },
    /// Update grit to the latest release (re-runs the install script).
    Update {
        #[command(flatten)]
        args: UpdateArgs,
    },
    /// Print an agent skill (SKILL.md) that explains how to use grit.
    Skill {
        #[command(flatten)]
        args: SkillArgs,
    },
    /// Generate shell completion scripts for bash, zsh, or fish.
    Completions {
        #[command(flatten)]
        args: CompletionsArgs,
    },
    /// Read, set, or list configuration values.
    Config {
        #[command(flatten)]
        args: ConfigArgs,
    },
    /// Git credential helper backed by the Windows Credential Manager.
    ///
    /// You don't normally run this yourself — `grit auth` wires it into
    /// `credential.helper` on Windows. It speaks Git's credential protocol.
    Manager {
        #[command(flatten)]
        args: ManagerArgs,
    },
    /// Serve repositories over HTTP, with a web view where people sign in with email and password.
    Serve {
        #[command(flatten)]
        args: ServeCommandArgs,
    },
    /// Serve a fetch or clone over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "upload-pack", hide = true)]
    UploadPack {
        #[command(flatten)]
        args: UploadPackArgs,
    },
    /// Accept a push over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "receive-pack", hide = true)]
    ReceivePack {
        #[command(flatten)]
        args: ReceivePackArgs,
    },
}

/// Arguments shared by the `upload-pack` and `receive-pack` plumbing commands.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeArgs {
    /// Answer a single request without advertising refs first (smart HTTP).
    #[arg(long)]
    pub(crate) stateless_rpc: bool,
    /// Print the ref advertisement and exit (smart HTTP discovery).
    #[arg(long, alias = "http-backend-info-refs")]
    pub(crate) advertise_refs: bool,
    /// The repository to serve.
    pub(crate) directory: String,
}
