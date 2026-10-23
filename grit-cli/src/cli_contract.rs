//! CLI convention contract: classify every leaf command path and enforce shared flag rules.

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

/// How a leaf command is used in the grit interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    /// Bounded listing (`-n` / `--limit`).
    List,
    /// Changes repository or local state (`--dry-run` previews unless documented otherwise).
    Mutation,
    /// Read-only inspection of one object, diff, or repository health.
    Query,
    /// Protocol or credential plumbing (not `--json` outcomes).
    Plumbing,
    /// Emits scripts or agent docs (`completions`, `skill`).
    Generator,
}

#[derive(Debug, Clone, Copy)]
struct CommandContract {
    kind: CommandKind,
    takes_message: bool,
    takes_paths: bool,
}

/// Static classification for every leaf command path discovered from the clap tree.
const COMMAND_CONTRACTS: &[(&str, CommandContract)] = &[
    (
        "add",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "auth",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "auth.logout",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "blame",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "branch",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "bundle.create",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "bundle.list",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "bundle.unbundle",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "bundle.verify",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "cat",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "clean",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "clone",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "commit",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: true,
        },
    ),
    (
        "completions",
        CommandContract {
            kind: CommandKind::Generator,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "config",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "diff",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "fetch",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "fsck",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc.commit-graph",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc.midx",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc.pack-refs",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc.prune",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "gc.repack",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "history.absorb",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "history.drop",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "history.fixup",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: true,
        },
    ),
    (
        "history.move",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "history.rebase",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "history.reword",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "history.split",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: true,
        },
    ),
    (
        "history.squash",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "history.undo",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "hooks",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "hooks.list",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "hooks.run",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "ignore.check",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "init",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "log",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "manager",
        CommandContract {
            kind: CommandKind::Plumbing,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "merge",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "merge-base",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "mv",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "notes",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "notes.add",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "notes.list",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "notes.remove",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "notes.show",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "patch-id",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "pick",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "pull",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "push",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "rebase",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "receive-pack",
        CommandContract {
            kind: CommandKind::Plumbing,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "reflog",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "reflog.delete",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "reflog.expire",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "remote",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "remote.add",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "remote.set-url",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "remote.refs",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "repack",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "reset",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "resolve",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "restore",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "revert",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "revs",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "rm",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "shortlog",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "show",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "skill",
        CommandContract {
            kind: CommandKind::Generator,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "stash",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "stash.apply",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "stash.drop",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "stash.list",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "stash.pop",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "stash.push",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "stash.show",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "status",
        CommandContract {
            kind: CommandKind::Query,
            takes_message: false,
            takes_paths: true,
        },
    ),
    (
        "switch",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "tag",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: true,
            takes_paths: false,
        },
    ),
    (
        "update",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "upload-pack",
        CommandContract {
            kind: CommandKind::Plumbing,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.add",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.list",
        CommandContract {
            kind: CommandKind::List,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.lock",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.prune",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.remove",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
    (
        "worktree.unlock",
        CommandContract {
            kind: CommandKind::Mutation,
            takes_message: false,
            takes_paths: false,
        },
    ),
];

/// Convention rule checked against the clap tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContractRule {
    Limit,
    DryRun,
    Message,
    Paths,
    Force,
}

/// Known violations tracked until a follow-up plan step lands.
const EXCEPTIONS: &[(&str, ContractRule, &str, &str)] = &[
    (
        "branch",
        ContractRule::Limit,
        "s4 limit",
        "branch list uses `-n` / `--max-count` today; step 4 unifies on `--limit`",
    ),
    (
        "reflog",
        ContractRule::Limit,
        "inspection-extras",
        "reflog list uses `--limit` without `-n` today",
    ),
    (
        "shortlog",
        ContractRule::Limit,
        "inspection-extras",
        "shortlog caps in the library without a `-n` / `--limit` flag yet",
    ),
    (
        "bundle.list",
        ContractRule::Limit,
        "s4 limit",
        "bundle list has no `-n` / `--limit` yet",
    ),
    (
        "hooks",
        ContractRule::Limit,
        "s4 limit",
        "hooks list has no `-n` / `--limit` yet",
    ),
    (
        "hooks.list",
        ContractRule::Limit,
        "s4 limit",
        "hooks list has no `-n` / `--limit` yet",
    ),
    (
        "notes",
        ContractRule::Limit,
        "s4 limit",
        "notes list has no `-n` / `--limit` yet",
    ),
    (
        "notes.list",
        ContractRule::Limit,
        "s4 limit",
        "notes list has no `-n` / `--limit` yet",
    ),
    (
        "remote",
        ContractRule::Limit,
        "clone-remote",
        "remote list has no `-n` / `--limit` yet",
    ),
    (
        "stash.list",
        ContractRule::Limit,
        "workspace-extras",
        "stash list has no `-n` / `--limit` yet",
    ),
    (
        "worktree.list",
        ContractRule::Limit,
        "workspace-extras",
        "worktree list has no `-n` / `--limit` yet",
    ),
    (
        "tag",
        ContractRule::Message,
        "s3 message",
        "tag uses `-F` / `--file` instead of `--message-file` today",
    ),
    (
        "merge",
        ContractRule::Message,
        "s3 message",
        "merge message group has no `-F` short on `--message-file` yet (step 3)",
    ),
    (
        "pick",
        ContractRule::Message,
        "s3 message",
        "pick has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "revert",
        ContractRule::Message,
        "s3 message",
        "revert has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "history.reword",
        ContractRule::Message,
        "s3 message",
        "history reword has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "history.fixup",
        ContractRule::Message,
        "s3 message",
        "history fixup has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "history.split",
        ContractRule::Message,
        "s3 message",
        "history split has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "history.squash",
        ContractRule::Message,
        "s3 message",
        "history squash has `--message-file` without `-F` short yet (step 3)",
    ),
    (
        "notes.add",
        ContractRule::Message,
        "s3 message",
        "notes add requires `-m` but has no `--message-file` yet",
    ),
    (
        "merge",
        ContractRule::DryRun,
        "s5 dry-run",
        "merge has `--check` but no `--dry-run` yet (step 5)",
    ),
    (
        "notes.add",
        ContractRule::DryRun,
        "s5 dry-run",
        "notes add has no `--dry-run` yet (step 5)",
    ),
    (
        "bundle.create",
        ContractRule::DryRun,
        "s6 dry-run",
        "bundle create has no `--dry-run` yet (step 6)",
    ),
    (
        "bundle.unbundle",
        ContractRule::DryRun,
        "s6 dry-run",
        "bundle unbundle has no `--dry-run` yet (step 6)",
    ),
    (
        "gc.commit-graph",
        ContractRule::DryRun,
        "s6 dry-run",
        "gc commit-graph has no `--dry-run` yet (step 6)",
    ),
    (
        "gc.midx",
        ContractRule::DryRun,
        "s6 dry-run",
        "gc midx has no `--dry-run` yet (step 6)",
    ),
    (
        "gc.pack-refs",
        ContractRule::DryRun,
        "s6 dry-run",
        "gc pack-refs has no `--dry-run` yet (step 6)",
    ),
    (
        "gc.prune",
        ContractRule::DryRun,
        "s6 dry-run",
        "gc prune has no `--dry-run` yet (step 6)",
    ),
    (
        "gc.repack",
        ContractRule::DryRun,
        "s6 dry-run",
        "gc repack has no `--dry-run` yet (step 6)",
    ),
    (
        "auth.logout",
        ContractRule::DryRun,
        "s6 dry-run",
        "auth logout has no `--dry-run` yet (step 6)",
    ),
    (
        "clone",
        ContractRule::DryRun,
        "clone-remote",
        "clone has no `--dry-run` yet",
    ),
    (
        "init",
        ContractRule::DryRun,
        "workspace-extras",
        "init has no `--dry-run` yet",
    ),
    (
        "config",
        ContractRule::DryRun,
        "workspace-extras",
        "config set/unset has no `--dry-run` yet",
    ),
    (
        "reset",
        ContractRule::DryRun,
        "workspace-extras",
        "reset has no `--dry-run` yet",
    ),
    (
        "rm",
        ContractRule::DryRun,
        "workspace-extras",
        "rm has no `--dry-run` yet",
    ),
    (
        "mv",
        ContractRule::DryRun,
        "workspace-extras",
        "mv has no `--dry-run` yet",
    ),
    (
        "auth",
        ContractRule::DryRun,
        "s6 dry-run",
        "auth sign-in has no `--dry-run` yet (step 6)",
    ),
    (
        "hooks.run",
        ContractRule::DryRun,
        "workspace-extras",
        "hooks run has no `--dry-run` yet",
    ),
    (
        "notes.add",
        ContractRule::Force,
        "workspace-extras",
        "notes add uses long-only `--force` for replace semantics today",
    ),
    (
        "notes.remove",
        ContractRule::DryRun,
        "s5 dry-run",
        "notes remove has no `--dry-run` yet (step 5)",
    ),
    (
        "reflog.delete",
        ContractRule::DryRun,
        "inspection-extras",
        "reflog delete has no `--dry-run` yet",
    ),
    (
        "reflog.expire",
        ContractRule::DryRun,
        "inspection-extras",
        "reflog expire has no `--dry-run` yet",
    ),
    (
        "remote.add",
        ContractRule::DryRun,
        "clone-remote",
        "remote add has no `--dry-run` yet",
    ),
    (
        "stash",
        ContractRule::Message,
        "s3 message",
        "stash push accepts `-m` / `--message` globally but not `--message-file` yet",
    ),
    (
        "stash.push",
        ContractRule::Message,
        "s3 message",
        "stash push has `-m` / `--message` but not `--message-file` yet",
    ),
    (
        "update",
        ContractRule::DryRun,
        "workspace-extras",
        "update has no `--dry-run` yet",
    ),
    (
        "stash.apply",
        ContractRule::DryRun,
        "workspace-extras",
        "stash apply has no `--dry-run` yet",
    ),
    (
        "stash.pop",
        ContractRule::DryRun,
        "workspace-extras",
        "stash pop has no `--dry-run` yet",
    ),
    (
        "stash.drop",
        ContractRule::DryRun,
        "workspace-extras",
        "stash drop has no `--dry-run` yet",
    ),
    (
        "worktree.add",
        ContractRule::DryRun,
        "workspace-extras",
        "worktree add has no `--dry-run` yet",
    ),
    (
        "worktree.remove",
        ContractRule::DryRun,
        "workspace-extras",
        "worktree remove has no `--dry-run` yet",
    ),
    (
        "worktree.prune",
        ContractRule::DryRun,
        "workspace-extras",
        "worktree prune has no `--dry-run` yet",
    ),
    (
        "worktree.lock",
        ContractRule::DryRun,
        "workspace-extras",
        "worktree lock has no `--dry-run` yet",
    ),
    (
        "worktree.unlock",
        ContractRule::DryRun,
        "workspace-extras",
        "worktree unlock has no `--dry-run` yet",
    ),
    (
        "ignore.check",
        ContractRule::Paths,
        "s7 paths",
        "ignore check uses positional paths without the shared pathspec group yet",
    ),
    (
        "mv",
        ContractRule::Paths,
        "workspace-extras",
        "mv uses positional paths without `--pathspec-from-file` yet",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_command::build_cli_command;

    fn with_cli_root<R>(run: impl FnOnce(clap::Command) -> R + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        const STACK: usize = 16 * 1024 * 1024;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || run(build_cli_command()))
            .expect("spawn cli root builder")
            .join()
            .expect("join cli root builder")
    }

    /// Parent commands that run when no subcommand is given (same set as JSON schema keys).
    fn parent_runs_without_subcommand(full: &str) -> bool {
        matches!(
            full,
            "remote" | "stash" | "auth" | "hooks" | "reflog" | "gc" | "notes"
        )
    }

    fn collect_leaf_paths(root: &clap::Command) -> Vec<String> {
        let mut paths = Vec::new();
        for sub in root.get_subcommands().filter(|c| c.get_name() != "help") {
            collect_leaves(sub, None, &mut paths);
        }
        paths.sort();
        paths.dedup();
        paths
    }

    fn collect_leaves(cmd: &clap::Command, prefix: Option<&str>, out: &mut Vec<String>) {
        let name = cmd.get_name();
        if name == "help" {
            return;
        }
        let full = match prefix {
            None => name.to_string(),
            Some(p) => format!("{p}.{name}"),
        };
        let subs: Vec<_> = cmd
            .get_subcommands()
            .filter(|s| s.get_name() != "help")
            .collect();

        if subs.is_empty() {
            out.push(full);
            return;
        }

        if parent_runs_without_subcommand(&full) {
            out.push(full.clone());
        }

        for sub in subs {
            collect_leaves(sub, Some(&full), out);
        }
    }

    fn resolve_command<'a>(root: &'a clap::Command, path: &str) -> Option<&'a clap::Command> {
        let mut cmd = root;
        for segment in path.split('.') {
            cmd = cmd.get_subcommands().find(|c| c.get_name() == segment)?;
        }
        Some(cmd)
    }

    fn is_exception(path: &str, rule: ContractRule) -> bool {
        EXCEPTIONS
            .iter()
            .any(|(p, r, _, _)| *p == path && *r == rule)
    }

    fn limit_flag_ok(cmd: &clap::Command) -> bool {
        cmd.get_arguments().any(|arg| n_arg_is_limit(arg))
    }

    fn n_arg_is_limit(arg: &clap::Arg) -> bool {
        if arg.get_short() != Some('n') {
            return false;
        }
        match arg.get_long() {
            Some("limit") => true,
            Some("max-count") => arg_has_limit_alias(arg),
            _ => false,
        }
    }

    fn arg_has_limit_alias(arg: &clap::Arg) -> bool {
        arg.get_aliases()
            .map(|aliases| aliases.into_iter().any(|alias| alias == "limit"))
            .unwrap_or(false)
    }

    fn n_only_means_limit(cmd: &clap::Command) -> bool {
        cmd.get_arguments()
            .filter(|a| a.get_short() == Some('n'))
            .all(n_arg_is_limit)
    }

    fn has_long_only_dry_run(cmd: &clap::Command) -> bool {
        cmd.get_arguments()
            .any(|arg| arg.get_long() == Some("dry-run") && arg.get_short().is_none())
    }

    fn message_flag(cmd: &clap::Command) -> Option<&clap::Arg> {
        cmd.get_arguments()
            .find(|arg| arg.get_short() == Some('m') && arg.get_long() == Some("message"))
    }

    fn message_file_flag(cmd: &clap::Command) -> Option<&clap::Arg> {
        cmd.get_arguments()
            .find(|arg| arg.get_short() == Some('F') && arg.get_long() == Some("message-file"))
    }

    fn has_message_input(cmd: &clap::Command) -> bool {
        match (message_flag(cmd), message_file_flag(cmd)) {
            (Some(message), Some(file)) => cmd
                .get_arg_conflicts_with(message)
                .iter()
                .any(|conflict| conflict.get_id() == file.get_id()),
            _ => false,
        }
    }

    fn has_trailing_path_operand(cmd: &clap::Command) -> bool {
        cmd.get_arguments().any(|arg| {
            if !arg.is_positional() {
                return false;
            }
            arg.get_value_names()
                .is_some_and(|names| names.iter().any(|n| *n == "PATH"))
        })
    }

    fn has_pathspec_group(cmd: &clap::Command) -> bool {
        let has_from_file = cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("pathspec-from-file"));
        let has_nul = cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("pathspec-file-nul"));
        has_from_file && has_nul && has_trailing_path_operand(cmd)
    }

    fn force_flags_ok(cmd: &clap::Command) -> bool {
        cmd.get_arguments()
            .filter(|arg| arg.get_long() == Some("force"))
            .all(|arg| arg.get_short() == Some('f'))
    }

    fn check_n_short_is_limit_only(path: &str, cmd: &clap::Command) -> Option<String> {
        if n_only_means_limit(cmd) || is_exception(path, ContractRule::Limit) {
            return None;
        }
        Some(format!(
            "{path}: `-n` must mean limit on every command; rename other `-n` flags or add an EXCEPTIONS row (owner s4 limit)"
        ))
    }

    fn check_rule(path: &str, kind: CommandKind, cmd: &clap::Command) -> Option<String> {
        if kind == CommandKind::List {
            if !limit_flag_ok(cmd) && !is_exception(path, ContractRule::Limit) {
                return Some(format!(
                    "{path}: list commands need `-n` with long name `limit` (or an alias); \
                     add the flag or add a row to EXCEPTIONS in cli_contract.rs (owner s4 limit)"
                ));
            }
        }

        if kind == CommandKind::Mutation && path != "clean" {
            if !has_long_only_dry_run(cmd) && !is_exception(path, ContractRule::DryRun) {
                return Some(format!(
                    "{path}: mutation commands need long-only `--dry-run`; \
                     add the flag or add an EXCEPTIONS row in cli_contract.rs (see plan step s5/s6)"
                ));
            }
        }

        None
    }

    fn check_message(path: &str, takes_message: bool, cmd: &clap::Command) -> Option<String> {
        if !takes_message {
            return None;
        }
        if has_message_input(cmd) || is_exception(path, ContractRule::Message) {
            return None;
        }
        Some(format!(
            "{path}: commands with takes_message need `-m`/`--message` and `-F`/`--message-file` \
             (conflicting); add both or add an EXCEPTIONS row (owner s3 message)"
        ))
    }

    fn check_paths(path: &str, takes_paths: bool, cmd: &clap::Command) -> Option<String> {
        if !takes_paths {
            return None;
        }
        if has_pathspec_group(cmd) || is_exception(path, ContractRule::Paths) {
            return None;
        }
        Some(format!(
            "{path}: commands with takes_paths need trailing `-- <paths>` plus \
             `--pathspec-from-file` and `--pathspec-file-nul`; add the group or an EXCEPTIONS row (owner s7 paths)"
        ))
    }

    fn check_force(path: &str, cmd: &clap::Command) -> Option<String> {
        if force_flags_ok(cmd) || is_exception(path, ContractRule::Force) {
            return None;
        }
        Some(format!(
            "{path}: every `--force` must expose short `-f` unless documented in EXCEPTIONS"
        ))
    }

    fn convention_problems(root: &clap::Command) -> Vec<String> {
        convention_problems_for_paths(root, &[])
    }

    fn convention_problems_for_paths(root: &clap::Command, extra_paths: &[&str]) -> Vec<String> {
        let mut paths = collect_leaf_paths(root);
        for extra in extra_paths {
            paths.push((*extra).to_owned());
        }
        paths.sort();
        paths.dedup();
        let classified: BTreeMap<&str, CommandContract> = COMMAND_CONTRACTS
            .iter()
            .map(|(path, spec)| (*path, *spec))
            .collect();

        let mut problems = Vec::new();

        for path in &paths {
            if !classified.contains_key(path.as_str()) {
                problems.push(format!(
                    "{path}: unclassified — add a row to COMMAND_CONTRACTS in cli_contract.rs"
                ));
            }
        }
        for path in classified.keys() {
            if !paths.contains(&(*path).to_owned()) {
                problems.push(format!(
                    "{path}: classified but not in the clap tree — remove or rename the COMMAND_CONTRACTS row"
                ));
            }
        }

        for path in &paths {
            let Some(spec) = classified.get(path.as_str()) else {
                continue;
            };
            let Some(cmd) = resolve_command(root, path) else {
                problems.push(format!("{path}: could not resolve clap command"));
                continue;
            };
            if let Some(msg) = check_n_short_is_limit_only(path, cmd) {
                problems.push(msg);
            }
            if let Some(msg) = check_rule(path, spec.kind, cmd) {
                problems.push(msg);
            }
            if let Some(msg) = check_message(path, spec.takes_message, cmd) {
                problems.push(msg);
            }
            if let Some(msg) = check_paths(path, spec.takes_paths, cmd) {
                problems.push(msg);
            }
            if let Some(msg) = check_force(path, cmd) {
                problems.push(msg);
            }
        }

        if paths.len() != COMMAND_CONTRACTS.len() {
            problems.push(format!(
                "classifier has {} entries but clap has {} leaf paths — add or remove COMMAND_CONTRACTS rows",
                COMMAND_CONTRACTS.len(),
                paths.len()
            ));
        }

        problems
    }

    #[test]
    fn cli_contract_conventions() {
        with_cli_root(|root| {
            let problems = convention_problems(&root);
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn cli_contract_exceptions_are_live() {
        with_cli_root(|root| {
            let paths: BTreeSet<String> = collect_leaf_paths(&root).into_iter().collect();
            let classified: BTreeMap<&str, CommandContract> = COMMAND_CONTRACTS
                .iter()
                .map(|(path, spec)| (*path, *spec))
                .collect();

            let mut problems = Vec::new();
            for (path, rule, owner, reason) in EXCEPTIONS {
                if !paths.contains(&(*path).to_owned()) {
                    problems.push(format!(
                        "EXCEPTIONS row ({path}, {rule:?}): path not in clap tree — remove the exception"
                    ));
                    continue;
                }
                let Some(spec) = classified.get(path) else {
                    problems.push(format!(
                        "EXCEPTIONS row ({path}, {rule:?}): path not classified — fix COMMAND_CONTRACTS first"
                    ));
                    continue;
                };
                let Some(cmd) = resolve_command(&root, path) else {
                    continue;
                };

                let still_violates = match rule {
                    ContractRule::Limit => {
                        spec.kind == CommandKind::List
                            && (!limit_flag_ok(cmd) || !n_only_means_limit(cmd))
                    }
                    ContractRule::DryRun => {
                        spec.kind == CommandKind::Mutation
                            && *path != "clean"
                            && !has_long_only_dry_run(cmd)
                    }
                    ContractRule::Message => spec.takes_message && !has_message_input(cmd),
                    ContractRule::Paths => spec.takes_paths && !has_pathspec_group(cmd),
                    ContractRule::Force => !force_flags_ok(cmd),
                };

                if !still_violates {
                    problems.push(format!(
                        "EXCEPTIONS ({path}, {rule:?}, owner {owner}): violation is fixed — \
                         remove this row ({reason}) and rely on the convention test"
                    ));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        });
    }

    #[test]
    fn cli_contract_conventions_fail_when_tag_loses_dry_run() {
        let tag_without_dry_run =
            clap::Command::new("tag").arg(clap::Arg::new("limit").long("limit"));
        let msg = check_rule("tag", CommandKind::Mutation, &tag_without_dry_run)
            .expect("mutation without --dry-run should fail the contract");
        assert!(
            msg.contains("tag") && msg.contains("dry-run"),
            "expected tag and dry-run in failure message, got: {msg}"
        );
    }

    #[test]
    fn cli_contract_rejects_n_when_not_limit_on_any_kind() {
        let cmd = clap::Command::new("sample").arg(
            clap::Arg::new("number")
                .short('n')
                .long("number")
                .action(clap::ArgAction::SetTrue),
        );
        let msg = check_n_short_is_limit_only("sample", &cmd).expect("bad -n should fail");
        assert!(
            msg.contains("sample") && msg.contains("`-n` must mean limit"),
            "{msg}"
        );
    }

    #[test]
    fn cli_contract_rejects_message_flags_without_conflict() {
        let cmd = clap::Command::new("sample")
            .arg(clap::Arg::new("message").short('m').long("message"))
            .arg(
                clap::Arg::new("message-file")
                    .short('F')
                    .long("message-file"),
            );
        let msg = check_message("sample", true, &cmd).expect("missing conflict should fail");
        assert!(msg.contains("sample") && msg.contains("message"), "{msg}");
    }

    #[test]
    fn cli_contract_rejects_pathspec_without_trailing_paths() {
        let cmd = clap::Command::new("sample")
            .arg(clap::Arg::new("pathspec-from-file").long("pathspec-from-file"))
            .arg(clap::Arg::new("pathspec-file-nul").long("pathspec-file-nul"));
        let msg = check_paths("sample", true, &cmd).expect("missing PATH operand should fail");
        assert!(msg.contains("sample") && msg.contains("pathspec"), "{msg}");
    }

    #[test]
    fn cli_contract_unclassified_path_is_actionable() {
        with_cli_root(|root| {
            let problems = convention_problems_for_paths(&root, &["brand.new.command"]);
            assert!(
                problems.iter().any(|p| {
                    p.contains("brand.new.command: unclassified") && p.contains("COMMAND_CONTRACTS")
                }),
                "expected unclassified diagnostic, got: {problems:?}"
            );
        });
    }
}
