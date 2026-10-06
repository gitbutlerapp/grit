//! `grit` — a small, opinionated command line interface backed by `grit-lib`.
//!
//! `grit` deliberately does not mirror Git's UX. It favors a single obvious way
//! to do the common thing, plain-language output, and a status screen that
//! doubles as the home base: running `grit` with no arguments shows you where you
//! are, what's changed, and what to do next.

mod commands;
mod context;
mod json_filter;
mod net;
mod output;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use output::{emit, OutputMode, OutputOptions};

/// A simplified alternative to the Git-compatible `grit-git` command line.
#[derive(Debug, Parser)]
#[command(name = "grit", version, about = "A simple Grit-powered CLI")]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
    /// jq-like expression applied to JSON output (requires `--json`).
    ///
    /// Examples: `.branch`, `.commits[].oid`, `{branch, clean}`.
    #[arg(long, global = true, value_name = "EXPR")]
    filter: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands of `grit remote`.
#[derive(Debug, Subcommand)]
enum RemoteAction {
    /// Add a new remote.
    Add {
        /// Short name for the remote (e.g. origin).
        name: String,
        /// The remote's URL or path.
        url: String,
    },
}

/// Subcommands of `grit auth`.
#[derive(Debug, Subcommand)]
enum AuthAction {
    /// Forget the stored GitHub token (sign out).
    Logout,
}

/// Top-level `grit` commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new, empty repository.
    Init {
        /// Where to create the repository (defaults to the current directory).
        path: Option<String>,
        /// Create a bare repository (no working tree).
        #[arg(long)]
        bare: bool,
    },
    /// Copy a remote repository into a new directory.
    Clone {
        /// The repository to clone (URL or local path).
        url: String,
        /// Directory to clone into (defaults to the repository name).
        dir: Option<String>,
    },
    /// List remotes, or add one.
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
    },
    /// Show recent commits reachable from HEAD.
    Log {
        /// Continue listing from before this commit (for paging).
        #[arg(long)]
        before: Option<String>,
    },
    /// Show changes as a diff. No argument: uncommitted changes; with a commit:
    /// the change that commit introduced.
    Diff {
        /// Commit to show the diff of (defaults to uncommitted changes).
        commit: Option<String>,
    },
    /// Show information about a commit, tag, or branch (defaults to HEAD).
    Show {
        /// The commit, tag, or branch to show.
        object: Option<String>,
    },
    /// Show what's changed and where you are (this is the default).
    #[command(alias = "st")]
    Status,
    /// List the commits on this branch that aren't on the target branch yet.
    #[command(alias = "sl")]
    Shortlog,
    /// Stage changes. With no paths, stages everything.
    Add {
        /// Files or directories to stage. Omit to stage all changes.
        paths: Vec<String>,
    },
    /// Stage every change and record a new commit.
    Commit {
        /// Commit message (you can also pass it with -m).
        message: Option<String>,
        /// Commit message.
        #[arg(short = 'm', long = "message", conflicts_with = "message")]
        message_flag: Option<String>,
        /// Stage every change first, then commit. This is the default behavior.
        #[arg(short = 'a', long = "all")]
        all: bool,
    },
    /// List branches, or create / delete one.
    Branch {
        /// Name of the branch to create. Omit to list branches.
        name: Option<String>,
        /// Delete the named branch instead of creating it.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
        /// Delete even when the branch is not fully merged into the current branch.
        #[arg(short = 'D', long = "force")]
        force_delete: bool,
    },
    /// Switch to another branch.
    #[command(alias = "checkout", alias = "co")]
    Switch {
        /// Branch to switch to.
        name: String,
        /// Create the branch first, then switch to it.
        #[arg(short = 'c', long = "create")]
        create: bool,
    },
    /// Merge another branch into the current one.
    Merge {
        /// Branch to merge in.
        branch: String,
    },
    /// Cherry-pick a commit onto the current branch.
    Pick {
        /// Commit to pick (any revision spec — full / short oid, branch, HEAD~2, …).
        commit: String,
    },
    /// Download refs and objects from a remote.
    Fetch {
        /// Remote to fetch from (defaults to origin).
        remote: Option<String>,
    },
    /// Fetch from the remote and integrate it into the current branch.
    Pull,
    /// Publish the current branch to its remote.
    Push {
        /// Push tags instead of the current branch. Pushes every local tag
        /// under `refs/tags/` to the remote.
        #[arg(short = 't', long = "tags")]
        tags: bool,
    },
    /// List tags, or create / delete one. A new tag points at HEAD.
    Tag {
        /// Name of the tag to create. Omit to list tags.
        name: Option<String>,
        /// Delete the named tag instead of creating it.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
    },
    /// Sign in to GitHub (device flow) and store a token for HTTPS push/fetch.
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Update grit to the latest release (re-runs the install script).
    Update,
    /// Read, set, or list configuration values.
    Config {
        /// Use the global (per-user) config file instead of this repository's.
        #[arg(long)]
        global: bool,
        /// List all configuration values.
        #[arg(short = 'l', long = "list")]
        list: bool,
        /// Remove the key instead of reading or setting it.
        #[arg(long)]
        unset: bool,
        /// The configuration key, e.g. user.name. Omit only with --list.
        key: Option<String>,
        /// The value to set. Omit to read the current value.
        value: Option<String>,
    },
    /// Git credential helper backed by the Windows Credential Manager.
    ///
    /// You don't normally run this yourself — `grit auth` wires it into
    /// `credential.helper` on Windows. It speaks Git's credential protocol.
    Manager {
        /// The credential operation: get, store, or erase.
        operation: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let opts = OutputOptions {
        mode: if cli.json {
            OutputMode::Json
        } else {
            OutputMode::Human
        },
        filter: cli.filter.clone(),
    };
    if let Err(err) = opts.validate() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
    if let Err(err) = dispatch(cli, &opts) {
        output::emit_error(&err, &opts);
        std::process::exit(1);
    }
}

/// Run the selected subcommand and render its outcome.
///
/// Each command computes a typed, serializable outcome; [`emit`] renders it as
/// human text or a single JSON object. The two exceptions are `manager` (a raw
/// credential-helper protocol on stdin/stdout — no outcome) and `push` (which
/// emits its per-ref outcome and then exits non-zero when a ref was rejected).
fn dispatch(cli: Cli, opts: &OutputOptions) -> Result<()> {
    match cli.command.unwrap_or(Command::Status) {
        Command::Init { path, bare } => emit(&commands::init::run(path, bare)?, opts),
        Command::Clone { url, dir } => emit(&commands::clone::run(&url, dir, opts.mode)?, opts),
        Command::Remote { action } => {
            let add = action.map(|RemoteAction::Add { name, url }| (name, url));
            emit(&commands::remote::run(add)?, opts)
        }
        Command::Log { before } => emit(&commands::log::run(before)?, opts),
        Command::Diff { commit } => emit(&commands::diff::run(commit)?, opts),
        Command::Show { object } => emit(&commands::show::run(object)?, opts),
        Command::Status => emit(&commands::status::run()?, opts),
        Command::Shortlog => emit(&commands::shortlog::run()?, opts),
        Command::Add { paths } => emit(&commands::add::run(&paths)?, opts),
        Command::Commit {
            message,
            message_flag,
            all: _,
        } => emit(&commands::commit::run(message.or(message_flag))?, opts),
        Command::Branch {
            name,
            delete,
            force_delete,
        } => emit(&commands::branch::run(name, delete, force_delete)?, opts),
        Command::Tag { name, delete } => emit(&commands::tag::run(name, delete)?, opts),
        Command::Switch { name, create } => emit(&commands::switch::run(&name, create)?, opts),
        Command::Merge { branch } => emit(&commands::merge::run(&branch)?, opts),
        Command::Pick { commit } => emit(&commands::pick::run(&commit)?, opts),
        Command::Fetch { remote } => emit(&commands::fetch::run(remote)?, opts),
        Command::Pull => emit(&commands::pull::run()?, opts),
        Command::Push { tags } => {
            let outcome = commands::push::run(tags)?;
            emit(&outcome, opts)?;
            // The outcome (per-ref results) is reported in both modes; a rejected
            // push is still a failure, so mirror Git and exit non-zero.
            if outcome.rejected {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Auth { action } => match action {
            None => emit(&commands::auth::run()?, opts),
            Some(AuthAction::Logout) => emit(&commands::auth::logout()?, opts),
        },
        Command::Update => emit(&commands::update::run(opts.mode)?, opts),
        Command::Config {
            global,
            list,
            unset,
            key,
            value,
        } => emit(
            &commands::config::run(global, list, unset, key, value)?,
            opts,
        ),
        // `manager` speaks Git's credential protocol on stdout; it has no JSON form.
        Command::Manager { operation } => commands::manager::run(&operation),
    }
}
