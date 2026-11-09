//! `grit resolve` — resolve revision specs to object ids and types.

use anyhow::{Context, Result};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::object_inspect::ResolvedObject;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_lib::revwalk::AbbrevLength;

use crate::context;
pub use crate::render::resolve::{
    RefNameOutput, ResolveEntry, ResolveOutcome, VerifyResolveOutcome,
};
use crate::revision_errors::map_resolve_error;

pub fn run_verify(spec: String, short: bool, quiet: bool) -> Result<VerifyResolveOutcome> {
    let repo = if quiet {
        context::discover()?
    } else {
        context::discover_with_warnings()?.0
    };
    let oid = repo
        .verify_object(&spec, None)
        .map_err(|e| map_resolve_error(&spec, e))?;
    let oid =
        display_oid(&repo, oid, short).with_context(|| format!("abbreviate oid for '{spec}'"))?;
    Ok(VerifyResolveOutcome { spec, oid })
}

fn display_oid(repo: &Repository, oid: ObjectId, short: bool) -> grit_lib::error::Result<String> {
    if short {
        repo.revwalk().abbreviate(oid, AbbrevLength::Configured)
    } else {
        Ok(oid.to_hex())
    }
}

/// Exit code for `grit resolve --verify` (0 = resolved, 1 = not).
#[must_use]
pub fn verify_exit_code(result: &Result<VerifyResolveOutcome, anyhow::Error>) -> i32 {
    if result.is_ok() {
        0
    } else {
        1
    }
}

pub fn run(specs: Vec<String>, short: bool, ref_output: RefNameOutput) -> Result<ResolveOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        match ref_output {
            RefNameOutput::AbbrevRef | RefNameOutput::SymbolicFullName => {
                match repo.resolve_ref_name(&spec) {
                    Ok(Some(names)) => {
                        let display = if ref_output == RefNameOutput::AbbrevRef {
                            names.short.clone()
                        } else {
                            names.full.clone()
                        };
                        results.push(ResolveEntry {
                            spec: spec.clone(),
                            oid: None,
                            kind: None,
                            size: None,
                            ref_name: Some(names.short),
                            full_ref: Some(names.full),
                            ref_display: Some(display),
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return Err(map_rev_parse_ref_error(&spec, e));
                    }
                }
            }
            RefNameOutput::ObjectIds => {
                let resolved = repo
                    .resolve(&spec)
                    .map_err(|e| map_resolve_error(&spec, e))?;
                let oid = display_oid(&repo, resolved.oid, short)
                    .with_context(|| format!("abbreviate oid for '{spec}'"))?;
                results.push(entry_from_resolved(&spec, &resolved, oid));
            }
        }
    }
    Ok(ResolveOutcome {
        warnings: sink.warnings(),
        results,
    })
}

fn entry_from_resolved(spec: &str, resolved: &ResolvedObject, oid: String) -> ResolveEntry {
    ResolveEntry {
        spec: spec.to_owned(),
        oid: Some(oid),
        kind: Some(kind_label(resolved.kind).to_owned()),
        size: Some(resolved.size),
        ref_name: resolved.ref_name.clone(),
        full_ref: resolved.full_ref.clone(),
        ref_display: None,
    }
}

fn map_rev_parse_ref_error(spec: &str, err: RevParseError) -> anyhow::Error {
    crate::error_map::map_rev_parse_ref_error(spec, err)
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// Dispatch parsed CLI args for `Resolve`.
pub fn dispatch(
    args: crate::args::ResolveArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let ResolveArgs {
        specs,
        short,
        abbrev_ref,
        symbolic_full_name,
        verify_spec,
        quiet,
    } = args;
    {
        use commands::resolve::RefNameOutput;
        if let Some(spec) = verify_spec {
            let outcome = commands::resolve::run_verify(spec, short, quiet);
            let code = commands::resolve::verify_exit_code(&outcome);
            if quiet {
                std::process::exit(code);
            }
            emit_success(outcome?, opts, ctx)
        } else {
            if quiet {
                anyhow::bail!("--quiet requires --verify");
            }
            let ref_output = if abbrev_ref {
                RefNameOutput::AbbrevRef
            } else if symbolic_full_name {
                RefNameOutput::SymbolicFullName
            } else {
                RefNameOutput::ObjectIds
            };
            emit_success(commands::resolve::run(specs, short, ref_output)?, opts, ctx)
        }
    }
}
