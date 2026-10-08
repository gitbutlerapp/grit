//! `grit` — a small, opinionated command line interface backed by `grit-lib`.
//!
//! `grit` deliberately does not mirror Git's UX. It favors a single obvious way
//! to do the common thing, plain-language output, and a status screen that
//! doubles as the home base: running `grit` with no arguments shows you where you
//! are, what's changed, and what to do next.

mod commands;
mod context;
mod diagnostics;
mod json_filter;
mod net;
mod output;
mod stdio;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use output::{emit, OutputMode, OutputOptions};

/// A small, opinionated Git client built on `grit-lib`.
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
    /// Serve a fetch or clone over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "upload-pack", hide = true)]
    UploadPack {
        #[command(flatten)]
        args: ServeArgs,
    },
    /// Accept a push over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "receive-pack", hide = true)]
    ReceivePack {
        #[command(flatten)]
        args: ServeArgs,
    },
}

/// Arguments shared by the `upload-pack` and `receive-pack` plumbing commands.
#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Answer a single request without advertising refs first (smart HTTP).
    #[arg(long)]
    stateless_rpc: bool,
    /// Print the ref advertisement and exit (smart HTTP discovery).
    #[arg(long, alias = "http-backend-info-refs")]
    advertise_refs: bool,
    /// The repository to serve.
    directory: String,
}

fn main() {
    stdio::configure();
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
/// human text or a single JSON object. The exceptions are `manager` (a raw
/// credential-helper protocol on stdin/stdout — no outcome), `upload-pack` and
/// `receive-pack` (the Git wire protocol on stdin/stdout — no outcome), and
/// `push` (which emits its per-ref outcome and then exits non-zero when a ref
/// was rejected).
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
        // The server programs speak the Git wire protocol on stdout.
        Command::UploadPack { args } => commands::serve::run(
            commands::serve::Service::UploadPack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        ),
        Command::ReceivePack { args } => commands::serve::run(
            commands::serve::Service::ReceivePack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use clap::CommandFactory;
    use serde_json::Value;

    use super::Cli;

    /// Options every command accepts; they're documented once on the docs
    /// overview page rather than on each command's page.
    const GLOBAL_OPTIONS: &[&str] = &["help", "version", "json", "filter"];

    /// Required `##` sections on every command page, in order. `Markdown output`
    /// may appear after `JSON output` when the command supports `--markdown`.
    const TEMPLATE_HEADINGS: &[&str] = &[
        "Synopsis",
        "Description",
        "Options",
        "Examples",
        "JSON output",
        "See also",
    ];

    /// Plumbing commands whose stdout is a wire protocol or credential stream,
    /// not a JSON outcome object.
    const NO_JSON_COMMANDS: &[&str] = &["manager", "upload-pack", "receive-pack"];

    fn commands_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands")
    }

    fn split_front_matter(text: &str) -> (&str, &str) {
        let Some(rest) = text.strip_prefix("---\n") else {
            return ("", text);
        };
        let Some((_, body)) = rest.split_once("\n---\n") else {
            return ("", text);
        };
        ("", body)
    }

    fn h2_headings(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| line.strip_prefix("## ").map(str::trim).map(String::from))
            .collect()
    }

    fn section_body(body: &str, heading: &str) -> Option<String> {
        let marker = format!("## {heading}");
        let start = body.find(&marker)? + marker.len();
        let rest = &body[start..];
        let end = rest.find("\n## ").map(|i| start + i).unwrap_or(body.len());
        Some(body[start..end].trim().to_owned())
    }

    fn fenced_json_blocks(section: &str) -> Vec<String> {
        const OPEN: &str = "```json";
        let mut blocks = Vec::new();
        let mut rest = section;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find("```") else {
                break;
            };
            blocks.push(after[..end].trim().to_owned());
            rest = &after[end + 3..];
        }
        blocks
    }

    fn json_section_is_none(section: &str) -> bool {
        let lower = section.to_ascii_lowercase();
        lower.contains("none.")
            || lower.contains("no json")
            || lower.contains("has no effect")
            || lower.contains("always speaks")
    }

    fn command_supports_markdown(command: &clap::Command) -> bool {
        command
            .get_arguments()
            .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
    }

    fn cli_supports_markdown() -> bool {
        Cli::command()
            .get_arguments()
            .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
    }

    fn top_level_keys(value: &Value) -> HashSet<String> {
        match value {
            Value::Object(map) => map.keys().cloned().collect(),
            _ => HashSet::new(),
        }
    }

    /// Keys named in the first column of a markdown field table inside the JSON section.
    fn field_table_top_level_keys(section: &str) -> HashSet<String> {
        let mut keys = HashSet::new();
        let mut past_header = false;
        for line in section.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                continue;
            }
            if trimmed.contains("---") {
                past_header = true;
                continue;
            }
            if !past_header {
                continue;
            }
            let Some(first_cell) = trimmed.split('|').nth(1) else {
                continue;
            };
            let cell = first_cell.trim().trim_matches('`');
            let top = cell.split(['.', '[']).next().unwrap_or(cell).trim();
            if top.is_empty() || top.eq_ignore_ascii_case("field") {
                continue;
            }
            keys.insert(top.to_owned());
        }
        keys
    }

    fn json_section_has_field_table(section: &str) -> bool {
        let has_header = section
            .lines()
            .any(|line| line.contains('|') && line.to_lowercase().contains("field"));
        let has_separator = section
            .lines()
            .any(|line| line.trim().starts_with('|') && line.contains("---"));
        has_header && has_separator && !field_table_top_level_keys(section).is_empty()
    }

    fn page_mentions_markdown(body: &str) -> bool {
        body.contains("## Markdown output") || body.contains("--markdown")
    }

    /// Validate `##` heading order, allowing `Markdown output` only between JSON output and See also.
    fn validate_page_headings(name: &str, headings: &[String]) -> Vec<String> {
        let see_also_idx = TEMPLATE_HEADINGS.len() - 1;
        let mut problems = Vec::new();
        let mut template_idx = 0usize;
        let mut heading_idx = 0usize;
        while heading_idx < headings.len() {
            let heading = &headings[heading_idx];
            if template_idx < TEMPLATE_HEADINGS.len() && heading == &TEMPLATE_HEADINGS[template_idx]
            {
                template_idx += 1;
                heading_idx += 1;
                continue;
            }
            if heading == "Markdown output" {
                if template_idx == see_also_idx {
                    heading_idx += 1;
                    continue;
                }
                if template_idx > see_also_idx {
                    problems.push(format!("{name}.md: unexpected ## Markdown output"));
                } else {
                    problems.push(format!(
                        "{name}.md: ## Markdown output must appear only after ## JSON output and before ## See also"
                    ));
                }
                heading_idx += 1;
                continue;
            }
            if template_idx < TEMPLATE_HEADINGS.len() {
                problems.push(format!(
                    "{name}.md: expected ## {} but found ## {heading}",
                    TEMPLATE_HEADINGS[template_idx]
                ));
            } else {
                problems.push(format!("{name}.md: unexpected ## {heading}"));
            }
            heading_idx += 1;
            template_idx = TEMPLATE_HEADINGS.len();
        }
        if template_idx < TEMPLATE_HEADINGS.len() {
            problems.push(format!(
                "{name}.md: missing ## {}",
                TEMPLATE_HEADINGS[template_idx]
            ));
        }
        problems
    }

    /// Every command page uses the documented section order.
    #[test]
    fn command_pages_follow_template() {
        let dir = commands_dir();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                problems.push(format!("missing page {}", path.display()));
                continue;
            };
            let (_, body) = split_front_matter(&text);
            problems.extend(validate_page_headings(name, &h2_headings(body)));
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn command_pages_follow_template_rejects_markdown_after_see_also() {
        let headings = vec![
            "Synopsis".into(),
            "Description".into(),
            "Options".into(),
            "Examples".into(),
            "JSON output".into(),
            "See also".into(),
            "Markdown output".into(),
        ];
        let problems = validate_page_headings("commit", &headings);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unexpected ## Markdown output")),
            "{problems:?}"
        );
    }

    /// JSON examples in the JSON output section must parse, and document top-level keys.
    #[test]
    fn json_examples_parse() {
        let dir = commands_dir();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = split_front_matter(&text);
            let Some(section) = section_body(body, "JSON output") else {
                problems.push(format!("{name}.md: no JSON output section"));
                continue;
            };
            let blocks = fenced_json_blocks(&section);
            if NO_JSON_COMMANDS.contains(&name) {
                if !blocks.is_empty() {
                    problems.push(format!(
                        "{name}.md: plumbing command must not include ```json examples"
                    ));
                }
                if !json_section_is_none(&section) {
                    problems.push(format!(
                        "{name}.md: JSON output section must explain that there is no JSON"
                    ));
                }
                continue;
            }
            if blocks.is_empty() {
                problems.push(format!(
                    "{name}.md: JSON output section needs a ```json example"
                ));
                continue;
            }
            if !json_section_has_field_table(&section) {
                problems.push(format!(
                    "{name}.md: JSON output section needs a field table (| Field | … |)"
                ));
                continue;
            }
            let table_keys = field_table_top_level_keys(&section);
            for (i, block) in blocks.iter().enumerate() {
                let parsed: Value = serde_json::from_str(block).unwrap_or_else(|e| {
                    problems.push(format!("{name}.md: JSON example {i} does not parse: {e}"));
                    Value::Null
                });
                if parsed.is_null() {
                    continue;
                }
                for key in top_level_keys(&parsed) {
                    if !table_keys.contains(&key) {
                        problems.push(format!(
                            "{name}.md: JSON example documents `{key}` but the field table does not"
                        ));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn json_examples_parse_requires_field_table_not_prose() {
        let section = r#"Pass `--json` for output.

The keys are `initialized`, `path`, `bare`, and `branch`.

```json
{"initialized": true, "path": "/tmp/x", "bare": false, "branch": "main"}
```
"#;
        assert!(!json_section_has_field_table(section));
        let keys = field_table_top_level_keys(section);
        assert!(keys.is_empty());
    }

    /// `--markdown` is documented only when clap exposes a `markdown` flag.
    #[test]
    fn markdown_output_only_when_supported() {
        let dir = commands_dir();
        let global_markdown = cli_supports_markdown();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = split_front_matter(&text);
            let mentions_markdown = page_mentions_markdown(body);
            let supported = global_markdown || command_supports_markdown(command);
            if mentions_markdown && !supported {
                problems.push(format!(
                    "{name}.md documents --markdown but the command has no markdown flag"
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn markdown_output_only_when_supported_detects_unbackticked_flag() {
        let body = "## JSON output\n\nPass --markdown for agent output.\n";
        assert!(page_mentions_markdown(body));
    }

    /// Every command (including hidden plumbing) has a man page in
    /// `content/docs/commands/`, and that page mentions each of its flags and
    /// subcommands. Positional arguments are named freely in the synopsis. Run `python3 scripts/docs.py` after editing the pages.
    #[test]
    fn every_command_is_documented() {
        let pages = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands");
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = pages.join(format!("{name}.md"));
            let Ok(page) = std::fs::read_to_string(&path) else {
                problems.push(format!("missing page {}", path.display()));
                continue;
            };
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                if GLOBAL_OPTIONS.contains(&id) {
                    continue;
                }
                let spellings = arg
                    .get_long()
                    .map(|long| format!("--{long}"))
                    .into_iter()
                    .chain(arg.get_short().map(|short| format!("-{short}")));
                for spelling in spellings {
                    if !page.contains(&format!("`{spelling}")) {
                        problems.push(format!("{name}.md does not document `{spelling}`"));
                    }
                }
            }
            for sub in command.get_subcommands() {
                let sub_name = sub.get_name();
                if sub_name != "help" && !page.contains(&format!("`{sub_name}")) {
                    problems.push(format!("{name}.md does not document `{sub_name}`"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
