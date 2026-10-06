//! `grit symbolic-ref` — read, update, and delete symbolic refs.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::objects::ObjectId;
use grit_lib::refs::{append_reflog, delete_ref, read_ref_file, Ref};
use grit_lib::repo::Repository;

use crate::ref_transaction_hooks::{
    run_ref_transaction_committed, run_ref_transaction_prepare, HookUpdate,
};
use std::fs;
use std::io;
use std::path::Path;

/// Arguments for `grit symbolic-ref`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Suppress non-symbolic-ref error output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Delete symbolic ref.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Shorten ref output.
    #[arg(long = "short")]
    pub short: bool,

    /// Stop after one dereference.
    #[arg(long = "no-recurse")]
    pub no_recurse: bool,

    /// Reflog message when updating a symbolic ref.
    #[arg(short = 'm')]
    pub message: Option<String>,

    /// The symbolic ref name.
    pub name: Option<String>,

    /// New target ref.
    pub reference: Option<String>,
}

/// Run `grit symbolic-ref`.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let _ = grit_lib::repo::trace_repo_setup_if_requested(&repo);

    if matches!(args.message.as_deref(), Some("")) {
        bail!("Refusing to perform update with empty message");
    }

    if args.delete {
        let Some(name) = args.name.as_deref() else {
            bail!("usage: grit symbolic-ref --delete [-q] <name>");
        };
        if args.reference.is_some() {
            bail!("usage: grit symbolic-ref --delete [-q] <name>");
        }
        if !is_symbolic_ref(&repo.git_dir, name)? {
            eprintln!("fatal: Cannot delete {name}, not a symbolic ref");
            std::process::exit(128);
        }
        if name == "HEAD" {
            bail!("deleting '{name}' is not allowed");
        }
        let old_target = read_symbolic_ref_target_maybe_missing(&repo.git_dir, name, false)?
            .ok_or_else(|| anyhow::anyhow!("ref {name} is not a symbolic ref"))?;
        let hook_update = HookUpdate {
            old_value: format!("ref:{old_target}"),
            new_value: zero_oid_hex().to_owned(),
            refname: name.to_owned(),
            deletes_ref: true,
        };
        run_ref_transaction_prepare(&repo, std::slice::from_ref(&hook_update))?;
        delete_loose_ref(&repo.git_dir, name)?;
        run_ref_transaction_committed(&repo, &[hook_update]);
        return Ok(());
    }

    match (args.name.as_deref(), args.reference.as_deref()) {
        (Some(name), None) => {
            match read_symbolic_ref_target(&repo.git_dir, name, !args.no_recurse)? {
                Some(target) => {
                    if args.short {
                        println!("{}", shorten_ref(&target));
                    } else {
                        println!("{target}");
                    }
                    Ok(())
                }
                None if args.quiet => {
                    std::process::exit(1);
                }
                None => bail!("ref {name} is not a symbolic ref"),
            }
        }
        (Some(name), Some(target)) => {
            if name == "HEAD" && !target.starts_with("refs/") {
                bail!("Refusing to point HEAD outside of refs/");
            }
            if !is_valid_refname(target, true) {
                bail!("Refusing to set '{name}' to invalid ref '{target}'");
            }
            // Check for d/f conflicts: verify no existing ref is a prefix of
            // the new ref name (or vice versa).
            if name.starts_with("refs/") {
                check_ref_df_conflict(&repo, name)?;
            }
            let old_oid = resolve_for_reflog(&repo, name);
            let old_value = read_symbolic_ref_target_maybe_missing(&repo.git_dir, name, false)?
                .map(|target| format!("ref:{target}"))
                .unwrap_or_else(|| zero_oid_hex().to_owned());
            let hook_update = HookUpdate {
                old_value,
                new_value: format!("ref:{target}"),
                refname: name.to_owned(),
                deletes_ref: false,
            };
            run_ref_transaction_prepare(&repo, std::slice::from_ref(&hook_update))?;
            write_symbolic_ref(&repo.git_dir, name, target)?;
            run_ref_transaction_committed(&repo, &[hook_update]);
            if let Some(message) = args.message.as_deref() {
                let new_oid = resolve_for_reflog(&repo, name);
                write_symref_reflog(&repo, name, &old_oid, &new_oid, message)?;
            }
            Ok(())
        }
        _ => bail!("usage: grit symbolic-ref [-m <reason>] <name> <ref>"),
    }
}

fn read_symbolic_ref_target(git_dir: &Path, name: &str, recurse: bool) -> Result<Option<String>> {
    let result = read_symbolic_ref_target_maybe_missing(git_dir, name, recurse)?;
    match result {
        Some(target) => Ok(Some(target)),
        None => {
            // Distinguish: direct ref (not symbolic) vs missing ref vs symref with missing target.
            let path = git_dir.join(name);
            match read_ref_file(&path) {
                Ok(Ref::Direct(_)) => Ok(None),
                Ok(Ref::Symbolic(target)) => Ok(Some(target)),
                Err(grit_lib::error::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                    bail!("No such ref: {name}");
                }
                Err(err) => Err(err.into()),
            }
        }
    }
}

fn read_symbolic_ref_target_maybe_missing(
    git_dir: &Path,
    name: &str,
    recurse: bool,
) -> Result<Option<String>> {
    // Reftable backend: check reftable stack for symrefs
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        // HEAD is still a file even in reftable repos
        if name == "HEAD" {
            if let Ok(Ref::Symbolic(target)) = read_ref_file(&git_dir.join("refs").join("heads")) {
                return Ok(Some(target));
            }
        }
        if name != "HEAD" {
            match grit_lib::reftable::reftable_read_symbolic_ref(git_dir, name)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                Some(target) => return Ok(Some(target)),
                None => {
                    // Check if it exists as a direct ref
                    match grit_lib::reftable::reftable_resolve_ref(git_dir, name) {
                        Ok(_) => return Ok(None), // exists but not symbolic
                        Err(_) => return Ok(None),
                    }
                }
            }
        }
    }

    let path = git_dir.join(name);
    match read_ref_file(&path) {
        Ok(Ref::Direct(_)) => Ok(None),
        Ok(Ref::Symbolic(mut target)) => {
            if !recurse {
                return Ok(Some(target));
            }
            for _ in 0..10 {
                let next_path = git_dir.join(&target);
                match read_ref_file(&next_path) {
                    Ok(Ref::Direct(_)) => return Ok(Some(target)),
                    Ok(Ref::Symbolic(next)) => target = next,
                    Err(grit_lib::error::Error::Io(err))
                        if err.kind() == io::ErrorKind::NotFound =>
                    {
                        return Ok(Some(target));
                    }
                    Err(_) => return Ok(Some(target)),
                }
            }
            Ok(Some(target))
        }
        Err(grit_lib::error::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn is_symbolic_ref(git_dir: &Path, name: &str) -> Result<bool> {
    if grit_lib::reftable::is_reftable_repo(git_dir) && name != "HEAD" {
        match grit_lib::reftable::reftable_read_symbolic_ref(git_dir, name)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            Some(_) => return Ok(true),
            None => return Ok(false),
        }
    }
    let path = git_dir.join(name);
    match read_ref_file(&path) {
        Ok(Ref::Symbolic(_)) => Ok(true),
        Ok(Ref::Direct(_)) => Ok(false),
        Err(grit_lib::error::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Check for directory/file conflicts with existing refs.
/// E.g., creating `refs/heads/foo/bar` when `refs/heads/foo` exists (or vice versa).
fn check_ref_df_conflict(repo: &Repository, name: &str) -> Result<()> {
    // Check if any prefix of `name` is an existing ref
    let components: Vec<&str> = name.split('/').collect();
    for i in 1..components.len() {
        let prefix = components[..i].join("/");
        if prefix.starts_with("refs/")
            && grit_lib::refs::resolve_ref(&repo.git_dir, &prefix).is_ok()
        {
            bail!("'{prefix}' exists; cannot create '{name}'");
        }
    }
    // Check if any existing ref has `name` as a prefix
    let prefix_with_slash = format!("{name}/");
    let all_refs = grit_lib::refs::list_refs(&repo.git_dir, "refs/").unwrap_or_default();
    for (refname, _) in &all_refs {
        if refname.starts_with(&prefix_with_slash) {
            bail!("'{refname}' exists; cannot create '{name}'");
        }
    }
    Ok(())
}

fn write_symbolic_ref(git_dir: &Path, name: &str, target: &str) -> Result<()> {
    if grit_lib::reftable::is_reftable_repo(git_dir) && name != "HEAD" {
        grit_lib::reftable::reftable_write_symref(git_dir, name, target, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(());
    }
    let path = git_dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = grit_lib::refs::lock_path_for_ref(&path);
    fs::write(&lock_path, format!("ref: {target}\n"))?;
    fs::rename(lock_path, path)?;
    Ok(())
}

fn delete_loose_ref(git_dir: &Path, name: &str) -> Result<()> {
    if grit_lib::reftable::is_reftable_repo(git_dir) && name != "HEAD" {
        grit_lib::reftable::reftable_delete_ref(git_dir, name)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(());
    }
    delete_ref(git_dir, name).map_err(Into::into)
}

fn write_symref_reflog(
    repo: &Repository,
    name: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    message: &str,
) -> Result<()> {
    append_reflog(
        &repo.git_dir,
        name,
        old_oid,
        new_oid,
        "grit <grit> 0 +0000",
        message,
        false,
    )?;
    Ok(())
}

fn resolve_for_reflog(repo: &Repository, name: &str) -> ObjectId {
    match grit_lib::refs::resolve_ref(&repo.git_dir, name) {
        Ok(oid) => oid,
        Err(_) => zero_oid(),
    }
}

fn zero_oid() -> ObjectId {
    match ObjectId::from_bytes(&[0u8; 20]) {
        Ok(oid) => oid,
        Err(_) => unreachable!("20-byte zero OID is always valid"),
    }
}

fn zero_oid_hex() -> &'static str {
    "0000000000000000000000000000000000000000"
}

fn is_valid_refname(name: &str, allow_onelevel: bool) -> bool {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name.ends_with(".lock")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return false;
    }
    if !allow_onelevel && !name.contains('/') {
        return false;
    }
    for comp in name.split('/') {
        if comp.is_empty()
            || comp == "."
            || comp == ".."
            || comp.starts_with('.')
            || comp.ends_with('.')
        {
            return false;
        }
    }
    true
}

fn shorten_ref(name: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if prefix == "refs/remotes/" {
                if let Some((remote, tail)) = rest.split_once("/HEAD") {
                    if tail.is_empty() {
                        return remote.to_owned();
                    }
                }
                return rest.to_owned();
            }
            return rest.to_owned();
        }
    }
    if let Some(rest) = name.strip_prefix("refs/") {
        return rest.to_owned();
    }
    name.to_owned()
}
