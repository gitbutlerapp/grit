//! `grit replay` — replay commits on a new base (Git-compatible subset).
//!
//! Replays commits selected by `rev-list` semantics onto `--onto` or advances a
//! single branch with `--advance`, using merge-ort style tree merging. Ref
//! updates are applied atomically (best-effort) or printed with
//! `--ref-action=print`.

use crate::commands::merge::{
    merge_trees_for_replay, MergeDirectoryRenamesMode, MergeRenameOptions, ReplayTreeMergeResult,
};
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{detect_renames, DiffEntry, DiffStatus};
use grit_lib::index::IndexEntry;
use grit_lib::merge_file::MergeFavor;
use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::refs::{self, read_head, resolve_ref};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, split_revision_token, OrderingMode, RevListOptions};
use grit_lib::rev_parse::{expand_rev_token_circ_bang, resolve_revision_as_commit};
use grit_lib::write_tree::write_tree_from_index;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use time::OffsetDateTime;

/// Arguments for `grit replay`.
#[derive(Debug, ClapArgs)]
#[command(about = "Replay commits on a new base")]
pub struct Args {
    /// Raw arguments forwarded from the grit dispatcher.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefAction {
    Update,
    Print,
}

#[derive(Debug, Default)]
struct ParsedReplayCli {
    onto: Option<String>,
    advance: Option<String>,
    contained: bool,
    ref_action_cli: Option<String>,
    branches: bool,
    ancestry_path: Option<String>,
    revisions: Vec<String>,
}

fn parse_replay_cli(raw: &[String]) -> Result<ParsedReplayCli> {
    let mut out = ParsedReplayCli::default();
    let mut i = 0usize;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--onto" {
            let Some(value) = raw.get(i + 1) else {
                bail!("error: option '--onto' requires a value");
            };
            out.onto = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--onto=") {
            if value.is_empty() {
                bail!("error: option '--onto' requires a value");
            }
            out.onto = Some(value.to_owned());
            i += 1;
            continue;
        }
        if arg == "--advance" {
            let Some(value) = raw.get(i + 1) else {
                bail!("error: option '--advance' requires a value");
            };
            out.advance = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--advance=") {
            if value.is_empty() {
                bail!("error: option '--advance' requires a value");
            }
            out.advance = Some(value.to_owned());
            i += 1;
            continue;
        }
        if arg == "--contained" {
            out.contained = true;
            i += 1;
            continue;
        }
        if arg == "--ref-action" {
            let Some(value) = raw.get(i + 1) else {
                bail!("error: option '--ref-action' requires a value");
            };
            out.ref_action_cli = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--ref-action=") {
            if value.is_empty() {
                bail!("error: option '--ref-action' requires a value");
            }
            out.ref_action_cli = Some(value.to_owned());
            i += 1;
            continue;
        }
        if arg == "--branches" {
            out.branches = true;
            i += 1;
            continue;
        }
        if arg == "--ancestry-path" {
            let Some(value) = raw.get(i + 1) else {
                bail!("error: option '--ancestry-path' requires a value");
            };
            out.ancestry_path = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--ancestry-path=") {
            if value.is_empty() {
                bail!("error: option '--ancestry-path' requires a value");
            }
            out.ancestry_path = Some(value.to_owned());
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            bail!("unsupported option: {arg}");
        }
        out.revisions.push(arg.clone());
        i += 1;
    }
    Ok(out)
}

fn parse_ref_action_mode(raw: Option<&str>, source: &str) -> Result<RefAction> {
    let s = raw.unwrap_or("update").trim();
    if s.eq_ignore_ascii_case("update") {
        return Ok(RefAction::Update);
    }
    if s.eq_ignore_ascii_case("print") {
        return Ok(RefAction::Print);
    }
    bail!(
        "invalid {source} value: '{raw}'",
        raw = raw.unwrap_or_default()
    );
}

fn ref_action_for_run(repo: &Repository, cli: Option<&str>) -> Result<RefAction> {
    if let Some(v) = cli {
        return parse_ref_action_mode(Some(v), "--ref-action");
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    if let Some(v) = config.get("replay.refAction") {
        return parse_ref_action_mode(Some(&v), "replay.refAction");
    }
    Ok(RefAction::Update)
}

fn peel_committish_for_mode(repo: &Repository, spec: &str, mode: &str) -> Result<ObjectId> {
    resolve_revision_as_commit(repo, spec)
        .map_err(|_| anyhow::anyhow!("fatal: '{spec}' is not a valid commit-ish for {mode}\n"))
}

fn try_dwim_single_branch_ref(git_dir: &Path, spec: &str) -> Result<Option<String>> {
    let want_short = spec.strip_prefix("refs/heads/").unwrap_or(spec);
    let mut matches: Vec<String> = Vec::new();
    for (name, _) in refs::list_refs(git_dir, "refs/heads/")? {
        if name == spec {
            matches.push(name);
            continue;
        }
        if let Some(tail) = name.strip_prefix("refs/heads/") {
            if tail == want_short {
                matches.push(name);
            }
        }
    }
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        Ok(Some(matches[0].clone()))
    } else {
        Ok(None)
    }
}

fn collect_revision_specs(
    repo: &Repository,
    parsed: &ParsedReplayCli,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for rev in &parsed.revisions {
        for expanded in expand_rev_token_circ_bang(repo, rev)? {
            let (pos, neg) = split_revision_token(&expanded);
            positive.extend(pos);
            negative.extend(neg);
        }
    }
    if parsed.branches {
        for (name, _) in refs::list_refs(&repo.git_dir, "refs/heads/")? {
            positive.push(name);
        }
    }
    Ok((positive, negative))
}

fn build_commit_to_refs_map(repo: &Repository) -> Result<HashMap<ObjectId, Vec<String>>> {
    let mut map: HashMap<ObjectId, Vec<String>> = HashMap::new();
    if let Ok(head_oid) = resolve_ref(&repo.git_dir, "HEAD") {
        if let Ok(commit_oid) = grit_lib::rev_parse::peel_to_commit_for_merge_base(repo, head_oid) {
            map.entry(commit_oid).or_default().push("HEAD".to_owned());
        }
    }
    for (name, oid) in refs::list_refs(&repo.git_dir, "refs/heads/")? {
        let Ok(commit_oid) = grit_lib::rev_parse::peel_to_commit_for_merge_base(repo, oid) else {
            continue;
        };
        map.entry(commit_oid).or_default().push(name);
    }
    Ok(map)
}

fn commit_is_ancestor(repo: &Repository, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut stack = vec![descendant];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        for parent in commit.parents {
            if parent == ancestor {
                return Ok(true);
            }
            stack.push(parent);
        }
    }
    Ok(false)
}

#[derive(Debug)]
struct RefUpdateLine {
    refname: String,
    new_oid: ObjectId,
    old_oid: ObjectId,
}

/// Run `grit replay`.
pub fn run(args: Args) -> Result<()> {
    let parsed = parse_replay_cli(&args.args)?;
    let has_onto = parsed.onto.is_some();
    let has_advance = parsed.advance.is_some();
    if !has_onto && !has_advance {
        eprintln!("error: option --onto or --advance is mandatory");
        eprintln!();
        eprintln!("usage: git replay ([--contained] --onto <newbase> | --advance <branch>) [--ref-action[=<mode>]] <revision-range>");
        std::process::exit(129);
    }
    if has_advance && parsed.contained {
        bail!("fatal: options '--advance' and '--contained' cannot be used together");
    }
    if has_advance && has_onto {
        bail!("fatal: options '--advance' and '--onto' cannot be used together");
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let ref_mode = ref_action_for_run(&repo, parsed.ref_action_cli.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let (positive_specs, negative_specs) = collect_revision_specs(&repo, &parsed)?;
    if positive_specs.is_empty() {
        bail!("fatal: need some commits to replay");
    }

    let mut rev_opts = RevListOptions::default();
    rev_opts.ordering = OrderingMode::Topo;
    rev_opts.reverse = true;
    if let Some(ap) = parsed.ancestry_path.as_deref() {
        rev_opts.ancestry_path = true;
        let bottom = resolve_revision_as_commit(&repo, ap).map_err(|e| {
            anyhow::anyhow!(
                "{}",
                e.to_string()
                    .trim_start_matches("fatal: ")
                    .trim_end_matches('\n')
            )
        })?;
        rev_opts.ancestry_path_bottoms = vec![bottom];
    }

    let result = rev_list(&repo, &positive_specs, &negative_specs, &rev_opts)?;
    let commits = result.commits;
    if commits.is_empty() {
        return Ok(());
    }

    let detached_head = read_head(&repo.git_dir)?.is_none();

    let onto_oid = if let Some(onto_spec) = parsed.onto.as_deref() {
        peel_committish_for_mode(&repo, onto_spec, "--onto")
            .map_err(|e| anyhow::anyhow!("{}", e.to_string().trim_end_matches('\n')))?
    } else {
        let adv = parsed
            .advance
            .as_deref()
            .context("option --onto or --advance is mandatory")?;
        let full = try_dwim_single_branch_ref(&repo.git_dir, adv)?
            .ok_or_else(|| anyhow::anyhow!("fatal: argument to --advance must be a reference"))?;
        if positive_specs.len() > 1 {
            bail!("fatal: cannot advance target with multiple sources because ordering would be ill-defined");
        }
        peel_committish_for_mode(&repo, &full, "--advance")
            .map_err(|e| anyhow::anyhow!("{}", e.to_string().trim_end_matches('\n')))?
    };

    let positive_ref_fullnames_order: Vec<String> = {
        let mut refs = Vec::new();
        let mut seen = HashSet::new();
        for spec in &positive_specs {
            if let Ok(Some(full)) = try_dwim_single_branch_ref(&repo.git_dir, spec) {
                if seen.insert(full.clone()) {
                    refs.push(full);
                }
            }
        }
        refs
    };
    let positive_ref_fullnames: HashSet<String> =
        positive_ref_fullnames_order.iter().cloned().collect();

    let advance_full_ref = if let Some(adv) = parsed.advance.as_deref() {
        Some(
            try_dwim_single_branch_ref(&repo.git_dir, adv)?.ok_or_else(|| {
                anyhow::anyhow!("fatal: argument to --advance must be a reference")
            })?,
        )
    } else {
        None
    };

    if has_onto && positive_ref_fullnames.len() < positive_specs.len() && !detached_head {
        bail!("fatal: all positive revisions given must be references");
    }

    let commit_to_refs = build_commit_to_refs_map(&repo)?;

    let mut updates: Vec<RefUpdateLine> = Vec::new();

    if let Some(adv_full) = advance_full_ref {
        let (replayed_tip, _replayed) = replay_commits_onto(&repo, &commits, onto_oid)?;
        let old_oid = resolve_ref(&repo.git_dir, &adv_full)?;
        updates.push(RefUpdateLine {
            refname: adv_full,
            new_oid: replayed_tip,
            old_oid,
        });
    } else {
        let mut seen_ref: HashSet<String> = HashSet::new();
        let mut push_ref_update = |updates: &mut Vec<RefUpdateLine>,
                                   replayed: &HashMap<ObjectId, ObjectId>,
                                   refname: &str,
                                   commit_oid: &ObjectId|
         -> Result<()> {
            let Some(&new_tip) = replayed.get(commit_oid) else {
                return Ok(());
            };
            if !seen_ref.insert(refname.to_owned()) {
                return Ok(());
            }
            let old_oid = resolve_ref(&repo.git_dir, refname)?;
            if old_oid != new_tip {
                updates.push(RefUpdateLine {
                    refname: refname.to_owned(),
                    new_oid: new_tip,
                    old_oid,
                });
            }
            Ok(())
        };

        for positive_ref in &positive_ref_fullnames_order {
            let positive_tip = resolve_ref(&repo.git_dir, positive_ref)?;
            let branch_commits: Vec<ObjectId> = commits
                .iter()
                .copied()
                .filter(|commit_oid| {
                    commit_is_ancestor(&repo, *commit_oid, positive_tip).unwrap_or(false)
                })
                .collect();
            let (_branch_tip, branch_replayed) =
                replay_commits_onto(&repo, &branch_commits, onto_oid)?;
            if !parsed.contained {
                push_ref_update(&mut updates, &branch_replayed, positive_ref, &positive_tip)?;
                continue;
            }
            for commit_oid in &branch_commits {
                let Some(refs_at) = commit_to_refs.get(commit_oid) else {
                    continue;
                };
                for r in refs_at {
                    if r == "HEAD" || !r.starts_with("refs/heads/") {
                        continue;
                    }
                    push_ref_update(&mut updates, &branch_replayed, r, commit_oid)?;
                }
            }
        }
        if detached_head && positive_ref_fullnames_order.is_empty() {
            let (_replayed_tip, replayed) = replay_commits_onto(&repo, &commits, onto_oid)?;
            for commit_oid in &commits {
                let Some(refs_at) = commit_to_refs.get(commit_oid) else {
                    continue;
                };
                if refs_at.iter().any(|r| r == "HEAD") {
                    push_ref_update(&mut updates, &replayed, "HEAD", commit_oid)?;
                }
            }
        }
    }

    let reflog_msg = if let Some(adv) = parsed.advance.as_deref() {
        format!("replay --advance {adv}")
    } else {
        format!("replay --onto {}", onto_oid.to_hex())
    };

    match ref_mode {
        RefAction::Print => {
            let mut out = std::io::stdout().lock();
            for u in &updates {
                writeln!(
                    out,
                    "update {} {} {}",
                    u.refname,
                    u.new_oid.to_hex(),
                    u.old_oid.to_hex()
                )?;
            }
        }
        RefAction::Update => {
            for u in &updates {
                refs::write_ref(&repo.git_dir, &u.refname, &u.new_oid)
                    .with_context(|| format!("failed to update ref {}", u.refname))?;
                if refs::should_autocreate_reflog(&repo.git_dir, &u.refname) {
                    let _ = refs::append_reflog(
                        &repo.git_dir,
                        &u.refname,
                        &u.old_oid,
                        &u.new_oid,
                        &resolve_committer_ident_string(&repo)?,
                        &reflog_msg,
                        false,
                    );
                }
            }
        }
    }

    Ok(())
}

/// One cherry-pick-style tree merge with an empty upstream rename cache (`git rebase`, `git am --3way`).
///
/// Uses merge-ort directory-rename preprocess then [`merge_trees_for_replay`] with directory
/// renames disabled inside the engine (t3401, t6429 part 2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_trees_for_single_cherry_pick(
    repo: &Repository,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    picked_oid: &ObjectId,
    parent_oid: &ObjectId,
    head_oid: &ObjectId,
    favor: MergeFavor,
    base_label_override: Option<&str>,
) -> Result<ReplayTreeMergeResult> {
    let merge_renormalize = read_merge_renormalize(repo);
    let directory_renames = read_directory_renames(repo);
    let rename_opts = MergeRenameOptions::from_config(repo);
    let mut cached_upstream_renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    let base_entries_raw = tree_to_map(tree_to_index_entries(repo, &base_tree, "")?);
    let ours_entries = tree_to_map(tree_to_index_entries(repo, &ours_tree, "")?);
    let theirs_entries_raw = tree_to_map(tree_to_index_entries(repo, &theirs_tree, "")?);

    let changed_paths = collect_changed_paths(&base_entries_raw, &theirs_entries_raw);
    if should_refresh_upstream_rename_cache(
        &base_entries_raw,
        &theirs_entries_raw,
        &cached_upstream_renames,
    ) {
        let detected = detect_side_renames(repo, &base_entries_raw, &ours_entries, true)?;
        cached_upstream_renames = filter_renames_for_changed_paths(detected, &changed_paths);
    }

    if likely_has_rename_candidates(&base_entries_raw, &theirs_entries_raw) {
        let topic_renames =
            detect_side_renames(repo, &base_entries_raw, &theirs_entries_raw, true)?;
        for (old, new) in &topic_renames {
            if cached_upstream_renames.get(old) == Some(new) {
                cached_upstream_renames.remove(old);
            }
        }
    }

    let base_entries = apply_cached_renames(
        &base_entries_raw,
        &cached_upstream_renames,
        &ours_entries,
        Some(&base_entries_raw),
    );
    let theirs_entries = apply_cached_renames(
        &theirs_entries_raw,
        &cached_upstream_renames,
        &ours_entries,
        Some(&base_entries_raw),
    );

    let dir_renames_preprocess_mode = if directory_renames {
        MergeDirectoryRenamesMode::FromConfig
    } else {
        MergeDirectoryRenamesMode::Disabled
    };
    let (ours_for_merge, theirs_for_merge) =
        crate::commands::merge::replay_preprocess_directory_renames_for_trees(
            repo,
            &base_entries,
            &ours_entries,
            &theirs_entries,
            dir_renames_preprocess_mode,
            rename_opts,
        );

    let mut result = merge_trees_for_replay(
        repo,
        &base_entries,
        &ours_for_merge,
        &theirs_for_merge,
        &short_oid(*picked_oid),
        &short_oid(*parent_oid),
        &head_oid.to_hex(),
        &picked_oid.to_hex(),
        favor,
        None,
        merge_renormalize,
        false,
        false,
        false,
        false,
        MergeDirectoryRenamesMode::Disabled,
        rename_opts,
        None,
        base_label_override,
    )?;
    resolve_same_blob_mode_only_replay_conflicts(&mut result);
    Ok(result)
}

fn resolve_same_blob_mode_only_replay_conflicts(result: &mut ReplayTreeMergeResult) {
    let paths = result
        .index
        .entries
        .iter()
        .filter(|entry| entry.stage() != 0)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();

    for path in paths {
        let Some(stage2) = result
            .index
            .entries
            .iter()
            .find(|entry| entry.path == path && entry.stage() == 2)
            .cloned()
        else {
            continue;
        };
        let Some(stage3) = result
            .index
            .entries
            .iter()
            .find(|entry| entry.path == path && entry.stage() == 3)
            .cloned()
        else {
            continue;
        };
        if stage2.oid != stage3.oid || stage2.mode == stage3.mode {
            continue;
        }

        result.index.entries.retain(|entry| entry.path != path);
        let mut resolved = stage3;
        resolved.flags &= 0x0FFF;
        result.index.entries.push(resolved);

        let path_str = String::from_utf8_lossy(&path);
        result
            .conflict_files
            .retain(|(conflict_path, _)| conflict_path != path_str.as_ref());
        result
            .conflict_descriptions
            .retain(|desc| desc.subject_path != path_str.as_ref());
    }

    result.index.sort();
    result.has_conflicts = result.index.entries.iter().any(|entry| entry.stage() != 0)
        || !result.conflict_files.is_empty()
        || !result.conflict_descriptions.is_empty();
}

/// Replay `commits` (oldest first) onto `onto`, returning the new tip and a map from each
/// source commit OID to its replayed OID (used by `grit replay` and `grit history reword`).
pub(crate) fn replay_commits_onto(
    repo: &Repository,
    commits: &[ObjectId],
    mut replayed_tip: ObjectId,
) -> Result<(ObjectId, HashMap<ObjectId, ObjectId>)> {
    let merge_renormalize = read_merge_renormalize(repo);
    let directory_renames = read_directory_renames(repo);
    let merge_dir_mode = MergeDirectoryRenamesMode::Disabled;
    let rename_opts = MergeRenameOptions::from_config(repo);

    let mut replayed: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut cached_upstream_renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    for &commit_oid in commits {
        let commit_obj = repo.odb.read(&commit_oid)?;
        let commit = parse_commit(&commit_obj.data)?;
        let parent_oid = *commit.parents.first().ok_or_else(|| {
            anyhow::anyhow!("fatal: replaying down from root commit is not supported yet!")
        })?;
        if commit.parents.len() > 1 {
            bail!("fatal: replaying merge commits is not supported yet!");
        }

        let base_tree = commit_tree(repo, parent_oid)?;
        let ours_tree = commit_tree(repo, replayed_tip)?;
        let theirs_tree = commit.tree;

        let base_entries_raw = tree_to_map(tree_to_index_entries(repo, &base_tree, "")?);
        let mut ours_entries = tree_to_map(tree_to_index_entries(repo, &ours_tree, "")?);
        let theirs_entries_raw = tree_to_map(tree_to_index_entries(repo, &theirs_tree, "")?);

        let changed_paths = collect_changed_paths(&base_entries_raw, &theirs_entries_raw);
        let should_refresh_upstream = should_refresh_upstream_rename_cache(
            &base_entries_raw,
            &theirs_entries_raw,
            &cached_upstream_renames,
        );
        if should_refresh_upstream {
            let detected = detect_side_renames(repo, &base_entries_raw, &ours_entries, true)?;
            cached_upstream_renames = filter_renames_for_changed_paths(detected, &changed_paths);
        }

        let mut topic_renames = HashMap::new();
        if likely_has_rename_candidates(&base_entries_raw, &theirs_entries_raw) {
            topic_renames =
                detect_side_renames(repo, &base_entries_raw, &theirs_entries_raw, true)?;
            for (old, new) in &topic_renames {
                if cached_upstream_renames.get(old) == Some(new) {
                    cached_upstream_renames.remove(old);
                }
            }
        }

        if directory_renames {
            apply_directory_renames_to_ours_additions(
                &base_entries_raw,
                &mut ours_entries,
                &topic_renames,
                &theirs_entries_raw,
            );
        }

        let base_entries = apply_cached_renames(
            &base_entries_raw,
            &cached_upstream_renames,
            &ours_entries,
            Some(&base_entries_raw),
        );
        let theirs_entries = apply_cached_renames(
            &theirs_entries_raw,
            &cached_upstream_renames,
            &ours_entries,
            Some(&base_entries_raw),
        );

        let merge_result = merge_trees_for_replay(
            repo,
            &base_entries,
            &ours_entries,
            &theirs_entries,
            &short_oid(commit_oid),
            &short_oid(parent_oid),
            &replayed_tip.to_hex(),
            &commit_oid.to_hex(),
            MergeFavor::None,
            None,
            merge_renormalize,
            false,
            false,
            false,
            false,
            merge_dir_mode,
            rename_opts,
            None,
            None,
        )?;
        if merge_result.has_conflicts {
            let reason = merge_result
                .conflict_descriptions
                .first()
                .map(|entry| entry.subject_path.as_str())
                .unwrap_or("conflict");
            bail!("replay stopped due to merge conflict in {reason}");
        }

        let merged_tree = write_tree_from_index(&repo.odb, &merge_result.index, "")?;

        if merged_tree == ours_tree && theirs_tree != base_tree {
            replayed.insert(commit_oid, replayed_tip);
            continue;
        }

        replayed_tip = create_replayed_commit(repo, replayed_tip, merged_tree, &commit)?;
        replayed.insert(commit_oid, replayed_tip);
    }

    Ok((replayed_tip, replayed))
}

fn resolve_committer_ident_string(repo: &Repository) -> Result<String> {
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    Ok(resolve_committer_ident(&config, now))
}

fn commit_tree(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&commit_oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        bail!("expected tree object");
    }
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for te in entries {
        let name = String::from_utf8_lossy(&te.name).into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if te.mode == 0o040000 {
            result.extend(tree_to_index_entries(repo, &te.oid, &path)?);
        } else {
            let path_bytes = path.into_bytes();
            result.push(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: te.mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: te.oid,
                flags: path_bytes.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path_bytes,
                base_index_pos: 0,
            });
        }
    }
    Ok(result)
}

fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for e in entries {
        out.insert(e.path.clone(), e);
    }
    out
}

fn create_replayed_commit(
    repo: &Repository,
    parent: ObjectId,
    tree: ObjectId,
    based_on: &CommitData,
) -> Result<ObjectId> {
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    let committer = resolve_committer_ident(&config, now);
    let commit = CommitData {
        tree,
        parents: vec![parent],
        author: based_on.author.clone(),
        committer,
        author_raw: based_on.author_raw.clone(),
        committer_raw: based_on.committer_raw.clone(),
        encoding: based_on.encoding.clone(),
        message: based_on.message.clone(),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit);
    repo.odb
        .write(ObjectKind::Commit, &bytes)
        .context("failed to write replayed commit")
}

fn resolve_committer_ident(config: &ConfigSet, now: OffsetDateTime) -> String {
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    let timestamp = format!("{epoch} {hours:+03}{minutes:02}");
    format!("{name} <{email}> {timestamp}")
}

fn read_merge_renormalize(repo: &Repository) -> bool {
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get("merge.renormalize"))
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

fn read_directory_renames(repo: &Repository) -> bool {
    // Replay/sequencer enables directory-rename *preprocess* only when this key is set
    // explicitly (Git documents `merge.directoryRenames`; tests use `-c` or repo config).
    // Do not mirror merge-ort's "default on when unset" here — that changes replay caching
    // and trace expectations (t6429) without matching upstream replay integration yet.
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| {
            c.get("merge.directoryRenames")
                .or_else(|| c.get("merge.directoryrenames"))
        })
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "true" | "yes" | "on" | "1" | "conflict" | "")
        })
        .unwrap_or(false)
}

fn short_oid(oid: ObjectId) -> String {
    let hex = oid.to_hex();
    hex[..7.min(hex.len())].to_owned()
}

fn likely_has_rename_candidates(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
) -> bool {
    let has_delete = base.keys().any(|path| !side.contains_key(path));
    let has_add = side.keys().any(|path| !base.contains_key(path));
    has_delete && has_add
}

fn collect_changed_paths(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
) -> Vec<Vec<u8>> {
    let mut all = BTreeSet::new();
    all.extend(base.keys().cloned());
    all.extend(side.keys().cloned());

    let mut changed = Vec::new();
    for path in all {
        match (base.get(&path), side.get(&path)) {
            (Some(be), Some(se)) if be.oid == se.oid && be.mode == se.mode => {}
            _ => changed.push(path),
        }
    }
    changed
}

fn filter_renames_for_changed_paths(
    renames: HashMap<Vec<u8>, Vec<u8>>,
    changed_paths: &[Vec<u8>],
) -> HashMap<Vec<u8>, Vec<u8>> {
    if changed_paths.is_empty() {
        return renames;
    }
    let mut kept = HashMap::new();
    let mut changed_dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in changed_paths {
        let dir = parent_dir(path);
        if !dir.is_empty() {
            changed_dirs.insert(dir);
        }
    }

    for (old, new) in &renames {
        let old_dir = parent_dir(old);
        let new_dir = parent_dir(new);
        let matched = changed_paths.iter().any(|path| {
            path == old
                || path == new
                || path.starts_with(old)
                || path.starts_with(new)
                || (!old_dir.is_empty() && parent_dir(path) == old_dir)
                || (!new_dir.is_empty() && parent_dir(path) == new_dir)
        });
        if matched {
            kept.insert(old.clone(), new.clone());
            if !old_dir.is_empty() {
                changed_dirs.insert(old_dir.clone());
            }
            if !new_dir.is_empty() {
                changed_dirs.insert(new_dir.clone());
            }
        }
    }

    if !changed_dirs.is_empty() {
        for (old, new) in renames {
            if kept.contains_key(&old) {
                continue;
            }
            let old_dir = parent_dir(&old);
            let new_dir = parent_dir(&new);
            if (!old_dir.is_empty() && changed_dirs.contains(&old_dir))
                || (!new_dir.is_empty() && changed_dirs.contains(&new_dir))
            {
                kept.insert(old, new);
            }
        }
    }

    kept
}

fn apply_directory_renames_to_ours_additions(
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &mut HashMap<Vec<u8>, IndexEntry>,
    theirs_renames: &HashMap<Vec<u8>, Vec<u8>>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) {
    if theirs_renames.is_empty() {
        return;
    }
    let dir_map = build_directory_rename_map(theirs_renames, theirs);
    if dir_map.is_empty() {
        return;
    }

    let keys: Vec<Vec<u8>> = ours.keys().cloned().collect();
    for key in keys {
        if base.contains_key(&key) {
            continue;
        }
        for (old_dir, new_dir) in &dir_map {
            if let Some(new_path) = replace_directory_prefix(&key, old_dir, new_dir) {
                if ours.contains_key(&new_path) {
                    break;
                }
                if let Some(mut entry) = ours.remove(&key) {
                    entry.path = new_path.clone();
                    ours.insert(new_path, entry);
                }
                break;
            }
        }
    }
}

fn parent_dir(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|b| *b == b'/') {
        Some(pos) => path[..pos].to_vec(),
        None => Vec::new(),
    }
}

fn apply_cached_renames(
    entries: &HashMap<Vec<u8>, IndexEntry>,
    renames: &HashMap<Vec<u8>, Vec<u8>>,
    side_snapshot: &HashMap<Vec<u8>, IndexEntry>,
    base_snapshot: Option<&HashMap<Vec<u8>, IndexEntry>>,
) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = entries.clone();
    for (old, new) in renames {
        if let Some(mut entry) = out.remove(old) {
            if out.contains_key(new) {
                out.insert(old.clone(), entry);
                continue;
            }
            entry.path = new.clone();
            out.insert(new.clone(), entry);
        }
    }

    let dir_map = build_directory_rename_map(renames, side_snapshot);
    let exact_sources: BTreeSet<Vec<u8>> = renames.keys().cloned().collect();
    let keys: Vec<Vec<u8>> = out.keys().cloned().collect();
    for key in keys {
        if exact_sources.contains(&key) {
            continue;
        }
        for (old_dir, new_dir) in &dir_map {
            if old_dir.is_empty() || new_dir.is_empty() {
                continue;
            }
            if let Some(base) = base_snapshot {
                if !dir_exists_in_tree(base, old_dir) {
                    continue;
                }
            }
            if let Some(new_path) = replace_directory_prefix(&key, old_dir, new_dir) {
                if out.contains_key(&new_path) {
                    continue;
                }
                if let Some(mut entry) = out.remove(&key) {
                    entry.path = new_path.clone();
                    out.insert(new_path, entry);
                }
                break;
            }
        }
    }

    out
}

fn dir_exists_in_tree(entries: &HashMap<Vec<u8>, IndexEntry>, dir: &[u8]) -> bool {
    entries.keys().any(|path| {
        path.len() > dir.len() && path.starts_with(dir) && path.get(dir.len()) == Some(&b'/')
    })
}

fn replace_directory_prefix(path: &[u8], old_dir: &[u8], new_dir: &[u8]) -> Option<Vec<u8>> {
    if !path.starts_with(old_dir) {
        return None;
    }
    if path.len() == old_dir.len() || path.get(old_dir.len()) != Some(&b'/') {
        return None;
    }
    let mut out = Vec::with_capacity(new_dir.len() + (path.len() - old_dir.len()));
    out.extend_from_slice(new_dir);
    out.extend_from_slice(&path[old_dir.len()..]);
    Some(out)
}

fn build_directory_rename_map(
    renames: &HashMap<Vec<u8>, Vec<u8>>,
    side_snapshot: &HashMap<Vec<u8>, IndexEntry>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut counts: HashMap<(Vec<u8>, Vec<u8>), usize> = HashMap::new();
    for (old, new) in renames {
        let old_dir = parent_dir(old);
        let new_dir = parent_dir(new);
        if old_dir == new_dir {
            continue;
        }
        *counts.entry((old_dir, new_dir)).or_insert(0) += 1;
    }

    let mut best_for_old: HashMap<Vec<u8>, (Vec<u8>, usize)> = HashMap::new();
    for ((old_dir, new_dir), count) in counts {
        if old_dir.is_empty() || old_dir_still_exists_in_side(&old_dir, side_snapshot) {
            continue;
        }
        match best_for_old.get(&old_dir) {
            Some((_, best)) if *best >= count => {}
            _ => {
                best_for_old.insert(old_dir, (new_dir, count));
            }
        }
    }

    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = best_for_old
        .into_iter()
        .map(|(old, (new, _))| (old, new))
        .collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    pairs
}

fn build_directory_rename_map_unconditional_with_counts(
    renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<(Vec<u8>, Vec<u8>, usize)> {
    let mut counts: HashMap<(Vec<u8>, Vec<u8>), usize> = HashMap::new();
    for (old, new) in renames {
        let old_dir = parent_dir(old);
        let new_dir = parent_dir(new);
        if old_dir == new_dir {
            continue;
        }
        *counts.entry((old_dir, new_dir)).or_insert(0) += 1;
    }

    let mut best_for_old: HashMap<Vec<u8>, (Vec<u8>, usize)> = HashMap::new();
    for ((old_dir, new_dir), count) in counts {
        if old_dir.is_empty() {
            continue;
        }
        match best_for_old.get(&old_dir) {
            Some((_, best)) if *best >= count => {}
            _ => {
                best_for_old.insert(old_dir, (new_dir, count));
            }
        }
    }

    let mut pairs: Vec<(Vec<u8>, Vec<u8>, usize)> = best_for_old
        .into_iter()
        .map(|(old, (new, count))| (old, new, count))
        .collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    pairs
}

fn detect_side_renames(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    trace_rename_call: bool,
) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
    let threshold = 50u32;
    let rename_limit: usize = {
        let config = ConfigSet::load(Some(&repo.git_dir), true).ok();
        config
            .as_ref()
            .and_then(|c| c.get("merge.renamelimit"))
            .or_else(|| config.as_ref().and_then(|c| c.get("diff.renamelimit")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000)
    };
    let zero_oid = ObjectId::zero();

    let mut side_oid_to_paths: HashMap<ObjectId, Vec<Vec<u8>>> = HashMap::new();
    for (path, entry) in side {
        side_oid_to_paths
            .entry(entry.oid)
            .or_default()
            .push(path.clone());
    }

    let mut exact_renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for (base_path, base_entry) in base {
        if let Some(side_entry) = side.get(base_path) {
            if side_entry.oid == base_entry.oid && side_entry.mode == base_entry.mode {
                continue;
            }
        }
        if let Some(side_paths) = side_oid_to_paths.get(&base_entry.oid) {
            for sp in side_paths {
                if sp != base_path && !base.contains_key(sp) {
                    exact_renames.insert(base_path.clone(), sp.clone());
                    break;
                }
            }
        }
    }

    let rename_targets: BTreeSet<Vec<u8>> = exact_renames.values().cloned().collect();
    let rename_sources: BTreeSet<Vec<u8>> = exact_renames.keys().cloned().collect();

    let mut diff_entries = Vec::new();
    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys());
    all_paths.extend(side.keys());
    for path in all_paths {
        match (base.get(path), side.get(path)) {
            (Some(be), None) => {
                if !rename_sources.contains(path) {
                    diff_entries.push(DiffEntry {
                        status: DiffStatus::Deleted,
                        old_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                        new_path: None,
                        old_mode: format!("{:06o}", be.mode),
                        new_mode: String::new(),
                        old_oid: be.oid,
                        new_oid: zero_oid,
                        score: None,
                    });
                }
            }
            (None, Some(se)) => {
                if !rename_targets.contains(path) {
                    diff_entries.push(DiffEntry {
                        status: DiffStatus::Added,
                        old_path: None,
                        new_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                        old_mode: String::new(),
                        new_mode: format!("{:06o}", se.mode),
                        old_oid: zero_oid,
                        new_oid: se.oid,
                        score: None,
                    });
                }
            }
            (Some(be), Some(se)) => {
                if rename_sources.contains(path) && be.oid != se.oid {
                    diff_entries.push(DiffEntry {
                        status: DiffStatus::Deleted,
                        old_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                        new_path: None,
                        old_mode: format!("{:06o}", be.mode),
                        new_mode: String::new(),
                        old_oid: be.oid,
                        new_oid: zero_oid,
                        score: None,
                    });
                }
            }
            _ => {}
        }
    }

    let n_deleted = diff_entries
        .iter()
        .filter(|e| matches!(e.status, DiffStatus::Deleted))
        .count();
    let n_added = diff_entries
        .iter()
        .filter(|e| matches!(e.status, DiffStatus::Added))
        .count();
    if trace_rename_call && likely_has_rename_candidates(base, side) {
        if let Ok(path) = std::env::var("GIT_TRACE2_PERF") {
            if !path.is_empty() {
                let _ = append_trace2_perf_line(&path, "region_enter", "diffcore_rename");
            }
        }
    }

    let mut renames = exact_renames;
    let mut matched_targets: BTreeSet<Vec<u8>> = renames.values().cloned().collect();
    let detected = if n_deleted > rename_limit || n_added > rename_limit {
        Vec::new()
    } else {
        detect_renames(&repo.odb, None, diff_entries.clone(), threshold)
    };
    for e in detected {
        if matches!(e.status, DiffStatus::Renamed) {
            if let (Some(old), Some(new)) = (e.old_path, e.new_path) {
                let old_bytes = old.as_bytes().to_vec();
                let new_bytes = new.as_bytes().to_vec();
                if !renames.contains_key(&old_bytes) && !matched_targets.contains(&new_bytes) {
                    renames.insert(old_bytes, new_bytes.clone());
                    matched_targets.insert(new_bytes);
                }
            }
        }
    }

    let mut deleted_by_name: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    let mut added_by_name: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    for entry in &diff_entries {
        match entry.status {
            DiffStatus::Deleted => {
                if let Some(old) = &entry.old_path {
                    let old_bytes = old.as_bytes().to_vec();
                    if renames.contains_key(&old_bytes) {
                        continue;
                    }
                    deleted_by_name
                        .entry(path_basename(&old_bytes))
                        .or_default()
                        .push(old_bytes);
                }
            }
            DiffStatus::Added => {
                if let Some(new) = &entry.new_path {
                    let new_bytes = new.as_bytes().to_vec();
                    if matched_targets.contains(&new_bytes) {
                        continue;
                    }
                    added_by_name
                        .entry(path_basename(&new_bytes))
                        .or_default()
                        .push(new_bytes);
                }
            }
            _ => {}
        }
    }
    for (name, deleted_paths) in deleted_by_name {
        if deleted_paths.len() != 1 {
            continue;
        }
        let Some(added_paths) = added_by_name.get(&name) else {
            continue;
        };
        if added_paths.len() != 1 {
            continue;
        }
        let old_path = deleted_paths[0].clone();
        let new_path = added_paths[0].clone();
        if !renames.contains_key(&old_path) && !matched_targets.contains(&new_path) {
            renames.insert(old_path, new_path.clone());
            matched_targets.insert(new_path);
        }
    }

    Ok(renames)
}

fn path_basename(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|b| *b == b'/') {
        Some(pos) => path[pos + 1..].to_vec(),
        None => path.to_vec(),
    }
}

fn should_refresh_upstream_rename_cache(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    cached_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> bool {
    if cached_renames.is_empty() {
        return true;
    }

    let changed_paths = collect_changed_paths(base, side);
    let mut covered_dir_prefixes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in changed_paths {
        if path_covered_by_cached_renames(&path, cached_renames) {
            covered_dir_prefixes.insert(parent_dir(&path));
            continue;
        }
        let path_parent = parent_dir(&path);
        if !path_parent.is_empty() && covered_dir_prefixes.contains(&path_parent) {
            continue;
        }
        if base.contains_key(&path) || parent_exists_in_base(&path, base) {
            return true;
        }
    }

    false
}

fn path_covered_by_cached_renames(path: &[u8], renames: &HashMap<Vec<u8>, Vec<u8>>) -> bool {
    if renames
        .iter()
        .any(|(old, new)| path == old.as_slice() || path == new.as_slice())
    {
        return true;
    }
    let dir_map = build_directory_rename_map_unconditional_with_counts(renames);
    dir_map.iter().any(|(old_dir, new_dir, support_count)| {
        *support_count >= 2
            && ((!old_dir.is_empty()
                && path.starts_with(old_dir)
                && path.len() > old_dir.len()
                && path.get(old_dir.len()) == Some(&b'/'))
                || (!new_dir.is_empty()
                    && path.starts_with(new_dir)
                    && path.len() > new_dir.len()
                    && path.get(new_dir.len()) == Some(&b'/')))
    })
}

fn parent_exists_in_base(path: &[u8], base: &HashMap<Vec<u8>, IndexEntry>) -> bool {
    let parent = parent_dir(path);
    if parent.is_empty() {
        return false;
    }
    base.keys().any(|candidate| {
        candidate.len() > parent.len()
            && candidate.starts_with(&parent)
            && candidate.get(parent.len()) == Some(&b'/')
    })
}

fn old_dir_still_exists_in_side(
    old_dir: &[u8],
    side_snapshot: &HashMap<Vec<u8>, IndexEntry>,
) -> bool {
    side_snapshot.keys().any(|path| {
        path.len() > old_dir.len()
            && path.starts_with(old_dir)
            && path.get(old_dir.len()) == Some(&b'/')
    })
}

fn append_trace2_perf_line(path: &str, event: &str, data: &str) -> Result<()> {
    let now = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = now.as_secs();
        let micros = now.subsec_micros();
        let secs_in_day = total_secs % 86400;
        let hours = secs_in_day / 3600;
        let mins = (secs_in_day % 3600) / 60;
        let secs = secs_in_day % 60;
        format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
    };

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(Path::new(path))?;
    writeln!(
        file,
        "{} grit:0  | d0 | main                     | {:<12} |     |           |           |              | {}",
        now, event, data
    )?;
    Ok(())
}
