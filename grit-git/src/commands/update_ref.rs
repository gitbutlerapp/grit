//! `grit update-ref` — update the object name stored in a ref safely.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use time::OffsetDateTime;

use grit_lib::check_ref_format::{check_refname_format, refname_is_safe, RefNameOptions};
use grit_lib::config::{parse_bool, ConfigSet};
use grit_lib::error::Error as GritError;
use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    append_reflog, delete_ref, read_head, read_ref_file, read_symbolic_ref, resolve_ref,
    should_autocreate_reflog_for_mode, verify_refname_available_for_create, write_ref,
};
use grit_lib::repo::Repository;
use std::collections::{BTreeSet, HashSet};

use crate::ref_transaction_hooks::{
    run_ref_transaction_aborted, run_ref_transaction_committed, run_ref_transaction_prepare,
    HookUpdate,
};

/// Arguments for `grit update-ref`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Delete the ref (use with --stdin or as: update-ref -d <ref>).
    #[arg(short = 'd')]
    pub delete: bool,

    /// Do not dereference symbolic refs.
    #[arg(long = "no-deref")]
    pub no_deref: bool,

    /// Read commands from stdin.
    #[arg(long)]
    pub stdin: bool,

    /// Use NUL as line terminator.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Create a reflog for this ref.
    #[arg(long = "create-reflog")]
    pub create_reflog: bool,

    /// Do not create a reflog unless `--create-reflog` or `-m` is given.
    #[arg(long = "no-create-reflog")]
    pub no_create_reflog: bool,

    /// Log message for reflog.
    #[arg(short = 'm', long = "message")]
    pub log_message: Option<String>,

    /// The reference to update.
    pub refname: Option<String>,

    /// The new value (SHA-1 or ref name).
    pub new_value: Option<String>,

    /// The expected old value (SHA-1).
    pub old_value: Option<String>,
}

/// Run `grit update-ref`.
pub fn run(mut args: Args) -> Result<()> {
    if args.null_terminated && !args.stdin {
        bail!("-z requires --stdin");
    }

    let repo = Repository::discover(None).context("not a git repository")?;

    if args.stdin {
        return run_batch(&repo, &args);
    }

    let refname = args
        .refname
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("ref name required"))?;
    if args.delete
        && !args.no_deref
        && read_symbolic_ref(&repo.git_dir, refname)
            .ok()
            .flatten()
            .as_deref()
            == Some(refname)
    {
        return Err(anyhow::Error::from(GritError::Message(format!(
            "error: multiple updates for '{refname}' (including one via symref '{refname}') are not allowed"
        ))));
    }
    let target_refname = effective_refname(&repo, refname, args.no_deref)?;

    if args.delete {
        validate_delete_refname(&target_refname)?;
        // `git update-ref -d <ref> [<old>]` — the optional old OID is the first "value"
        // positional; clap maps it to `new_value`. Prefer explicit `--` old if given.
        let old_from_positional = args.new_value.take();
        let expected =
            parse_old_expectation(args.old_value.as_deref().or(old_from_positional.as_deref()))?;
        if let Some(exp) = expected {
            verify_expected_old(&repo, refname, &target_refname, args.no_deref, exp)?;
        }

        let old_oid_for_reflog =
            resolve_ref(&repo.git_dir, &target_refname).unwrap_or_else(|_| zero_oid());

        let hook_update = HookUpdate {
            old_value: hook_old_value_from_expectation(expected),
            new_value: zero_oid_hex().to_owned(),
            refname: target_refname.clone(),
            deletes_ref: true,
        };
        run_ref_transaction_prepare(&repo, &[hook_update.clone()])?;
        delete_ref(&repo.git_dir, &target_refname).context("deleting ref")?;
        run_ref_transaction_committed(&repo, &[hook_update]);

        if let Some(msg) = args.log_message.as_deref() {
            let _ = append_reflog(
                &repo.git_dir,
                &reflog_ref_for_delete(&repo.git_dir, refname, &target_refname),
                &old_oid_for_reflog,
                &zero_oid(),
                &resolve_reflog_identity(&repo),
                msg,
                update_reflog_force_create(&repo, &args, &target_refname, msg),
            );
        }
        return Ok(());
    }

    let new_str = args
        .new_value
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("new value required"))?;
    validate_update_refname(&target_refname)?;
    let new_oid: ObjectId = resolve_oid_or_ref(&repo, new_str)?;

    let expected = parse_old_expectation(args.old_value.as_deref())?;
    ensure_lockable_ref_path(&repo, refname, &target_refname, expected.is_none())?;
    if let Some(expected) = expected {
        verify_expected_old(&repo, refname, &target_refname, args.no_deref, expected)?;
    }

    let old_oid_for_reflog =
        resolve_ref(&repo.git_dir, &target_refname).unwrap_or_else(|_| zero_oid());
    let hook_update = HookUpdate {
        old_value: hook_old_value_from_expectation(expected),
        new_value: new_oid.to_hex(),
        refname: target_refname.clone(),
        deletes_ref: new_oid == zero_oid(),
    };

    if new_oid == zero_oid() {
        run_ref_transaction_prepare(&repo, &[hook_update.clone()])?;
        delete_ref(&repo.git_dir, &target_refname).context("deleting ref")?;
        run_ref_transaction_committed(&repo, &[hook_update]);
        if let Some(msg) = args.log_message.as_deref() {
            let _ = append_reflog(
                &repo.git_dir,
                &reflog_ref_for_delete(&repo.git_dir, refname, &target_refname),
                &old_oid_for_reflog,
                &zero_oid(),
                &resolve_reflog_identity(&repo),
                msg,
                update_reflog_force_create(&repo, &args, &target_refname, msg),
            );
        }
        return Ok(());
    }

    let msg = args.log_message.as_deref().unwrap_or("");
    let updates_symbolic_storage =
        args.no_deref && read_symbolic_ref_no_deref(&repo, refname)?.is_some();
    if old_oid_for_reflog == new_oid
        && !updates_symbolic_storage
        && msg.is_empty()
        && !args.create_reflog
    {
        return Ok(());
    }
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) && !msg.is_empty() {
        let opts = grit_lib::reftable::read_write_options(&repo.git_dir);
        if opts.block_size > 0 && msg.len() > opts.block_size as usize {
            return Err(anyhow::Error::from(GritError::Message(format!(
                "fatal: update_ref failed for ref '{target_refname}': reftable: transaction failure: entry too large"
            ))));
        }
    }

    if should_write_update_reflog(&repo, &args, &target_refname, msg) {
        let identity = resolve_reflog_identity(&repo);
        run_ref_transaction_prepare(&repo, &[hook_update.clone()])?;
        if grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
            grit_lib::reftable::reftable_write_ref(
                &repo.git_dir,
                &target_refname,
                &new_oid,
                Some(&identity),
                Some(msg),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("writing ref")?;
        } else {
            write_ref(&repo.git_dir, &target_refname, &new_oid).context("writing ref")?;
            let _ = append_reflog(
                &repo.git_dir,
                &target_refname,
                &old_oid_for_reflog,
                &new_oid,
                &identity,
                msg,
                update_reflog_force_create(&repo, &args, &target_refname, msg),
            );
        }
        run_ref_transaction_committed(&repo, &[hook_update]);
    } else {
        run_ref_transaction_prepare(&repo, &[hook_update.clone()])?;
        write_ref(&repo.git_dir, &target_refname, &new_oid).context("writing ref")?;
        run_ref_transaction_committed(&repo, &[hook_update]);
    }

    maybe_emit_reference_fsync_counter(4);
    Ok(())
}

fn maybe_emit_reference_fsync_counter(count: u64) {
    if std::env::var("GIT_TEST_FSYNC").ok().as_deref() != Some("true") {
        return;
    }
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    let _ = crate::trace2_write_json_counter_line(&path, "fsync", "hardware-flush", count);
}

fn should_write_update_reflog(
    repo: &Repository,
    args: &Args,
    reflog_refname: &str,
    msg: &str,
) -> bool {
    if args.no_create_reflog {
        return args.create_reflog || !msg.is_empty();
    }
    if args.create_reflog || !msg.is_empty() {
        return true;
    }
    if grit_lib::reflog::reflog_exists(&repo.git_dir, reflog_refname) {
        return true;
    }
    should_autocreate_reflog_for_mode(
        reflog_refname,
        update_log_refs_mode(repo)
            .unwrap_or_else(|| grit_lib::refs::effective_log_refs_config(&repo.git_dir)),
    )
}

fn update_reflog_force_create(
    repo: &Repository,
    args: &Args,
    reflog_refname: &str,
    msg: &str,
) -> bool {
    args.create_reflog
        || !msg.is_empty()
        || update_log_refs_mode(repo)
            .is_some_and(|mode| should_autocreate_reflog_for_mode(reflog_refname, mode))
}

fn update_log_refs_mode(repo: &Repository) -> Option<grit_lib::refs::LogRefsConfig> {
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .map(|config| config.effective_log_refs_config(&repo.git_dir))
}

fn reflog_ref_for_delete(git_dir: &std::path::Path, user_arg: &str, leaf_ref: &str) -> String {
    if user_arg.eq_ignore_ascii_case("HEAD") {
        return "HEAD".to_owned();
    }
    match read_head(git_dir).ok().flatten() {
        Some(t) if t == leaf_ref => "HEAD".to_owned(),
        _ => leaf_ref.to_owned(),
    }
}

fn take_batch_no_deref(args: &Args, pending_option: &mut bool) -> bool {
    let nd = args.no_deref || *pending_option;
    *pending_option = false;
    nd
}

fn resolve_oid_or_ref(repo: &Repository, s: &str) -> Result<ObjectId> {
    if let Ok(oid) = s.parse::<ObjectId>() {
        return Ok(oid);
    }
    // Full ref names must be resolved via the ref store first; `rev_parse` can
    // treat `refs/...` as ambiguous against worktree paths.
    if s.starts_with("refs/") {
        if let Ok(oid) = resolve_ref(&repo.git_dir, s) {
            return Ok(oid);
        }
    }
    if let Ok(oid) = grit_lib::rev_parse::resolve_revision(repo, s) {
        return Ok(oid);
    }
    if let Ok(oid) = resolve_ref(&repo.git_dir, s) {
        return Ok(oid);
    }
    // Try DWIM-style resolution: refs/heads/<s>, refs/tags/<s>, refs/remotes/<s>
    for prefix in &["refs/heads/", "refs/tags/", "refs/remotes/", "refs/notes/"] {
        let full = format!("{prefix}{s}");
        if let Ok(oid) = resolve_ref(&repo.git_dir, &full) {
            return Ok(oid);
        }
    }
    bail!("not a valid object name: '{s}'")
}

#[derive(Clone, Copy)]
enum OldExpectation {
    MustNotExist,
    MustEqual(ObjectId),
}

#[derive(Clone)]
enum SymrefOldExpectation {
    MustNotExist,
    MustTarget(String),
    MustOid(ObjectId),
}

#[derive(Clone)]
enum BatchOp {
    UpdateOid {
        refname: String,
        new_oid: ObjectId,
        expected_old: Option<OldExpectation>,
    },
    CreateOid {
        refname: String,
        new_oid: ObjectId,
    },
    DeleteOid {
        refname: String,
        expected_old: Option<OldExpectation>,
    },
    VerifyOid {
        refname: String,
        expected_old: Option<OldExpectation>,
    },
    UpdateSymref {
        refname: String,
        new_target: String,
        expected_old: Option<SymrefOldExpectation>,
    },
    CreateSymref {
        refname: String,
        new_target: String,
    },
    DeleteSymref {
        refname: String,
        expected_old: Option<SymrefOldExpectation>,
    },
    VerifySymref {
        refname: String,
        expected_old: SymrefOldExpectation,
    },
}

fn stdin_lines_explicit_transaction(text: &str) -> bool {
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"start") {
            return true;
        }
    }
    false
}

fn stdin_chunks_explicit_transaction(input: &[u8]) -> Result<bool> {
    for chunk in input.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(chunk).context("invalid utf-8 in stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"start") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Process `--stdin` batch commands.
fn run_batch(repo: &Repository, args: &Args) -> Result<()> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    if args.null_terminated {
        return run_batch_nul(repo, args, &input);
    }

    let text = String::from_utf8_lossy(&input);
    if !stdin_lines_explicit_transaction(&text) {
        return run_implicit_stdin_batch(repo, args, &text);
    }

    let mut transaction_active = false;
    let mut transaction_prepared = false;
    let mut staged: Vec<(bool, BatchOp)> = Vec::new();
    let mut pending_option_no_deref = false;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.chars().next().is_some_and(|c| c.is_whitespace()) {
            bail!("whitespace before command: {line}");
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        process_batch_command(
            repo,
            args,
            &parts,
            line,
            &mut transaction_active,
            &mut transaction_prepared,
            &mut staged,
            &mut pending_option_no_deref,
            false,
        )?;
    }

    if transaction_active {
        let hook_updates = hook_updates_for_ops(&staged)?;
        if !hook_updates.is_empty() {
            run_ref_transaction_aborted(repo, &hook_updates);
        }
    }

    Ok(())
}

fn run_implicit_stdin_batch(repo: &Repository, args: &Args, text: &str) -> Result<()> {
    // Only mutating commands claim a ref for the duration of the transaction;
    // two mutations targeting the same ref are rejected ("multiple updates for
    // ref"). A `verify` is a read-only precondition check, so it may coexist
    // with a later mutation of the same ref (e.g. `verify X <old>` followed by
    // `update X <new> <old>`) — the verify simply runs first.
    let mut seen_refs = HashSet::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if matches!(parts.first().copied(), Some("update" | "create" | "delete")) {
            if let Some(refname) = parts.get(1) {
                if !seen_refs.insert((*refname).to_owned()) {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: multiple updates for ref '{refname}' not allowed"
                    ))));
                }
            }
        }
    }

    let mut staged: Vec<(bool, BatchOp)> = Vec::new();
    let mut pending_option_no_deref = false;
    let mut transaction_active = false;
    let mut transaction_prepared = false;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.chars().next().is_some_and(|c| c.is_whitespace()) {
            bail!("whitespace before command: {line}");
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        process_batch_command(
            repo,
            args,
            &parts,
            line,
            &mut transaction_active,
            &mut transaction_prepared,
            &mut staged,
            &mut pending_option_no_deref,
            true,
        )?;
    }

    if staged.is_empty() {
        return Ok(());
    }
    commit_batch_staged(repo, args, &staged)
}

fn storage_refname_for_op(repo: &Repository, no_deref: bool, op: &BatchOp) -> Result<String> {
    Ok(match op {
        BatchOp::UpdateOid { refname, .. }
        | BatchOp::CreateOid { refname, .. }
        | BatchOp::DeleteOid { refname, .. }
        | BatchOp::VerifyOid { refname, .. } => effective_refname(repo, refname, no_deref)?,
        BatchOp::UpdateSymref { refname, .. }
        | BatchOp::CreateSymref { refname, .. }
        | BatchOp::DeleteSymref { refname, .. }
        | BatchOp::VerifySymref { refname, .. } => refname.clone(),
    })
}

fn build_batch_extras(repo: &Repository, staged: &[(bool, BatchOp)]) -> Result<BTreeSet<String>> {
    let mut extras = BTreeSet::new();
    for (nd, op) in staged {
        extras.insert(storage_refname_for_op(repo, *nd, op)?);
    }
    Ok(extras)
}

fn verify_refname_available_for_batch_create(
    repo: &Repository,
    staged: &[(bool, BatchOp)],
    target_refname: &str,
    display_refname: &str,
) -> Result<()> {
    let extras = build_batch_extras(repo, staged)?;
    let skip = HashSet::<String>::new();
    match verify_refname_available_for_create(&repo.git_dir, target_refname, &extras, &skip) {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow::Error::from(GritError::Message(format!(
            "fatal: cannot lock ref '{display_refname}': {}",
            e.lock_message_suffix()
        )))),
    }
}

fn verify_batch_staged(repo: &Repository, staged: &[(bool, BatchOp)]) -> Result<()> {
    for (nd, op) in staged {
        match op {
            BatchOp::CreateOid { refname, .. } => {
                let target = effective_refname(repo, refname, *nd)?;
                verify_refname_available_for_batch_create(repo, staged, &target, refname)?;
            }
            BatchOp::CreateSymref { refname, .. } => {
                verify_refname_available_for_batch_create(repo, staged, refname, refname)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn apply_batch_staged(repo: &Repository, args: &Args, staged: &[(bool, BatchOp)]) -> Result<()> {
    for (nd, op) in staged {
        apply_batch_op(repo, args, *nd, op.clone())?;
    }
    Ok(())
}

fn commit_batch_staged(repo: &Repository, args: &Args, staged: &[(bool, BatchOp)]) -> Result<()> {
    verify_batch_staged(repo, staged)?;
    let hook_updates = hook_updates_for_ops(staged)?;
    run_ref_transaction_prepare(repo, &hook_updates)?;
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
        apply_reftable_batch_staged(repo, args, staged)?;
    } else {
        apply_batch_staged(repo, args, staged)?;
    }
    run_ref_transaction_committed(repo, &hook_updates);
    Ok(())
}

fn apply_reftable_batch_staged(
    repo: &Repository,
    args: &Args,
    staged: &[(bool, BatchOp)],
) -> Result<()> {
    let mut updates = Vec::new();
    for (no_deref, op) in staged {
        match op {
            BatchOp::UpdateOid {
                refname,
                new_oid,
                expected_old,
            } => {
                let target_refname = effective_refname(repo, refname, *no_deref)?;
                ensure_lockable_ref_path(repo, refname, &target_refname, expected_old.is_none())?;
                if let Some(expected) = *expected_old {
                    verify_expected_old(repo, refname, &target_refname, *no_deref, expected)?;
                }
                let old_oid =
                    resolve_ref(&repo.git_dir, &target_refname).unwrap_or_else(|_| zero_oid());
                let msg = args.log_message.as_deref().unwrap_or("");
                let log = if should_write_update_reflog(repo, args, &target_refname, msg) {
                    let (name, email, time_seconds, tz_offset) =
                        reftable_log_identity_parts(&resolve_reflog_identity(repo));
                    Some(grit_lib::reftable::LogRecord {
                        refname: target_refname.clone(),
                        update_index: 0,
                        old_id: old_oid,
                        new_id: *new_oid,
                        name,
                        email,
                        time_seconds,
                        tz_offset,
                        message: msg.to_owned(),
                    })
                } else {
                    None
                };
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: target_refname,
                    value: grit_lib::reftable::RefValue::Val1(*new_oid),
                    log,
                });
            }
            BatchOp::CreateOid { refname, new_oid } => {
                let target_refname = effective_refname(repo, refname, *no_deref)?;
                ensure_lockable_ref_path(repo, refname, &target_refname, true)?;
                if resolve_ref(&repo.git_dir, &target_refname).is_ok() {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': reference already exists"
                    ))));
                }
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: target_refname,
                    value: grit_lib::reftable::RefValue::Val1(*new_oid),
                    log: None,
                });
            }
            BatchOp::DeleteOid {
                refname,
                expected_old,
            } => {
                let target_refname = effective_refname(repo, refname, *no_deref)?;
                if let Some(expected) = *expected_old {
                    verify_expected_old(repo, refname, &target_refname, *no_deref, expected)?;
                }
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: target_refname,
                    value: grit_lib::reftable::RefValue::Deletion,
                    log: None,
                });
            }
            BatchOp::VerifyOid {
                refname,
                expected_old,
            } => {
                let target_refname = effective_refname(repo, refname, *no_deref)?;
                if let Some(expected) = *expected_old {
                    verify_expected_old(repo, refname, &target_refname, *no_deref, expected)?;
                } else if resolve_ref(&repo.git_dir, &target_refname).is_err() {
                    bail!("ref '{target_refname}' does not exist");
                }
            }
            BatchOp::UpdateSymref {
                refname,
                new_target,
                expected_old,
            } => {
                if let Some(expected) = expected_old.clone() {
                    verify_symref_expected_old(repo, refname, expected)?;
                }
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: refname.clone(),
                    value: grit_lib::reftable::RefValue::Symref(new_target.clone()),
                    log: None,
                });
            }
            BatchOp::CreateSymref {
                refname,
                new_target,
            } => {
                if ref_exists_no_deref(repo, refname)? {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': reference already exists"
                    ))));
                }
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: refname.clone(),
                    value: grit_lib::reftable::RefValue::Symref(new_target.clone()),
                    log: None,
                });
            }
            BatchOp::DeleteSymref {
                refname,
                expected_old,
            } => {
                if let Some(expected) = expected_old.clone() {
                    verify_symref_expected_old(repo, refname, expected)?;
                }
                updates.push(grit_lib::reftable::ReftableTransactionUpdate {
                    refname: refname.clone(),
                    value: grit_lib::reftable::RefValue::Deletion,
                    log: None,
                });
            }
            BatchOp::VerifySymref {
                refname,
                expected_old,
            } => verify_symref_expected_old(repo, refname, expected_old.clone())?,
        }
    }

    grit_lib::reftable::reftable_write_transaction(&repo.git_dir, updates)
        .map_err(|err| anyhow::anyhow!("{err}"))
}

fn queue_or_apply(
    repo: &Repository,
    args: &Args,
    no_deref: bool,
    transaction_active: bool,
    staged: &mut Vec<(bool, BatchOp)>,
    op: BatchOp,
    implicit_one_shot: bool,
) -> Result<()> {
    if transaction_active {
        staged.push((no_deref, op));
        Ok(())
    } else if implicit_one_shot {
        staged.push((no_deref, op));
        Ok(())
    } else {
        commit_batch_staged(repo, args, &[(no_deref, op)])
    }
}

#[allow(clippy::too_many_arguments)]
fn process_batch_command(
    repo: &Repository,
    args: &Args,
    parts: &[&str],
    raw_line: &str,
    transaction_active: &mut bool,
    transaction_prepared: &mut bool,
    staged: &mut Vec<(bool, BatchOp)>,
    pending_option_no_deref: &mut bool,
    implicit_one_shot: bool,
) -> Result<()> {
    validate_batch_refname(parts[0], raw_line, args.null_terminated)?;
    match parts[0] {
        "update" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 3 {
                bail!("update requires ref and new-value");
            }
            let op = BatchOp::UpdateOid {
                refname: parts[1].to_owned(),
                new_oid: resolve_oid_or_ref(repo, parts[2])?,
                expected_old: parse_old_expectation(parts.get(3).copied())?,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "create" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 3 {
                bail!("create requires ref and new-value");
            }
            let op = BatchOp::CreateOid {
                refname: parts[1].to_owned(),
                new_oid: resolve_oid_or_ref(repo, parts[2])?,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "delete" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 2 {
                bail!("delete requires ref");
            }
            let op = BatchOp::DeleteOid {
                refname: parts[1].to_owned(),
                expected_old: parse_old_expectation(parts.get(2).copied())?,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "verify" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 2 {
                bail!("verify requires ref");
            }
            let op = BatchOp::VerifyOid {
                refname: parts[1].to_owned(),
                expected_old: parse_old_expectation(parts.get(2).copied())?,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "symref-update" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 3 {
                bail!("symref-update requires ref and new-target");
            }
            let expected_old = match parts.get(3).copied() {
                None => None,
                Some("ref") => {
                    let Some(target) = parts.get(4) else {
                        bail!("symref-update requires old-target after 'ref'");
                    };
                    Some(SymrefOldExpectation::MustTarget((*target).to_owned()))
                }
                Some("oid") => {
                    let Some(oid) = parts.get(4) else {
                        bail!("symref-update requires old-oid after 'oid'");
                    };
                    let parsed = oid.parse::<ObjectId>().context("invalid old-value OID")?;
                    Some(SymrefOldExpectation::MustOid(parsed))
                }
                Some(other) => bail!("symref-update expected 'ref' or 'oid', got '{other}'"),
            };
            let op = BatchOp::UpdateSymref {
                refname: parts[1].to_owned(),
                new_target: parts[2].to_owned(),
                expected_old,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "symref-create" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 3 {
                bail!("symref-create requires ref and new-target");
            }
            let op = BatchOp::CreateSymref {
                refname: parts[1].to_owned(),
                new_target: parts[2].to_owned(),
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "symref-delete" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if parts.len() < 2 {
                bail!("symref-delete requires ref");
            }
            let expected_old = parts
                .get(2)
                .map(|target| SymrefOldExpectation::MustTarget((*target).to_owned()));
            let op = BatchOp::DeleteSymref {
                refname: parts[1].to_owned(),
                expected_old,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "symref-verify" => {
            let nd = take_batch_no_deref(args, pending_option_no_deref);
            if !nd {
                bail!("symref-verify can only be used in no-deref mode");
            }
            if parts.len() < 2 {
                bail!("symref-verify requires ref");
            }
            let expected_old = parts
                .get(2)
                .map(|target| SymrefOldExpectation::MustTarget((*target).to_owned()))
                .unwrap_or(SymrefOldExpectation::MustNotExist);
            let op = BatchOp::VerifySymref {
                refname: parts[1].to_owned(),
                expected_old,
            };
            queue_or_apply(
                repo,
                args,
                nd,
                *transaction_active,
                staged,
                op,
                implicit_one_shot,
            )?;
        }
        "option" => {
            if parts.len() == 2 && parts[1] == "no-deref" {
                *pending_option_no_deref = true;
            } else {
                bail!("option unknown: {}", parts.get(1).copied().unwrap_or(""));
            }
        }
        "start" => {
            if *transaction_active {
                bail!("transaction already started");
            }
            *transaction_active = true;
            *transaction_prepared = false;
            staged.clear();
            println!("start: ok");
        }
        "prepare" => {
            if !*transaction_active {
                *transaction_active = true;
            }
            let hook_updates = hook_updates_for_ops(staged)?;
            run_ref_transaction_prepare(repo, &hook_updates)?;
            *transaction_prepared = true;
            println!("prepare: ok");
        }
        "commit" => {
            if !*transaction_active {
                bail!("no transaction started");
            }

            let drained: Vec<(bool, BatchOp)> = staged.drain(..).collect();
            let hook_updates = hook_updates_for_ops(&drained)?;
            verify_batch_staged(repo, &drained)?;
            if !*transaction_prepared {
                run_ref_transaction_prepare(repo, &hook_updates)?;
            }
            match apply_batch_staged(repo, args, &drained) {
                Ok(()) => {
                    run_ref_transaction_committed(repo, &hook_updates);
                }
                Err(e) => {
                    if !hook_updates.is_empty() {
                        run_ref_transaction_aborted(repo, &hook_updates);
                    }
                    return Err(e);
                }
            }
            *transaction_active = false;
            *transaction_prepared = false;
            println!("commit: ok");
        }
        "abort" => {
            if *transaction_active {
                let hook_updates = hook_updates_for_ops(staged)?;
                if !hook_updates.is_empty() {
                    run_ref_transaction_aborted(repo, &hook_updates);
                }
            }
            staged.clear();
            *transaction_active = false;
            *transaction_prepared = false;
            println!("abort: ok");
        }
        other => bail!("unknown batch command: {other}"),
    }
    Ok(())
}

/// Split `-z` stdin into logical command lines.
///
/// Grit treats `-z` purely as a *line terminator*: NUL replaces the newline
/// that separates whole commands, but the fields **within** a command are still
/// whitespace-separated (e.g. `create refs/heads/x <oid>\0`). This differs from
/// upstream `builtin/update-ref.c`, whose `-z` mode puts the command word, the
/// refname, and each value in their own NUL-terminated fields. Grit's own ported
/// suites (t10620, t11290) encode the line-terminator form, so we mirror that
/// here and reuse the same whitespace tokenizer as the non-`-z` path.
fn split_nul_lines(input: &[u8]) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for chunk in input.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(chunk).context("invalid utf-8 in stdin")?;
        lines.push(line.to_owned());
    }
    Ok(lines)
}

fn run_batch_nul(repo: &Repository, args: &Args, input: &[u8]) -> Result<()> {
    let lines = split_nul_lines(input)?;

    if !stdin_chunks_explicit_transaction(input)? {
        let mut staged: Vec<(bool, BatchOp)> = Vec::new();
        let mut pending_option_no_deref = false;
        let mut transaction_active = false;
        let mut transaction_prepared = false;
        for line in &lines {
            if line.trim().is_empty() {
                continue;
            }
            if line.chars().next().is_some_and(|c| c.is_whitespace()) {
                bail!("whitespace before command: {line}");
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }
            process_batch_command(
                repo,
                args,
                &parts,
                line,
                &mut transaction_active,
                &mut transaction_prepared,
                &mut staged,
                &mut pending_option_no_deref,
                true,
            )?;
        }
        if staged.is_empty() {
            return Ok(());
        }
        return commit_batch_staged(repo, args, &staged);
    }

    let mut transaction_active = false;
    let mut transaction_prepared = false;
    let mut staged: Vec<(bool, BatchOp)> = Vec::new();
    let mut pending_option_no_deref = false;

    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        if line.chars().next().is_some_and(|c| c.is_whitespace()) {
            bail!("whitespace before command: {line}");
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        process_batch_command(
            repo,
            args,
            &parts,
            line,
            &mut transaction_active,
            &mut transaction_prepared,
            &mut staged,
            &mut pending_option_no_deref,
            false,
        )?;
    }

    if transaction_active {
        let hook_updates = hook_updates_for_ops(&staged)?;
        if !hook_updates.is_empty() {
            run_ref_transaction_aborted(repo, &hook_updates);
        }
    }

    Ok(())
}

fn apply_batch_op(repo: &Repository, args: &Args, no_deref: bool, op: BatchOp) -> Result<()> {
    match op {
        BatchOp::UpdateOid {
            refname,
            new_oid,
            expected_old,
        } => {
            let target_refname = effective_refname(repo, &refname, no_deref)?;
            ensure_lockable_ref_path(repo, &refname, &target_refname, expected_old.is_none())?;
            if let Some(expected) = expected_old {
                verify_expected_old(repo, &refname, &target_refname, no_deref, expected)?;
            }
            let old_oid =
                resolve_ref(&repo.git_dir, &target_refname).unwrap_or_else(|_| zero_oid());
            write_ref(&repo.git_dir, &target_refname, &new_oid)?;
            let msg = args.log_message.as_deref().unwrap_or("");
            if should_write_update_reflog(repo, args, &target_refname, msg) {
                let _ = append_reflog(
                    &repo.git_dir,
                    &target_refname,
                    &old_oid,
                    &new_oid,
                    &resolve_reflog_identity(repo),
                    msg,
                    update_reflog_force_create(repo, args, &target_refname, msg),
                );
            }
        }
        BatchOp::CreateOid { refname, new_oid } => {
            let target_refname = effective_refname(repo, &refname, no_deref)?;
            ensure_lockable_ref_path(repo, &refname, &target_refname, true)?;
            if resolve_ref(&repo.git_dir, &target_refname).is_ok() {
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{refname}': reference already exists"
                ))));
            }
            write_ref(&repo.git_dir, &target_refname, &new_oid)?;
        }
        BatchOp::DeleteOid {
            refname,
            expected_old,
        } => {
            let target_refname = effective_refname(repo, &refname, no_deref)?;
            if let Some(expected) = expected_old {
                verify_expected_old(repo, &refname, &target_refname, no_deref, expected)?;
            }
            delete_ref(&repo.git_dir, &target_refname)?;
        }
        BatchOp::VerifyOid {
            refname,
            expected_old,
        } => {
            let target_refname = effective_refname(repo, &refname, no_deref)?;
            if let Some(expected) = expected_old {
                verify_expected_old(repo, &refname, &target_refname, no_deref, expected)?;
            } else if resolve_ref(&repo.git_dir, &target_refname).is_err() {
                bail!("ref '{target_refname}' does not exist");
            }
        }
        BatchOp::UpdateSymref {
            refname,
            new_target,
            expected_old,
        } => {
            if let Some(expected) = expected_old {
                verify_symref_expected_old(repo, &refname, expected)?;
            }
            write_symbolic_ref(repo, &refname, &new_target)?;
        }
        BatchOp::CreateSymref {
            refname,
            new_target,
        } => {
            if ref_exists_no_deref(repo, &refname)? {
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{refname}': reference already exists"
                ))));
            }
            write_symbolic_ref(repo, &refname, &new_target)?;
        }
        BatchOp::DeleteSymref {
            refname,
            expected_old,
        } => {
            if let Some(expected) = expected_old {
                verify_symref_expected_old(repo, &refname, expected)?;
            }
            delete_ref_no_deref(repo, &refname)?;
        }
        BatchOp::VerifySymref {
            refname,
            expected_old,
        } => verify_symref_expected_old(repo, &refname, expected_old)?,
    }

    Ok(())
}

fn effective_refname(repo: &Repository, refname: &str, no_deref: bool) -> Result<String> {
    if no_deref {
        return Ok(refname.to_owned());
    }
    let mut current = refname.to_owned();
    let mut seen = HashSet::new();
    for _ in 0..64 {
        if !seen.insert(current.clone()) {
            bail!("symref cycle involving '{refname}'");
        }
        match read_symbolic_ref(&repo.git_dir, &current) {
            Ok(Some(target)) => current = target,
            Ok(None) | Err(GritError::InvalidRef(_)) => return Ok(current),
            Err(err) => return Err(err.into()),
        }
    }
    bail!("symref chain depth exceeded for '{refname}'");
}

fn validate_update_refname(refname: &str) -> Result<()> {
    check_refname_format(
        refname,
        &RefNameOptions {
            allow_onelevel: true,
            refspec_pattern: false,
            normalize: false,
        },
    )
    .map(|_| ())
    .map_err(|_| anyhow::anyhow!("invalid ref format: {refname}"))
}

/// Commands whose first argument is a reference name and which must reject a
/// badly formatted name with `fatal: invalid ref format: <ref>`, mirroring the
/// `parse_refname()`/`check_refname_format()` check in `builtin/update-ref.c`.
fn batch_command_takes_refname(cmd: &str) -> bool {
    matches!(
        cmd,
        "update"
            | "create"
            | "delete"
            | "verify"
            | "symref-update"
            | "symref-create"
            | "symref-delete"
            | "symref-verify"
    )
}

/// Extract the raw reference name from the remainder of a stdin command line,
/// matching `parse_refname()` in `builtin/update-ref.c`.
///
/// Without `-z` (`null_terminated == false`) the refname is the first
/// whitespace-delimited argument, honoring backslash C-quoting. With `-z` the
/// refname is everything up to the end of the field, preserving any trailing
/// whitespace.
fn extract_raw_refname(rest: &str, null_terminated: bool) -> Option<String> {
    if null_terminated {
        if rest.is_empty() {
            return None;
        }
        return Some(rest.to_owned());
    }

    let mut chars = rest.chars().peekable();
    // Skip nothing: the caller has already trimmed the single delimiter space.
    let mut out = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        chars.next();
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                chars.next();
                out.push(next);
                continue;
            }
        }
        out.push(c);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Validate the reference name carried by a stdin batch command, emitting the
/// `fatal: invalid ref format: <ref>` error Git produces for a bad name.
fn validate_batch_refname(cmd: &str, raw_line: &str, null_terminated: bool) -> Result<()> {
    if !batch_command_takes_refname(cmd) {
        return Ok(());
    }
    // The refname follows the command word and a single delimiter character.
    let rest = match raw_line.strip_prefix(cmd) {
        Some(r) => r.strip_prefix(' ').unwrap_or(r),
        None => return Ok(()),
    };
    // Mirror `parse_refname()` in `builtin/update-ref.c`: without `-z` the
    // refname is the first whitespace-delimited (C-quoted) argument, but with
    // `-z` the refname is *everything* up to the end of the current
    // NUL-terminated field, including any trailing whitespace. The latter is why
    // `create ~a \0refs/heads/main\0` reports the bad name as `~a ` (with the
    // trailing space) rather than `~a`.
    let Some(refname) = extract_raw_refname(rest, null_terminated) else {
        return Ok(());
    };
    if check_refname_format(
        &refname,
        &RefNameOptions {
            allow_onelevel: true,
            refspec_pattern: false,
            normalize: false,
        },
    )
    .is_err()
    {
        return Err(anyhow::Error::from(GritError::Message(format!(
            "fatal: invalid ref format: {refname}"
        ))));
    }
    Ok(())
}

fn validate_delete_refname(refname: &str) -> Result<()> {
    if refname_is_safe(refname) {
        Ok(())
    } else {
        bail!("refusing to update ref with bad name '{refname}'")
    }
}

fn parse_old_expectation(raw: Option<&str>) -> Result<Option<OldExpectation>> {
    let Some(old) = raw else {
        return Ok(None);
    };
    if old.is_empty() {
        return Ok(None);
    }
    let expected: ObjectId = old.parse().context("invalid old-value OID")?;
    if is_zero_oid(&expected) {
        Ok(Some(OldExpectation::MustNotExist))
    } else {
        Ok(Some(OldExpectation::MustEqual(expected)))
    }
}

fn hook_old_value_from_expectation(expected: Option<OldExpectation>) -> String {
    match expected {
        Some(OldExpectation::MustNotExist) | None => zero_oid_hex().to_owned(),
        Some(OldExpectation::MustEqual(oid)) => oid.to_hex(),
    }
}

fn verify_expected_old(
    repo: &Repository,
    display_refname: &str,
    lock_refname: &str,
    no_deref: bool,
    expected: OldExpectation,
) -> Result<()> {
    let current = resolve_ref(&repo.git_dir, lock_refname).ok();
    match expected {
        OldExpectation::MustNotExist => {
            if current.is_some() {
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{display_refname}': reference already exists"
                ))));
            }
        }
        OldExpectation::MustEqual(oid) => match current {
            None => {
                if no_deref {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{display_refname}': reference is missing but expected {}",
                        oid.to_hex()
                    ))));
                }
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{display_refname}': unable to resolve reference '{lock_refname}'"
                ))));
            }
            Some(cur) if cur != oid => {
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{display_refname}': is at {} but expected {}",
                    cur.to_hex(),
                    oid.to_hex()
                ))));
            }
            _ => {}
        },
    }
    Ok(())
}

fn ensure_lockable_ref_path(
    repo: &Repository,
    display_refname: &str,
    lock_refname: &str,
    report_directory_block: bool,
) -> Result<()> {
    let (store, stor_name) =
        grit_lib::worktree_ref::resolve_ref_storage(&repo.git_dir, lock_refname);
    let storage_name = grit_lib::ref_namespace::storage_ref_name(&stor_name);
    let path = store.join(storage_name);

    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
        && directory_tree_contains_file(&path)
        && report_directory_block
    {
        let display_path = ref_path_for_display(&path);
        return Err(anyhow::Error::from(GritError::Message(format!(
            "fatal: cannot lock ref '{display_refname}': there is a non-empty directory '{display_path}' blocking reference '{lock_refname}'"
        ))));
    }

    match read_ref_file(&path) {
        Ok(_) => Ok(()),
        Err(GritError::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(GritError::InvalidRef(_)) => Err(anyhow::Error::from(GritError::Message(format!(
            "fatal: cannot lock ref '{display_refname}': unable to resolve reference '{lock_refname}': reference broken"
        )))),
        Err(err) => Err(err.into()),
    }
}

fn directory_tree_contains_file(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&entry_path) else {
            continue;
        };
        if meta.file_type().is_dir() {
            if directory_tree_contains_file(&entry_path) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

fn ref_path_for_display(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.to_string_lossy().into_owned();
        }
        if let (Ok(cwd_c), Ok(path_c)) = (cwd.canonicalize(), path.canonicalize()) {
            if let Ok(rel) = path_c.strip_prefix(&cwd_c) {
                return rel.to_string_lossy().into_owned();
            }
        }
    }
    path.to_string_lossy().into_owned()
}

fn hook_update_for_op(op: &BatchOp) -> Result<HookUpdate> {
    let update = match op {
        BatchOp::UpdateOid {
            refname,
            new_oid,
            expected_old,
        } => HookUpdate {
            old_value: hook_old_value_from_expectation(*expected_old),
            new_value: new_oid.to_hex(),
            refname: refname.clone(),
            deletes_ref: *new_oid == zero_oid(),
        },
        BatchOp::CreateOid { refname, new_oid } => HookUpdate {
            old_value: zero_oid_hex().to_owned(),
            new_value: new_oid.to_hex(),
            refname: refname.clone(),
            deletes_ref: false,
        },
        BatchOp::DeleteOid {
            refname,
            expected_old,
        } => HookUpdate {
            old_value: hook_old_value_from_expectation(*expected_old),
            new_value: zero_oid_hex().to_owned(),
            refname: refname.clone(),
            deletes_ref: true,
        },
        BatchOp::VerifyOid {
            refname,
            expected_old,
        } => HookUpdate {
            old_value: hook_old_value_from_expectation(*expected_old),
            new_value: zero_oid_hex().to_owned(),
            refname: refname.clone(),
            deletes_ref: false,
        },
        BatchOp::UpdateSymref {
            refname,
            new_target,
            expected_old,
        } => HookUpdate {
            old_value: symref_old_for_hook(expected_old.clone()),
            new_value: format!("ref:{new_target}"),
            refname: refname.clone(),
            deletes_ref: false,
        },
        BatchOp::CreateSymref {
            refname,
            new_target,
        } => HookUpdate {
            old_value: zero_oid_hex().to_owned(),
            new_value: format!("ref:{new_target}"),
            refname: refname.clone(),
            deletes_ref: false,
        },
        BatchOp::DeleteSymref {
            refname,
            expected_old,
        } => HookUpdate {
            old_value: symref_old_for_hook(expected_old.clone()),
            new_value: zero_oid_hex().to_owned(),
            refname: refname.clone(),
            deletes_ref: true,
        },
        BatchOp::VerifySymref {
            refname,
            expected_old,
        } => HookUpdate {
            old_value: symref_old_for_hook(Some(expected_old.clone())),
            new_value: zero_oid_hex().to_owned(),
            refname: refname.clone(),
            deletes_ref: false,
        },
    };

    Ok(update)
}

fn hook_updates_for_ops(ops: &[(bool, BatchOp)]) -> Result<Vec<HookUpdate>> {
    let mut updates = Vec::with_capacity(ops.len());
    for (_, op) in ops {
        updates.push(hook_update_for_op(op)?);
    }
    Ok(updates)
}

fn symref_old_for_hook(expected_old: Option<SymrefOldExpectation>) -> String {
    match expected_old {
        None | Some(SymrefOldExpectation::MustNotExist) => zero_oid_hex().to_owned(),
        Some(SymrefOldExpectation::MustTarget(target)) => format!("ref:{target}"),
        Some(SymrefOldExpectation::MustOid(oid)) => oid.to_hex(),
    }
}

fn reftable_log_identity_parts(identity: &str) -> (String, String, u64, i16) {
    let (name_part, rest) = identity
        .rsplit_once(" <")
        .map(|(name, rest)| (name.to_owned(), rest))
        .unwrap_or_else(|| ("Unknown".to_owned(), identity));
    let (email, after_email) = rest
        .split_once("> ")
        .map(|(email, after)| (email.to_owned(), after))
        .unwrap_or_else(|| (String::new(), rest));
    let mut parts = after_email.split_whitespace();
    let time_seconds = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let tz_offset = parts.next().map(parse_reftable_tz_offset).unwrap_or(0);
    (name_part, email, time_seconds, tz_offset)
}

fn parse_reftable_tz_offset(raw: &str) -> i16 {
    if raw.len() != 5 {
        return 0;
    }
    let sign = if raw.as_bytes().first() == Some(&b'-') {
        -1
    } else {
        1
    };
    let hours = raw[1..3].parse::<i16>().unwrap_or(0);
    let minutes = raw[3..5].parse::<i16>().unwrap_or(0);
    sign * (hours * 60 + minutes)
}

pub(crate) fn resolve_reflog_identity(repo: &Repository) -> String {
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_NAME").ok())
        .or_else(|| config.as_ref().and_then(|c| c.get("user.name")))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_EMAIL").ok())
        .or_else(|| config.as_ref().and_then(|c| c.get("user.email")))
        .unwrap_or_default();

    let date_str = std::env::var("GIT_COMMITTER_DATE")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_DATE").ok());

    if let Some(date) = date_str {
        if let Ok(canonical) = grit_lib::git_date::parse::parse_date(&date) {
            return format!("{name} <{email}> {canonical}");
        }
        return format!("{name} <{email}> {date}");
    }

    let now = OffsetDateTime::now_utc();
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    format!("{name} <{email}> {epoch} {hours:+03}{minutes:02}")
}

fn read_symbolic_ref_no_deref(repo: &Repository, refname: &str) -> Result<Option<String>> {
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) && refname != "HEAD" {
        return grit_lib::reftable::reftable_read_symbolic_ref(&repo.git_dir, refname)
            .map_err(|e| anyhow::anyhow!("{e}"));
    }

    let path = repo.git_dir.join(refname);
    if let Ok(target) = fs::read_link(&path) {
        return Ok(Some(target.to_string_lossy().into_owned()));
    }
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let trimmed = content.trim();
    if let Some(target) = trimmed.strip_prefix("ref: ") {
        Ok(Some(target.to_owned()))
    } else {
        Ok(None)
    }
}

fn write_symbolic_ref(repo: &Repository, refname: &str, target: &str) -> Result<()> {
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) && refname != "HEAD" {
        grit_lib::reftable::reftable_write_symref(&repo.git_dir, refname, target, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(());
    }
    let path = repo.git_dir.join(refname);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if prefers_symlink_refs(repo) {
        warn_prefer_symlink_refs();
        let _ = fs::remove_file(&path);
        create_symbolic_link(target, &path)?;
        return Ok(());
    }
    let lock_path = grit_lib::refs::lock_path_for_ref(&path);
    fs::write(&lock_path, format!("ref: {target}\n"))?;
    fs::rename(lock_path, path)?;
    Ok(())
}

fn prefers_symlink_refs(repo: &Repository) -> bool {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    config
        .get("core.prefersymlinkrefs")
        .and_then(|raw| parse_bool(&raw).ok())
        .unwrap_or(false)
}

fn warn_prefer_symlink_refs() {
    eprintln!("warning: 'core.preferSymlinkRefs=true' is nominated for removal.");
    eprintln!("hint: The use of symbolic links for symbolic refs is deprecated");
    eprintln!("hint: and will be removed in Git 3.0. The configuration that");
    eprintln!("hint: tells Git to use them is thus going away. You can unset");
    eprintln!("hint: it with:");
    eprintln!("hint:");
    eprintln!("hint:\tgit config unset core.preferSymlinkRefs");
    eprintln!("hint:");
    eprintln!("hint: Git will then use the textual symref format instead.");
}

#[cfg(unix)]
fn create_symbolic_link(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn create_symbolic_link(target: &str, path: &Path) -> io::Result<()> {
    fs::write(path, format!("ref: {target}\n"))
}

fn delete_ref_no_deref(repo: &Repository, refname: &str) -> Result<()> {
    delete_ref(&repo.git_dir, refname).map_err(Into::into)
}

fn ref_exists_no_deref(repo: &Repository, refname: &str) -> Result<bool> {
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) && refname != "HEAD" {
        if read_symbolic_ref_no_deref(repo, refname)?.is_some() {
            return Ok(true);
        }
        return Ok(grit_lib::reftable::reftable_resolve_ref(&repo.git_dir, refname).is_ok());
    }

    let path = repo.git_dir.join(refname);
    if path.exists() {
        return Ok(true);
    }
    Ok(resolve_ref(&repo.git_dir, refname).is_ok())
}

fn verify_symref_expected_old(
    repo: &Repository,
    refname: &str,
    expected: SymrefOldExpectation,
) -> Result<()> {
    match expected {
        SymrefOldExpectation::MustNotExist => {
            if ref_exists_no_deref(repo, refname)? {
                return Err(anyhow::Error::from(GritError::Message(format!(
                    "fatal: cannot lock ref '{refname}': reference already exists"
                ))));
            }
        }
        SymrefOldExpectation::MustTarget(target) => {
            let current = read_symbolic_ref_no_deref(repo, refname)?;
            match current {
                None => {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': unable to resolve reference '{refname}'"
                    ))));
                }
                Some(cur) if cur != target => {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': points to {cur} but expected {target}"
                    ))));
                }
                Some(_) => {}
            }
        }
        SymrefOldExpectation::MustOid(oid) => {
            let current = resolve_ref(&repo.git_dir, refname).ok();
            match current {
                None => {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': unable to resolve reference '{refname}'"
                    ))));
                }
                Some(cur) if cur != oid => {
                    return Err(anyhow::Error::from(GritError::Message(format!(
                        "fatal: cannot lock ref '{refname}': is at {} but expected {}",
                        cur.to_hex(),
                        oid.to_hex()
                    ))));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn zero_oid_hex() -> &'static str {
    "0000000000000000000000000000000000000000"
}

fn is_zero_oid(oid: &ObjectId) -> bool {
    oid.as_bytes().iter().all(|byte| *byte == 0)
}

fn zero_oid() -> ObjectId {
    match ObjectId::from_bytes(&[0u8; 20]) {
        Ok(oid) => oid,
        Err(err) => panic!("20-byte zero OID should always be valid: {err}"),
    }
}
