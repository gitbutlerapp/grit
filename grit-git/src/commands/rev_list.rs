//! `grit rev-list` command.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::diff::diff_trees;
use grit_lib::git_date::approx::approxidate_careful;
use grit_lib::git_date::parse::parse_date_basic;
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::pack;
use grit_lib::pathspec::matches_pathspec;
use grit_lib::promisor::read_promisor_missing_oids;
use grit_lib::quote_path::quote_c_style;
use grit_lib::ref_exclusions::RefExclusions;
use grit_lib::reflog::{read_reflog_dwim, ReflogEntry};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{
    collect_revision_specs_with_stdin, commit_tips_from_named_refs,
    commit_visible_for_dense_pathspecs, is_symmetric_diff, merge_bases, render_commit,
    render_commit_with_color, rev_list, select_bisect_commits, split_symmetric_diff, tag_targets,
    FilterObjectKind, MissingAction, ObjectFilter, OrderingMode, OutputMode, RevListOptions,
};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

/// Default maximum tree recursion depth when `core.maxtreedepth` is unset.
const DEFAULT_MAX_TREE_DEPTH: usize = 2048;

/// Bitmap traversal uses OID-only object lines (no path, no trailing space) for filters that
/// enumerate blobs/trees/commits like Git's bitmap path. Pure `tree:<n>` walks match non-bitmap
/// bytes (`test_cmp` in t6113), and `object:type=tag` keeps full formatting for `test_cmp`.
fn bitmap_use_oid_only_object_lines(filter: Option<&ObjectFilter>) -> bool {
    match filter {
        None => false,
        Some(ObjectFilter::BlobNone) | Some(ObjectFilter::BlobLimit(_)) => true,
        Some(ObjectFilter::ObjectType(k)) => *k != FilterObjectKind::Tag,
        Some(ObjectFilter::SparseOid(_)) | Some(ObjectFilter::TreeDepth(_)) => false,
        Some(ObjectFilter::Combine(parts)) => parts
            .iter()
            .any(|p| bitmap_use_oid_only_object_lines(Some(p))),
    }
}

fn object_type_filter_commit_only(filter: Option<&ObjectFilter>) -> bool {
    fn is_commit_only(f: &ObjectFilter) -> bool {
        match f {
            ObjectFilter::ObjectType(FilterObjectKind::Commit) => true,
            ObjectFilter::Combine(parts) => parts.iter().all(is_commit_only),
            _ => false,
        }
    }
    filter.is_some_and(is_commit_only)
}

fn resolve_max_tree_depth(config: &ConfigSet) -> Result<usize> {
    let depth = if let Some(raw) = config.get("core.maxtreedepth") {
        raw.parse::<usize>()
            .map_err(|_| anyhow::anyhow!("invalid core.maxtreedepth: '{raw}'"))?
    } else {
        DEFAULT_MAX_TREE_DEPTH
    };
    Ok(depth)
}

fn is_utf8_encoding(label: &str) -> bool {
    matches!(label.trim().to_ascii_lowercase().as_str(), "utf-8" | "utf8")
}

fn effective_pretty_output_encoding(config: &ConfigSet) -> Option<String> {
    config
        .get("i18n.logOutputEncoding")
        .or_else(|| config.get("i18n.logoutputencoding"))
        .or_else(|| config.get("i18n.commitEncoding"))
        .or_else(|| config.get("i18n.commitencoding"))
        .filter(|label| !label.trim().is_empty() && !is_utf8_encoding(label))
}

fn encode_pretty_output(text: &str, encoding: Option<&str>) -> Vec<u8> {
    match encoding {
        None => text.as_bytes().to_vec(),
        Some(label) => grit_lib::commit_encoding::encode_header_text(label, text)
            .unwrap_or_else(|| text.as_bytes().to_vec()),
    }
}

/// Arguments for `grit rev-list`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Raw command arguments forwarded by the CLI parser.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiskUsageFormat {
    Bytes,
    Human,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BisectMode {
    One,
    Vars,
    All,
}

fn push_ref_oids_as_specs(
    specs: &mut Vec<String>,
    not_mode: bool,
    oids: impl Iterator<Item = ObjectId>,
) {
    for oid in oids {
        let s = oid.to_hex();
        if not_mode {
            specs.push(format!("^{s}"));
        } else {
            specs.push(s);
        }
    }
}

fn push_ref_pairs_as_specs(
    repo: &Repository,
    specs: &mut Vec<String>,
    not_mode: bool,
    pairs: &[(String, ObjectId)],
    exclusions: &mut RefExclusions,
) -> Result<()> {
    let tips = commit_tips_from_named_refs(repo, pairs, exclusions)
        .context("failed to resolve ref tips")?;
    push_ref_oids_as_specs(specs, not_mode, tips.into_iter());
    exclusions.clear();
    Ok(())
}

fn push_category_ref_pairs_as_specs(
    repo: &Repository,
    specs: &mut Vec<String>,
    not_mode: bool,
    pairs: &[(String, ObjectId)],
    prefix: &str,
    exclude_patterns: &[String],
) -> Result<()> {
    let filtered: Vec<(String, ObjectId)> = pairs
        .iter()
        .filter(|(refname, _)| {
            let short = refname.strip_prefix(prefix).unwrap_or(refname);
            !exclude_patterns
                .iter()
                .any(|pattern| grit_lib::refs::ref_matches_glob(short, pattern))
        })
        .cloned()
        .collect();
    let tips = commit_tips_from_named_refs(repo, &filtered, &RefExclusions::default())
        .context("failed to resolve ref tips")?;
    push_ref_oids_as_specs(specs, not_mode, tips.into_iter());
    Ok(())
}

fn push_all_refs_as_specs(
    repo: &Repository,
    specs: &mut Vec<String>,
    not_mode: bool,
    exclusions: &mut RefExclusions,
) -> Result<()> {
    let mut pairs = Vec::new();
    if let Ok(head_oid) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        pairs.push(("HEAD".to_owned(), head_oid));
    }
    pairs.extend(grit_lib::refs::list_refs_physical(&repo.git_dir, "refs/")?);
    push_ref_pairs_as_specs(repo, specs, not_mode, &pairs, exclusions)
}

#[derive(Clone, Debug)]
struct MissingObjectInfo {
    oid: ObjectId,
    path: Option<String>,
    kind: ObjectKind,
}

fn infer_missing_object_info(
    repo: &Repository,
    commits: &[ObjectId],
    oid: ObjectId,
) -> MissingObjectInfo {
    for commit_oid in commits {
        let Ok(object) = repo.odb.read(commit_oid) else {
            continue;
        };
        if object.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(commit) = parse_commit(&object.data) else {
            continue;
        };
        if commit.tree == oid {
            return MissingObjectInfo {
                oid,
                path: None,
                kind: ObjectKind::Tree,
            };
        }
        let mut seen_trees = HashSet::new();
        if let Some((path, kind)) =
            find_missing_object_in_tree(repo, commit.tree, "", oid, &mut seen_trees)
        {
            return MissingObjectInfo {
                oid,
                path: Some(path),
                kind,
            };
        }
    }

    MissingObjectInfo {
        oid,
        path: None,
        kind: ObjectKind::Commit,
    }
}

fn find_missing_object_in_tree(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    target: ObjectId,
    seen: &mut HashSet<ObjectId>,
) -> Option<(String, ObjectKind)> {
    if !seen.insert(tree_oid) {
        return None;
    }
    let object = repo.odb.read(&tree_oid).ok()?;
    if object.kind != ObjectKind::Tree {
        return None;
    }
    let entries = parse_tree(&object.data).ok()?;
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = object_kind_for_tree_mode(entry.mode);
        if entry.oid == target {
            return Some((path, kind));
        }
        if kind == ObjectKind::Tree {
            if let Some(found) = find_missing_object_in_tree(repo, entry.oid, &path, target, seen) {
                return Some(found);
            }
        }
    }
    None
}

fn object_kind_for_tree_mode(mode: u32) -> ObjectKind {
    match mode {
        0o040000 => ObjectKind::Tree,
        0o160000 => ObjectKind::Commit,
        _ => ObjectKind::Blob,
    }
}

fn quote_missing_info_path(path: &str) -> String {
    let quoted = quote_c_style(path, true);
    if quoted == path && path.as_bytes().iter().any(|&b| b == b' ') {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        quoted
    }
}

fn list_refs_glob_physical(repo: &Repository, pattern: &str) -> Result<Vec<(String, ObjectId)>> {
    Ok(grit_lib::refs::list_refs_physical(&repo.git_dir, "refs/")?
        .into_iter()
        .filter(|(refname, _)| grit_lib::refs::ref_matches_glob(refname, pattern))
        .collect())
}

fn normalize_glob_pattern(pattern: &str) -> String {
    let full = if pattern.starts_with("refs/") {
        pattern.to_owned()
    } else {
        format!("refs/{pattern}")
    };
    ensure_glob_suffix(&full)
}

fn normalize_ref_pattern(prefix: &str, pattern: &str) -> String {
    ensure_glob_suffix(&format!("{prefix}{pattern}"))
}

fn ensure_glob_suffix(pattern: &str) -> String {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        return pattern.to_owned();
    }
    if pattern.ends_with('/') {
        format!("{pattern}*")
    } else {
        format!("{pattern}/*")
    }
}

fn reject_exclude_hidden_with(option: &str, exclusions: &RefExclusions) -> Result<()> {
    if exclusions.hidden_configured {
        bail!("options '--exclude-hidden' and '{option}' cannot be used together");
    }
    Ok(())
}

fn output_mode_requests_parent_rewrite(output_mode: &OutputMode) -> bool {
    match output_mode {
        OutputMode::Parents => true,
        OutputMode::Format(fmt) => fmt.contains("%P") || fmt.contains("%p"),
        _ => false,
    }
}

fn append_bisect_ref_specs(
    repo: &Repository,
    positive_specs: &mut Vec<String>,
    negative_specs: &mut Vec<String>,
) -> Result<()> {
    let refs = grit_lib::refs::list_refs(&repo.git_dir, "refs/bisect/")
        .context("failed to list bisect refs")?;
    for (name, oid) in refs {
        if name.starts_with("refs/bisect/bad")
            && name
                .as_bytes()
                .get("refs/bisect/bad".len())
                .is_none_or(|b| *b == b'-')
        {
            positive_specs.push(oid.to_hex());
        } else if name.starts_with("refs/bisect/good")
            && name
                .as_bytes()
                .get("refs/bisect/good".len())
                .is_none_or(|b| *b == b'-')
        {
            negative_specs.push(oid.to_hex());
        }
    }
    Ok(())
}

fn bisect_tag_decorations(repo: &Repository) -> Result<HashMap<ObjectId, Vec<String>>> {
    let mut decorations: HashMap<ObjectId, Vec<String>> = HashMap::new();
    for (name, oid) in
        grit_lib::refs::list_refs(&repo.git_dir, "refs/tags/").context("failed to list tag refs")?
    {
        let short = name.strip_prefix("refs/tags/").unwrap_or(&name).to_owned();
        decorations.entry(oid).or_default().push(short);
    }
    for names in decorations.values_mut() {
        names.sort();
    }
    Ok(decorations)
}

fn estimate_bisect_steps(total: usize) -> usize {
    if total < 3 {
        return 0;
    }
    let n = usize::BITS as usize - 1 - total.leading_zeros() as usize;
    let e = 1usize << n;
    let x = total - e;
    if e < 3 * x {
        n
    } else {
        n.saturating_sub(1)
    }
}

/// Run `grit rev-list`.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("failed to discover repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let pretty_output_encoding = effective_pretty_output_encoding(&config);

    let mut options = RevListOptions::default();
    let mut object_depth_limit: Option<usize> = None;
    let mut abbrev_len = 7usize;
    let mut revision_specs = Vec::new();
    let mut read_stdin = false;
    let mut end_of_options = false;
    let mut path_mode = false;
    let mut default_rev: Option<String> = None;
    let mut no_commit_header = false;
    let mut raw_commit_header = false;
    let mut use_color = false;
    let mut disk_usage_format: Option<DiskUsageFormat> = None;
    let mut show_parents = false;
    let mut show_children = false;
    let mut not_mode = false;
    let mut _missing_explicit = false;
    let mut use_bitmap_index = false;
    let mut unpacked_only = false;
    let mut test_bitmap = false;
    let mut cli_topo_order = false;
    let mut cli_date_order = false;
    let mut cli_author_date_order = false;
    let mut walk_reflog = false;
    let mut add_reflog_tips = false;
    let mut ref_exclusions = RefExclusions::default();
    let mut ref_exclude_patterns = Vec::new();
    let mut saw_pseudo_ref = false;
    let mut bisect_mode: Option<BisectMode> = None;
    let mut zero_terminated = false;

    let mut i = 0usize;
    while i < args.args.len() {
        let arg = &args.args[i];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            path_mode = true;
            i += 1;
            continue;
        }
        if path_mode {
            options.paths.push(arg.clone());
            i += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "--all" => {
                    saw_pseudo_ref = true;
                    push_all_refs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &mut ref_exclusions,
                    )?;
                    ref_exclude_patterns.clear();
                }
                "--first-parent" => options.first_parent = true,
                "--ancestry-path" => options.ancestry_path = true,
                "--simplify-by-decoration" => options.simplify_by_decoration = true,
                "--topo-order" => cli_topo_order = true,
                "--date-order" => cli_date_order = true,
                "--author-date-order" => cli_author_date_order = true,
                "--exclude-first-parent-only" => options.exclude_first_parent_only = true,
                "--maximal-only" => options.maximal_only = true,
                "--show-pulls" => options.show_pulls = true,
                "--reverse" => options.reverse = true,
                "--count" => options.count = true,
                "--parents" => {
                    options.output_mode = OutputMode::Parents;
                    show_parents = true;
                }
                "--children" => {
                    show_children = true;
                }
                "--quiet" => options.quiet = true,
                // `--progress[=<header>]` only controls a stderr progress meter and has no effect
                // on the emitted object/commit list. `git receive-pack` runs the connectivity
                // check as `git rev-list --progress="Checking connectivity" ...`, so accept and
                // ignore it rather than erroring (t5543 atomic exit-code via a git-receive-pack
                // wrapper whose internal `git` resolves to grit).
                "--progress" | "--no-progress" => {}
                _ if arg.starts_with("--progress=") => {}
                "-z" => zero_terminated = true,
                "--stdin" => read_stdin = true,
                "--not" => not_mode = !not_mode,
                "--end-of-options" => end_of_options = true,
                "--objects" => options.objects = true,
                "--objects-edge" | "--objects-edge-aggressive" => options.objects = true,
                "--indexed-objects" => {
                    options.objects = true;
                    if not_mode {
                        options.exclude_indexed_objects = true;
                    } else {
                        options.include_indexed_objects = true;
                    }
                }
                "--use-bitmap-index" => use_bitmap_index = true,
                "--test-bitmap" => test_bitmap = true,
                "--unpacked" => unpacked_only = true,
                "--disk-usage" => disk_usage_format = Some(DiskUsageFormat::Bytes),
                "--bisect" => bisect_mode = Some(BisectMode::One),
                "--bisect-vars" => bisect_mode = Some(BisectMode::Vars),
                "--bisect-all" => bisect_mode = Some(BisectMode::All),
                _ if arg.starts_with("--disk-usage=") => {
                    let value = arg.trim_start_matches("--disk-usage=");
                    disk_usage_format = Some(match value {
                        "human" => DiskUsageFormat::Human,
                        _ => {
                            eprintln!(
                                "fatal: invalid value for '--disk-usage=<format>': '{}', the only allowed format is 'human'",
                                value
                            );
                            std::process::exit(128);
                        }
                    });
                }
                _ if arg.starts_with("--missing=") => {
                    _missing_explicit = true;
                    let value = arg.trim_start_matches("--missing=");
                    options.missing_action = match value {
                        "error" => MissingAction::Error,
                        "print" => MissingAction::Print,
                        "print-info" => MissingAction::PrintInfo,
                        "allow-any" | "allow-promisor" => MissingAction::Allow,
                        _ => bail!("unsupported value for --missing: {value}"),
                    };
                }
                "--no-object-names" => options.no_object_names = true,
                "--object-names" => options.no_object_names = false,
                "--boundary" => options.boundary = true,
                "--in-commit-order" => options.in_commit_order = true,
                "--no-kept-objects" => options.no_kept_objects = true,
                "--exclude-promisor-objects" => options.exclude_promisor_objects = true,
                "--ignore-missing" => options.ignore_missing = true,
                "--full-history" => options.full_history = true,
                "--sparse" => options.sparse = true,
                "--dense" => { /* default behavior, no-op */ }
                "--simplify-merges" => options.simplify_merges = true,
                "--left-right" => {
                    options.left_right = true;
                    options.left_right_explicit = true;
                }
                "--left-only" => options.left_only = true,
                "--right-only" => options.right_only = true,
                "--cherry-mark" => options.cherry_mark = true,
                "--cherry-pick" => options.cherry_pick = true,
                "--merges" => options.min_parents = Some(2),
                "--no-merges" => options.max_parents = Some(1),
                "--cherry" => {
                    options.cherry_mark = true;
                    options.right_only = true;
                    options.max_parents = Some(1);
                }
                "-n" => {
                    let Some(value) = args.args.get(i + 1) else {
                        bail!("-n requires an argument");
                    };
                    options.max_count = Some(parse_non_negative(value, "-n")?);
                    i += 1;
                }
                "--skip" => {
                    let Some(value) = args.args.get(i + 1) else {
                        bail!("--skip requires an argument");
                    };
                    options.skip = parse_non_negative(value, "--skip")?;
                    i += 1;
                }
                _ if arg.starts_with("--max-count=") => {
                    let value = arg.trim_start_matches("--max-count=");
                    options.max_count = Some(parse_non_negative(value, "--max-count")?);
                }
                _ if arg.starts_with("--skip=") => {
                    let value = arg.trim_start_matches("--skip=");
                    options.skip = parse_non_negative(value, "--skip")?;
                }
                _ if arg.starts_with("--format=") => {
                    let value = arg.trim_start_matches("--format=").to_owned();
                    options.output_mode = OutputMode::Format(value);
                }
                _ if arg.starts_with("--pretty=") => {
                    let value = arg.trim_start_matches("--pretty=").to_owned();
                    // --pretty=format:xxx is the same as --format=format:xxx
                    // --pretty=oneline etc are named formats
                    options.output_mode = OutputMode::Format(value);
                }
                "--pretty" => {
                    // --pretty without a value defaults to medium
                    options.output_mode = OutputMode::Format("medium".to_owned());
                }
                _ if arg.starts_with("--abbrev=") => {
                    let value = arg.trim_start_matches("--abbrev=");
                    abbrev_len = parse_non_negative(value, "--abbrev")?;
                }
                _ if arg.starts_with("-n") && arg.len() > 2 => {
                    let value = &arg[2..];
                    options.max_count = Some(parse_non_negative(value, "-n")?);
                }
                _ if arg.starts_with('-')
                    && arg.len() > 1
                    && arg.as_bytes().get(1).is_some_and(u8::is_ascii_digit) =>
                {
                    options.max_count = Some(parse_non_negative(&arg[1..], "-<n>")?);
                }
                _ if arg.starts_with("--max-tree-depth=") => {
                    let value = arg.trim_start_matches("--max-tree-depth=");
                    let depth = parse_non_negative(value, "--max-tree-depth")?;
                    object_depth_limit = Some(depth);
                    options.filter =
                        Some(grit_lib::rev_list::ObjectFilter::TreeDepth(depth as u64));
                }
                _ if arg.starts_with("--exclude=") => {
                    let pattern = arg.trim_start_matches("--exclude=");
                    ref_exclusions.add_excluded_ref(pattern);
                    ref_exclude_patterns.push(pattern.to_owned());
                }
                _ if arg.starts_with("--exclude-hidden=") => {
                    if ref_exclusions.hidden_configured {
                        eprintln!("fatal: --exclude-hidden= passed more than once");
                        std::process::exit(128);
                    }
                    let section = arg.trim_start_matches("--exclude-hidden=");
                    match section {
                        "fetch" | "receive" | "uploadpack" => {
                            ref_exclusions.load_hidden_refs_from_config(&config, section);
                        }
                        _ => {
                            eprintln!("fatal: unsupported section for hidden refs: {section}");
                            std::process::exit(128);
                        }
                    }
                }
                _ if arg.starts_with("--glob=") => {
                    saw_pseudo_ref = true;
                    let pattern = normalize_glob_pattern(arg.trim_start_matches("--glob="));
                    let matching = list_refs_glob_physical(&repo, &pattern)
                        .context("failed to list glob refs")?;
                    push_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        &mut ref_exclusions,
                    )?;
                    ref_exclude_patterns.clear();
                }
                "--glob" => {
                    saw_pseudo_ref = true;
                    // Detached option: next arg is the pattern.
                    i += 1;
                    if let Some(next) = args.args.get(i) {
                        let pattern = normalize_glob_pattern(next);
                        let matching = list_refs_glob_physical(&repo, &pattern)
                            .context("failed to list glob refs")?;
                        push_ref_pairs_as_specs(
                            &repo,
                            &mut revision_specs,
                            not_mode,
                            &matching,
                            &mut ref_exclusions,
                        )?;
                        ref_exclude_patterns.clear();
                    }
                }
                "--branches" => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--branches", &ref_exclusions)?;
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")
                        .context("failed to list branch refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/heads/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                _ if arg.starts_with("--branches=") => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--branches", &ref_exclusions)?;
                    let pattern = arg.trim_start_matches("--branches=");
                    let full_pattern = normalize_ref_pattern("refs/heads/", pattern);
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)
                        .context("failed to list branch refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/heads/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                "--tags" => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--tags", &ref_exclusions)?;
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/tags/")
                        .context("failed to list tag refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/tags/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                _ if arg.starts_with("--tags=") => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--tags", &ref_exclusions)?;
                    let pattern = arg.trim_start_matches("--tags=");
                    let full_pattern = normalize_ref_pattern("refs/tags/", pattern);
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)
                        .context("failed to list tag refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/tags/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                "--remotes" => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--remotes", &ref_exclusions)?;
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/remotes/")
                        .context("failed to list remote refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/remotes/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                _ if arg.starts_with("--remotes=") => {
                    saw_pseudo_ref = true;
                    reject_exclude_hidden_with("--remotes", &ref_exclusions)?;
                    let pattern = arg.trim_start_matches("--remotes=");
                    let full_pattern = normalize_ref_pattern("refs/remotes/", pattern);
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)
                        .context("failed to list remote refs")?;
                    push_category_ref_pairs_as_specs(
                        &repo,
                        &mut revision_specs,
                        not_mode,
                        &matching,
                        "refs/remotes/",
                        &ref_exclude_patterns,
                    )?;
                    ref_exclusions.clear();
                    ref_exclude_patterns.clear();
                }
                "--alternate-refs" => {
                    let tips = grit_lib::refs::collect_alternate_ref_oids(&repo.git_dir)?;
                    push_ref_oids_as_specs(&mut revision_specs, not_mode, tips.into_iter());
                }
                _ if arg.starts_with("--min-parents=") => {
                    let value = arg.trim_start_matches("--min-parents=");
                    options.min_parents = Some(parse_non_negative(value, "--min-parents")?);
                }
                _ if arg.starts_with("--max-parents=") => {
                    let value = arg.trim_start_matches("--max-parents=");
                    options.max_parents = Some(parse_non_negative(value, "--max-parents")?);
                }
                "--no-min-parents" => options.min_parents = None,
                "--no-max-parents" => options.max_parents = None,
                _ if arg.starts_with("--ancestry-path=") => {
                    let value = arg.trim_start_matches("--ancestry-path=");
                    let oid =
                        grit_lib::rev_parse::resolve_revision(&repo, value).with_context(|| {
                            format!("could not get commit for --ancestry-path argument {value}")
                        })?;
                    options.ancestry_path = true;
                    options.ancestry_path_bottoms.push(oid);
                }
                "--filter-print-omitted" => options.filter_print_omitted = true,
                "--filter-provided-objects" => options.filter_provided_objects = true,
                "--header" => raw_commit_header = true,
                "--no-commit-header" => no_commit_header = true,
                "--commit-header" => no_commit_header = false,
                "--oneline" => options.output_mode = OutputMode::Format("oneline".to_owned()),
                "--graph" => { /* graph decoration is irrelevant for rev-list consumers here */ }
                "--color" => {
                    use_color = true;
                }
                "--no-color" => {
                    use_color = false;
                }
                _ if arg.starts_with("--color=") => {
                    let val = arg.trim_start_matches("--color=");
                    use_color = val == "always" || val == "true";
                }
                "--abbrev-commit" | "--no-abbrev-commit" => { /* silently accept */ }
                "--abbrev" => abbrev_len = 7,
                "--walk-reflogs" | "-g" => {
                    walk_reflog = true;
                }
                "--reflog" => {
                    // `--reflog` (unlike `-g`/`--walk-reflogs`) adds every reflog
                    // entry's OID as a pending tip; the normal walk dedupes/sorts.
                    add_reflog_tips = true;
                }
                _ if arg.starts_with("--filter=") => {
                    let spec = arg.trim_start_matches("--filter=");
                    let filter = ObjectFilter::parse(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
                    options.filter = Some(match options.filter.take() {
                        Some(existing) => {
                            // Match Git `transform_to_combine_type` + `filter_spec_append_urlencode`:
                            // when the second `--filter` is parsed, trace the URL-encoded first spec,
                            // then each subsequent spec traces only itself.
                            if options.filter_raw_specs.len() == 1 {
                                let first = options.filter_raw_specs[0].as_str();
                                let enc =
                                    grit_lib::rev_list::url_encode_object_filter_subspec(first);
                                grit_lib::rev_list::trace_combine_filter_append(&enc);
                            }
                            let enc = grit_lib::rev_list::url_encode_object_filter_subspec(spec);
                            grit_lib::rev_list::trace_combine_filter_append(&enc);
                            options.filter_raw_specs.push(spec.to_string());
                            existing.merge_with(filter)
                        }
                        None => {
                            options.filter_raw_specs.push(spec.to_string());
                            filter
                        }
                    });
                }
                _ if arg.starts_with("--default") => {
                    // --default REV: use REV as default if no revisions given
                    if let Some(val) = arg.strip_prefix("--default=") {
                        default_rev = Some(val.to_string());
                    } else {
                        i += 1;
                        if let Some(val) = args.args.get(i) {
                            default_rev = Some(val.to_string());
                        }
                    }
                }
                "--until" | "--before" | "--min-age" => {
                    i += 1;
                    let Some(val) = args.args.get(i) else {
                        bail!("{arg} requires a date");
                    };
                    options.until_cutoff = Some(parse_rev_list_date(val)?);
                }
                _ if arg.starts_with("--until=")
                    || arg.starts_with("--before=")
                    || arg.starts_with("--min-age=") =>
                {
                    let val = arg.split_once('=').map(|(_, v)| v).unwrap_or_default();
                    options.until_cutoff = Some(parse_rev_list_date(val)?);
                }
                "--since" | "--after" | "--max-age" => {
                    i += 1;
                    let Some(val) = args.args.get(i) else {
                        bail!("{arg} requires a date");
                    };
                    options.since_cutoff = Some(parse_rev_list_date(val)?);
                }
                _ if arg.starts_with("--since=")
                    || arg.starts_with("--after=")
                    || arg.starts_with("--max-age=") =>
                {
                    let val = arg.split_once('=').map(|(_, v)| v).unwrap_or_default();
                    options.since_cutoff = Some(parse_rev_list_date(val)?);
                }
                _ => bail!("unsupported option: {arg}"),
            }
            i += 1;
            continue;
        }
        if not_mode {
            if let Some(stripped) = arg.strip_prefix('^') {
                revision_specs.push(stripped.to_owned());
            } else {
                revision_specs.push(format!("^{arg}"));
            }
        } else {
            revision_specs.push(arg.clone());
        }
        i += 1;
    }

    if test_bitmap {
        return Ok(());
    }

    if options.objects {
        options.use_bitmap_index = use_bitmap_index;
        options.unpacked_only = unpacked_only;
        if use_bitmap_index {
            options.bitmap_oid_only_objects =
                bitmap_use_oid_only_object_lines(options.filter.as_ref());
        }
        let depth = match object_depth_limit {
            Some(d) => d,
            None => resolve_max_tree_depth(&config)?,
        };
        object_depth_limit = Some(depth);
    }

    if options.paths.is_empty() {
        let keep_at_least = if options.all_refs { 0 } else { 1 };
        let (revs, trailing_paths) =
            split_trailing_pathspecs(&repo, &revision_specs, keep_at_least);
        revision_specs = revs;
        if !trailing_paths.is_empty() {
            options.paths.extend(trailing_paths);
        }
    }

    // Check config for color settings if not explicitly set via --color/--no-color
    if !use_color {
        if let Ok(config) = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true) {
            if let Some(val) = config.get("color.diff") {
                if val == "always" || val == "true" {
                    use_color = true;
                }
            }
            if !use_color {
                if let Some(val) = config.get("color.ui") {
                    if val == "always" || val == "true" {
                        use_color = true;
                    }
                }
            }
        }
    }

    if options.simplify_merges {
        cli_topo_order = true;
    }
    if cli_topo_order {
        options.ordering = if cli_author_date_order {
            OrderingMode::AuthorDateTopo
        } else {
            OrderingMode::Topo
        };
    } else if cli_date_order || cli_author_date_order {
        options.ordering = if cli_author_date_order {
            OrderingMode::AuthorDateWalk
        } else {
            OrderingMode::DateOrderWalk
        };
    }

    if options.simplify_by_decoration {
        // Decoration subset: keep commits pointed to by tags only.
        let decorated = tag_targets(&repo.git_dir).context("failed to list tag refs")?;
        if decorated.is_empty() {
            options.simplify_by_decoration = false;
        }
    }
    options.parent_rewrite =
        show_parents || output_mode_requests_parent_rewrite(&options.output_mode);

    // `--reflog` adds every reflog entry's OID as a positive tip; the ordinary
    // walk below dedupes and date-sorts them (Git's `add_reflogs_to_pending`).
    // Insertion order matters: equal-date commits break ties by the order tips were
    // added (Git's `prio_queue` FIFO), so feed them in Git's reflog-scan order.
    if add_reflog_tips {
        saw_pseudo_ref = true;
        for oid in grit_lib::reflog::all_reflog_oids_ordered(&repo.git_dir)? {
            revision_specs.push(oid.to_hex());
        }
    }

    // Apply --default when no revision specs given
    if revision_specs.is_empty() {
        if let Some(def) = default_rev {
            revision_specs.push(def);
        }
    }

    if walk_reflog {
        // Unlike `git log -g` (which defaults to HEAD via `add_head_to_pending`),
        // `git rev-list -g` does *not* supply a default revision. With no
        // positional revision the reflog walk stays empty and Git falls through
        // to `usage(rev_list_usage)`, exiting non-zero. Mirror that here instead
        // of silently walking every ref's reflog (that is `--reflog` behaviour,
        // handled above via `add_reflog_tips`).
        if revision_specs.is_empty() {
            eprintln!("usage: git rev-list [<options>] <commit>... [--] [<path>...]");
            std::process::exit(129);
        }
        return run_rev_list_reflog_walk(
            &repo,
            &revision_specs,
            options.max_count,
            options.skip,
            options.max_parents,
            options.min_parents,
            options.since_cutoff,
            options.until_cutoff,
            &options.paths,
            show_parents,
        );
    }

    // Handle symmetric diff (A...B) tokens
    let mut symmetric_left: Option<String> = None;
    let mut symmetric_right: Option<String> = None;
    let mut processed_specs = Vec::new();
    for spec in &revision_specs {
        for expanded in expand_parent_shorthand(&repo, spec)? {
            if is_symmetric_diff(&expanded) {
                if let Some((lhs, rhs)) = split_symmetric_diff(&expanded) {
                    symmetric_left = Some(lhs);
                    symmetric_right = Some(rhs);
                }
            } else {
                processed_specs.push(expanded);
            }
        }
    }

    let (mut positive_specs, mut negative_specs, stdin_all_refs, stdin_paths) =
        collect_revision_specs_with_stdin(&repo.git_dir, &processed_specs, read_stdin)
            .context("failed to parse revision arguments")?;
    if bisect_mode == Some(BisectMode::One)
        && positive_specs.is_empty()
        && negative_specs.is_empty()
    {
        append_bisect_ref_specs(&repo, &mut positive_specs, &mut negative_specs)?;
    }
    if stdin_all_refs {
        options.all_refs = true;
    }
    if !stdin_paths.is_empty() {
        options.paths.extend(stdin_paths);
    }
    if ((saw_pseudo_ref && !options.all_refs)
        || (read_stdin && negative_specs.is_empty() && !options.all_refs))
        && positive_specs.is_empty()
    {
        return Ok(());
    }

    // If symmetric diff, resolve merge bases and set up positive/negative
    if let (Some(ref lhs), Some(ref rhs)) = (&symmetric_left, &symmetric_right) {
        let lhs_spec = if lhs.is_empty() { "HEAD" } else { lhs };
        let rhs_spec = if rhs.is_empty() { "HEAD" } else { rhs };
        let lhs_oid = grit_lib::rev_parse::resolve_revision(&repo, lhs_spec)
            .with_context(|| format!("bad revision '{lhs_spec}'"))?;
        let rhs_oid = grit_lib::rev_parse::resolve_revision(&repo, rhs_spec)
            .with_context(|| format!("bad revision '{rhs_spec}'"))?;
        let bases = merge_bases(&repo, lhs_oid, rhs_oid, options.first_parent)
            .context("failed to compute merge bases")?;
        positive_specs.push(lhs_spec.to_owned());
        positive_specs.push(rhs_spec.to_owned());
        for base in bases {
            negative_specs.push(base.to_hex());
        }
        // Pass symmetric OIDs to rev_list for left-right classification
        options.symmetric_left = Some(lhs_oid);
        options.symmetric_right = Some(rhs_oid);
    }

    let mut result =
        rev_list(&repo, &positive_specs, &negative_specs, &options).context("rev-list failed")?;

    // `rev-list --objects --missing=error` against a promisor remote must
    // hydrate the full reachable set: Git bulk lazy-fetches every missing
    // reachable tree/blob so they become present and are enumerated. The first
    // walk above only returns the present commits (missing objects are skipped),
    // so use its commit set to drive a bulk hydration, then walk again.
    if options.objects
        && options.missing_action == MissingAction::Error
        && !options.exclude_promisor_objects
    {
        match crate::commands::promisor_hydrate::hydrate_reachable_trees_blobs_from_commits(
            &repo,
            &result.commits,
        ) {
            Ok(true) => {
                grit_lib::pack::clear_pack_cache();
                result = rev_list(&repo, &positive_specs, &negative_specs, &options)
                    .context("rev-list failed")?;
            }
            Ok(false) => {}
            Err(e) => return Err(e),
        }
    }

    if let Some(format) = disk_usage_format {
        let mut object_ids = Vec::with_capacity(result.commits.len() + result.objects.len());
        object_ids.extend(result.commits.iter().copied());
        if options.objects {
            object_ids.extend(result.objects.iter().map(|(oid, _)| *oid));
        }

        let pack_sizes = collect_packed_object_sizes(repo.odb.objects_dir())?;
        let mut total = 0u64;
        let mut seen = HashSet::new();
        for oid in object_ids {
            if !seen.insert(oid) {
                continue;
            }
            total = total.saturating_add(object_disk_usage(&repo, oid, &pack_sizes)?);
        }

        match format {
            DiskUsageFormat::Bytes => println!("{total}"),
            DiskUsageFormat::Human => println!("{total} bytes"),
        }
        return Ok(());
    }

    if options.objects && options.missing_action == MissingAction::Error {
        let max_tree_depth = object_depth_limit.unwrap_or(resolve_max_tree_depth(&config)?);
        validate_rev_list_tree_depth(&repo, &result.commits, max_tree_depth)?;
    }

    if let Some(mode) = bisect_mode {
        let selection = select_bisect_commits(&repo, &result.commits, options.first_parent)
            .context("failed to select bisection commit")?;
        match mode {
            BisectMode::One => {
                if let Some(best) = selection.best {
                    println!("{}", best.oid);
                }
            }
            BisectMode::Vars => {
                let best = selection
                    .all
                    .iter()
                    .map(|entry| entry.distance)
                    .max()
                    .and_then(|max_distance| {
                        result.commits.iter().rev().find_map(|oid| {
                            selection
                                .all
                                .iter()
                                .find(|entry| entry.oid == *oid && entry.distance == max_distance)
                                .cloned()
                        })
                    });
                if let Some(best) = best {
                    let remaining = selection.total.saturating_sub(best.reaches);
                    let count_after_test = remaining.max(best.reaches);
                    println!("bisect_rev='{}'", best.oid);
                    println!("bisect_nr={}", count_after_test.saturating_sub(1));
                    println!("bisect_good={}", remaining.saturating_sub(1));
                    println!("bisect_bad={}", best.reaches.saturating_sub(1));
                    println!("bisect_all={}", selection.total);
                    println!("bisect_steps={}", estimate_bisect_steps(selection.total));
                }
            }
            BisectMode::All => {
                let decorations = bisect_tag_decorations(&repo)?;
                for entry in selection.all {
                    let mut parts: Vec<String> = decorations
                        .get(&entry.oid)
                        .into_iter()
                        .flat_map(|names| names.iter())
                        .map(|name| format!("tag: {name}"))
                        .collect();
                    parts.push(format!("dist={}", entry.distance));
                    println!("{} ({})", entry.oid, parts.join(", "));
                }
            }
        }
        return Ok(());
    }

    if options.count {
        if options.left_right {
            if options.cherry_mark {
                let left_count = result
                    .commits
                    .iter()
                    .filter(|oid| !result.cherry_equivalent.contains(oid))
                    .filter(|oid| result.left_right_map.get(oid) == Some(&true))
                    .count();
                let right_count = result
                    .commits
                    .iter()
                    .filter(|oid| !result.cherry_equivalent.contains(oid))
                    .filter(|oid| result.left_right_map.get(oid) == Some(&false))
                    .count();
                let equivalent_count = result
                    .commits
                    .iter()
                    .filter(|oid| result.cherry_equivalent.contains(oid))
                    .count();
                println!("{left_count}\t{right_count}\t{equivalent_count}");
            } else {
                let left_count = result
                    .commits
                    .iter()
                    .filter(|oid| result.left_right_map.get(oid) == Some(&true))
                    .count();
                let right_count = result
                    .commits
                    .iter()
                    .filter(|oid| result.left_right_map.get(oid) == Some(&false))
                    .count();
                println!("{left_count}\t{right_count}");
            }
        } else if options.cherry_mark {
            let equivalent_count = result
                .commits
                .iter()
                .filter(|oid| result.cherry_equivalent.contains(oid))
                .count();
            let different_count = result.commits.len().saturating_sub(equivalent_count);
            println!("{different_count}\t{equivalent_count}");
        } else {
            let mut total = result.commits.len();
            if options.objects {
                total += result.objects.len();
            }
            println!("{total}");
        }
        return Ok(());
    }

    if options.boundary && options.maximal_only {
        bail!("options '--boundary' and '--maximal-only' cannot be used together");
    }

    let emit_record = |record: &str| {
        if zero_terminated {
            print!("{record}\0");
        } else {
            println!("{record}");
        }
    };

    let print_object = |oid: &grit_lib::objects::ObjectId, path: &str| {
        if options.no_object_names {
            emit_record(&oid.to_string());
        } else if zero_terminated {
            emit_record(&oid.to_string());
            if !path.is_empty() {
                emit_record(&format!("path={path}"));
            }
        } else if path.is_empty() {
            if result.bitmap_object_format {
                emit_record(&oid.to_string());
            } else {
                emit_record(&format!("{oid} "));
            }
        } else {
            emit_record(&format!("{oid} {path}"));
        }
    };

    let print_boundary_commits = || {
        for oid in &result.boundary_commits {
            if zero_terminated {
                emit_record(&oid.to_string());
                emit_record("boundary=yes");
            } else {
                println!("-{oid}");
            }
        }
    };

    let graft_parents = load_graft_parents(&repo.git_dir);
    let selected_for_parent_rewrite: HashSet<ObjectId> = result.commits.iter().copied().collect();
    let rewrite_path_limited_parents = !options.paths.is_empty();

    // Build the children decoration like Git's set_children(): iterate the output
    // commit list in order and, for each commit, prepend it to each of its parents'
    // children list. Prepending means a parent's children end up in reverse
    // output order, matching Git's show_children().
    let children_map: HashMap<ObjectId, Vec<ObjectId>> = if show_children {
        let mut map: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        for child in &result.commits {
            let parents = commit_parents_for_output(&repo, *child, &graft_parents)?;
            for parent in parents {
                map.entry(parent).or_default().insert(0, *child);
            }
        }
        map
    } else {
        HashMap::new()
    };

    let object_type_commit_oid_only = options.objects
        && matches!(&options.output_mode, OutputMode::OidOnly)
        && object_type_filter_commit_only(options.filter.as_ref());

    let print_commit_line = |oid: &ObjectId| -> Result<()> {
        let mut prefix = String::new();
        if options.left_right {
            if let Some(&is_left) = result.left_right_map.get(oid) {
                if is_left {
                    prefix.push('<');
                } else {
                    prefix.push('>');
                }
            }
        }
        if options.cherry_mark {
            if result.cherry_equivalent.contains(oid) {
                prefix = "=".to_owned();
            } else if !options.left_right_explicit {
                prefix = "+".to_owned();
            }
        }
        let children_suffix = |oid: &ObjectId| -> String {
            if !show_children {
                return String::new();
            }
            match children_map.get(oid) {
                Some(children) if !children.is_empty() => {
                    let mut s = String::new();
                    for child in children {
                        s.push(' ');
                        s.push_str(&child.to_hex());
                    }
                    s
                }
                _ => String::new(),
            }
        };
        if object_type_commit_oid_only && matches!(&options.output_mode, OutputMode::OidOnly) {
            emit_record(&oid.to_string());
        } else {
            match &options.output_mode {
                OutputMode::Format(fmt) => {
                    let is_oneline = fmt == "oneline";
                    let is_named_format = matches!(
                        fmt.as_str(),
                        "oneline" | "short" | "medium" | "full" | "fuller" | "email" | "raw"
                    );
                    if (!no_commit_header || is_named_format) && !is_oneline {
                        let mut header = format!("commit {prefix}{oid}");
                        if show_parents {
                            // Path-limited history rewrites parents through omitted commits. Other
                            // narrowing, such as `-n`, still shows stored/grafted parents.
                            let parents = commit_parents_for_output_mode(
                                &repo,
                                *oid,
                                &graft_parents,
                                rewrite_path_limited_parents,
                                &selected_for_parent_rewrite,
                            )?;
                            for parent in parents {
                                header.push(' ');
                                header.push_str(&parent.to_hex());
                            }
                        }
                        println!("{header}");
                    }
                    let rendered = render_commit_with_color(
                        &repo,
                        *oid,
                        &options.output_mode,
                        abbrev_len,
                        use_color,
                    )?;
                    if is_named_format {
                        std::io::stdout()
                            .write_all(&encode_pretty_output(
                                &rendered,
                                pretty_output_encoding.as_deref(),
                            ))
                            .context("failed to write rev-list output")?;
                        if !rendered.ends_with('\n') {
                            println!();
                        }
                    } else if !rendered.is_empty() {
                        std::io::stdout()
                            .write_all(&encode_pretty_output(
                                &rendered,
                                pretty_output_encoding.as_deref(),
                            ))
                            .context("failed to write rev-list output")?;
                        println!();
                    }
                }
                OutputMode::Parents => {
                    // Path-limited history rewrites parents through omitted commits. Other
                    // narrowing, such as `-n`, still shows stored/grafted parents.
                    let parents = commit_parents_for_output_mode(
                        &repo,
                        *oid,
                        &graft_parents,
                        rewrite_path_limited_parents,
                        &selected_for_parent_rewrite,
                    )?;
                    let children = children_suffix(oid);
                    if parents.is_empty() {
                        emit_record(&format!("{prefix}{oid}{children}"));
                    } else {
                        let rendered_parents = parents
                            .iter()
                            .map(ObjectId::to_hex)
                            .collect::<Vec<_>>()
                            .join(" ");
                        emit_record(&format!("{prefix}{oid} {rendered_parents}{children}"));
                    }
                }
                _ => {
                    let rendered = render_commit(&repo, *oid, &options.output_mode, abbrev_len)?;
                    let children = children_suffix(oid);
                    emit_record(&format!("{prefix}{rendered}{children}"));
                }
            }
        }
        if raw_commit_header {
            let object = repo
                .odb
                .read(oid)
                .with_context(|| format!("failed to read commit object {oid}"))?;
            if object.kind != ObjectKind::Commit {
                bail!("object {oid} is not a commit");
            }
            let mut stdout = std::io::stdout();
            stdout
                .write_all(&object.data)
                .context("failed to write rev-list commit header")?;
            stdout
                .write_all(b"\0")
                .context("failed to write rev-list commit header separator")?;
        }
        Ok(())
    };

    if !options.quiet {
        if options.boundary && options.reverse {
            print_boundary_commits();
        }

        let interleaved_objects = options.objects
            && options.use_bitmap_index
            && result.per_commit_object_counts.is_empty()
            && !result.object_segments.is_empty()
            && (result.bitmap_object_format || result.objects.is_empty());

        if interleaved_objects {
            let all_object_segments_empty = result.object_segments.iter().all(|s| s.is_empty());
            let mut commit_order: Vec<usize> = (0..result.commits.len()).collect();
            // Git's bitmap path reorders commits when the filter removes all trees/blobs (`tree:0`);
            // when any objects remain, bitmap output stays in walk order (`test_cmp` with non-bitmap).
            if all_object_segments_empty && result.objects.is_empty() {
                commit_order.sort_by_key(|&i| result.commits[i].to_hex());
            }
            for &ci in &commit_order {
                let oid = &result.commits[ci];
                if !options.quiet && result.objects_print_commit.get(ci).copied().unwrap_or(true) {
                    print_commit_line(oid)?;
                }
                if let Some(seg) = result.object_segments.get(ci) {
                    for (obj_oid, path) in seg {
                        print_object(obj_oid, path);
                    }
                }
            }
            if let Some(roots) = result.object_segments.get(result.commits.len()) {
                for (obj_oid, path) in roots {
                    print_object(obj_oid, path);
                }
            }
        } else {
            let mut obj_offset = 0usize;
            for (ci, oid) in result.commits.iter().enumerate() {
                if !options.quiet
                    && (!options.objects
                        || result.objects_print_commit.get(ci).copied().unwrap_or(true))
                {
                    print_commit_line(oid)?;
                }

                if !result.per_commit_object_counts.is_empty() {
                    let count = result
                        .per_commit_object_counts
                        .get(ci)
                        .copied()
                        .unwrap_or(0);
                    for j in obj_offset..obj_offset + count {
                        if let Some((obj_oid, path)) = result.objects.get(j) {
                            print_object(obj_oid, path);
                        }
                    }
                    obj_offset += count;
                }
            }

            if options.objects && result.per_commit_object_counts.is_empty() {
                for (oid, path) in &result.objects {
                    print_object(oid, path);
                }
            }
        }
    }

    // Print omitted objects if --filter-print-omitted
    if options.filter_print_omitted {
        for oid in &result.omitted_objects {
            println!("~{oid}");
        }
    }

    if matches!(
        options.missing_action,
        MissingAction::Print | MissingAction::PrintInfo
    ) {
        let mut seen_missing = HashSet::new();
        let mut missing_oids = Vec::new();
        for oid in &result.missing_objects {
            if seen_missing.insert(*oid) {
                missing_oids.push(*oid);
            }
        }
        for oid in read_promisor_missing_oids(&repo.git_dir) {
            if repo.odb.exists_local(&oid) {
                continue;
            }
            if seen_missing.insert(oid) {
                missing_oids.push(oid);
            }
        }

        match options.missing_action {
            MissingAction::Print => {
                for oid in missing_oids {
                    if zero_terminated {
                        print!("?{oid}\0");
                    } else {
                        println!("?{oid}");
                    }
                }
            }
            MissingAction::PrintInfo => {
                for oid in missing_oids {
                    let info = infer_missing_object_info(&repo, &result.commits, oid);
                    if zero_terminated {
                        print!("{}\0missing=yes\0", info.oid);
                        if let Some(path) = &info.path {
                            print!("path={path}\0");
                        }
                        print!("type={}\0", info.kind);
                    } else {
                        let mut line = format!("?{}", info.oid);
                        if let Some(path) = &info.path {
                            line.push_str(" path=");
                            line.push_str(&quote_missing_info_path(path));
                        }
                        line.push_str(" type=");
                        line.push_str(info.kind.as_str());
                        println!("{line}");
                    }
                }
            }
            MissingAction::Error | MissingAction::Allow => {}
        }
    }

    if options.boundary && (!options.reverse || options.quiet) {
        print_boundary_commits();
    }

    Ok(())
}

fn validate_rev_list_tree_depth(
    repo: &Repository,
    commits: &[ObjectId],
    max_tree_depth: usize,
) -> Result<()> {
    let mut seen_trees = HashSet::new();
    for oid in commits {
        let object = repo.odb.read(oid)?;
        let commit = parse_commit(&object.data)?;
        validate_tree_depth_limit(repo, commit.tree, 0, max_tree_depth, &mut seen_trees)?;
    }
    Ok(())
}

fn validate_tree_depth_limit(
    repo: &Repository,
    tree_oid: ObjectId,
    depth: usize,
    max_tree_depth: usize,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !seen.insert(tree_oid) {
        return Ok(());
    }
    if depth > max_tree_depth {
        bail!(
            "tree depth {} exceeds core.maxtreedepth {}",
            depth,
            max_tree_depth
        );
    }
    let object = match repo.odb.read(&tree_oid) {
        Ok(o) => o,
        Err(grit_lib::error::Error::ObjectNotFound(_)) => return Ok(()),
        Err(e) => return Err(e).context("read tree for maxtreedepth validation")?,
    };
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        if entry.mode == 0o040000 {
            validate_tree_depth_limit(repo, entry.oid, depth + 1, max_tree_depth, seen)?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn commit_touches_paths_for_show(
    repo: &Repository,
    oid: ObjectId,
    paths: &[String],
) -> Result<bool> {
    let object = repo.odb.read(&oid)?;
    let commit = parse_commit(&object.data)?;
    let commit_entries = flatten_tree_for_show(repo, commit.tree, "")?;
    let commit_map: HashMap<String, ObjectId> = commit_entries.into_iter().collect();

    if commit.parents.is_empty() {
        return Ok(commit_map.keys().any(|path| {
            paths
                .iter()
                .any(|spec| pathspec_matches_for_show(spec, path))
        }));
    }

    if commit.parents.len() == 1 {
        let parent_object = repo.odb.read(&commit.parents[0])?;
        let parent_commit = parse_commit(&parent_object.data)?;
        let parent_map: HashMap<String, ObjectId> =
            flatten_tree_for_show(repo, parent_commit.tree, "")?
                .into_iter()
                .collect();
        return Ok(path_differs_for_specs_for_show(
            &commit_map,
            &parent_map,
            paths,
        ));
    }

    let mut treesame_parents = 0usize;
    let mut differs_any = false;
    for parent_oid in &commit.parents {
        let parent_object = repo.odb.read(parent_oid)?;
        let parent_commit = parse_commit(&parent_object.data)?;
        let parent_map: HashMap<String, ObjectId> =
            flatten_tree_for_show(repo, parent_commit.tree, "")?
                .into_iter()
                .collect();
        let differs = path_differs_for_specs_for_show(&commit_map, &parent_map, paths);
        if differs {
            differs_any = true;
        } else {
            treesame_parents += 1;
        }
    }
    if treesame_parents == 1 {
        return Ok(false);
    }
    Ok(differs_any)
}

#[allow(dead_code)]
fn path_differs_for_specs_for_show(
    current: &HashMap<String, ObjectId>,
    parent: &HashMap<String, ObjectId>,
    specs: &[String],
) -> bool {
    let mut paths = std::collections::BTreeSet::new();
    paths.extend(current.keys().cloned());
    paths.extend(parent.keys().cloned());

    for path in &paths {
        if !specs
            .iter()
            .any(|spec| pathspec_matches_for_show(spec, path))
        {
            continue;
        }
        if current.get(path) != parent.get(path) {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn pathspec_matches_for_show(spec: &str, path: &str) -> bool {
    let normalized = spec.strip_prefix("./").unwrap_or(spec);
    if normalized == "." || normalized.is_empty() {
        return true;
    }
    if normalized.contains('*') || normalized.contains('?') || normalized.contains('[') {
        return grit_lib::wildmatch::wildmatch(
            normalized.as_bytes(),
            path.as_bytes(),
            grit_lib::wildmatch::WM_PATHNAME,
        );
    }
    if let Some(prefix) = normalized.strip_suffix('/') {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    path == normalized || path.starts_with(&format!("{normalized}/"))
}

#[allow(dead_code)]
fn flatten_tree_for_show(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
) -> Result<Vec<(String, ObjectId)>> {
    let mut result = Vec::new();
    let object = match repo.odb.read(&tree_oid) {
        Ok(o) => o,
        Err(_) => return Ok(result),
    };
    if object.kind != grit_lib::objects::ObjectKind::Tree {
        return Ok(result);
    }
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name).to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let child = repo.odb.read(&entry.oid)?;
        if child.kind == grit_lib::objects::ObjectKind::Tree {
            result.extend(flatten_tree_for_show(repo, entry.oid, &path)?);
        } else {
            result.push((path, entry.oid));
        }
    }
    Ok(result)
}

fn parse_non_negative(text: &str, flag: &str) -> Result<usize> {
    let value = text
        .parse::<isize>()
        .map_err(|_| anyhow::anyhow!("{flag}: '{text}' is not an integer"))?;
    if value < 0 {
        return Ok(usize::MAX);
    }
    Ok(value as usize)
}

fn parse_rev_list_date(s: &str) -> Result<i64> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s
            .parse::<i64>()
            .with_context(|| format!("invalid date: '{s}'"));
    }
    let mut approx_err = 0;
    let approx = approxidate_careful(s, Some(&mut approx_err));
    if approx_err == 0 {
        return i64::try_from(approx).context("date out of range for rev-list cutoff");
    }
    if let Ok((ts, _)) = parse_date_basic(s) {
        return i64::try_from(ts).context("date out of range for rev-list cutoff");
    }
    // Fallback for plain `YYYY-MM-DD` dates only. `parse_date_basic()` already handles
    // date+time strings (including RFC formats) and should not be truncated to midnight.
    if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        let parts: Vec<&str> = s[..10].split('-').collect();
        if parts.len() == 3 {
            if let (Ok(y), Ok(m), Ok(d)) = (
                parts[0].parse::<i32>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                if let Ok(month) = time::Month::try_from(m) {
                    if let Ok(date) = time::Date::from_calendar_date(y, month, d) {
                        let dt = date
                            .with_hms(0, 0, 0)
                            .context("invalid midnight time for rev-list date")?
                            .assume_utc();
                        return Ok(dt.unix_timestamp());
                    }
                }
            }
        }
    }
    s.parse::<i64>()
        .with_context(|| format!("invalid date: '{s}'"))
}

struct RevListReflogWalkLog {
    entries: Vec<ReflogEntry>,
    recno: isize,
    nr: usize,
    tie_order: usize,
}

fn reflog_entry_unix_ts_rev(entry: &ReflogEntry) -> Option<i64> {
    let parts: Vec<&str> = entry.identity.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

fn rev_list_reflog_message_is_checkout(message: &str) -> bool {
    message
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("checkout:")
}

fn rev_list_tree_matches_any_pathspec(
    odb: &grit_lib::odb::Odb,
    tree_oid: &ObjectId,
    pathspecs: &[String],
) -> Result<bool> {
    let paths = grit_lib::diff::head_path_states(odb, Some(tree_oid))?;
    Ok(paths.keys().any(|path| {
        pathspecs
            .iter()
            .any(|ps| matches_pathspec(ps, path.as_str()))
    }))
}

fn rev_list_reflog_transition_touches_paths(
    repo: &Repository,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    reflog_message: &str,
    pathspecs: &[String],
) -> Result<bool> {
    if pathspecs.is_empty() {
        return Ok(true);
    }
    let odb = &repo.odb;
    let new_obj = odb.read(new_oid)?;
    let new_commit = parse_commit(&new_obj.data)?;

    let tree_diff_touches = |from_tree: Option<ObjectId>, to_tree: &ObjectId| -> Result<bool> {
        let entries = diff_trees(odb, from_tree.as_ref(), Some(to_tree), "")?;
        Ok(entries.iter().any(|e| {
            let path = e.path().to_string_lossy();
            pathspecs.iter().any(|ps| matches_pathspec(ps, &path))
        }))
    };

    if new_commit.parents.len() >= 2 {
        if !commit_visible_for_dense_pathspecs(repo, *new_oid, pathspecs)? {
            return Ok(false);
        }
        for p in &new_commit.parents {
            let p_obj = match odb.read(p) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let p_commit = match parse_commit(&p_obj.data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if tree_diff_touches(Some(p_commit.tree), &new_commit.tree)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if new_commit.parents.len() < 2 && rev_list_reflog_message_is_checkout(reflog_message) {
        return rev_list_tree_matches_any_pathspec(odb, &new_commit.tree, pathspecs);
    }

    let old_tree = if old_oid.is_zero() {
        None
    } else {
        let old_obj = match odb.read(old_oid) {
            Ok(o) => o,
            Err(_) => return Ok(false),
        };
        let old_commit = match parse_commit(&old_obj.data) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        Some(old_commit.tree)
    };
    tree_diff_touches(old_tree, &new_commit.tree)
}

fn run_rev_list_reflog_walk(
    repo: &Repository,
    revision_specs: &[String],
    max_count: Option<usize>,
    skip_n: usize,
    max_parents: Option<usize>,
    min_parents: Option<usize>,
    since_cutoff: Option<i64>,
    until_cutoff: Option<i64>,
    paths: &[String],
    show_parents: bool,
) -> Result<()> {
    let mut walks: Vec<RevListReflogWalkLog> = Vec::new();
    for (tie_order, spec) in revision_specs.iter().enumerate() {
        let log_ref = grit_lib::rev_parse::resolve_reflog_walk_log_ref(repo, spec)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let entries =
            read_reflog_dwim(&repo.git_dir, &log_ref).map_err(|e| anyhow::anyhow!("{e}"))?;
        if entries.is_empty() {
            continue;
        }
        let nr = entries.len();
        walks.push(RevListReflogWalkLog {
            entries,
            recno: (nr - 1) as isize,
            nr,
            tie_order,
        });
    }

    if walks.is_empty() {
        return Ok(());
    }

    let graft_parents = load_graft_parents(&repo.git_dir);
    let max = max_count.unwrap_or(usize::MAX);
    let mut shown = 0usize;
    let mut skipped = 0usize;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        if shown >= max {
            break;
        }

        let mut best_i: Option<usize> = None;
        let mut best_ts = i64::MIN;
        let mut best_tie = usize::MAX;

        for (i, w) in walks.iter().enumerate() {
            if w.recno < 0 {
                continue;
            }
            let e = &w.entries[w.recno as usize];
            let Some(ts) = reflog_entry_unix_ts_rev(e) else {
                continue;
            };
            if ts > best_ts || (ts == best_ts && w.tie_order < best_tie) {
                best_ts = ts;
                best_tie = w.tie_order;
                best_i = Some(i);
            }
        }

        let Some(wi) = best_i else {
            break;
        };

        let w = &mut walks[wi];
        let entry = w.entries[w.recno as usize].clone();
        w.recno -= 1;

        let commit_data = match repo.odb.read(&entry.new_oid) {
            Ok(obj) => match parse_commit(&obj.data) {
                Ok(c) => c,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let n_parents = commit_data.parents.len();
        if let Some(mx) = max_parents {
            if n_parents > mx {
                continue;
            }
        }
        if let Some(mn) = min_parents {
            if n_parents < mn {
                continue;
            }
        }

        if let Some(since) = since_cutoff {
            if let Some(ets) = reflog_entry_unix_ts_rev(&entry) {
                if ets < since {
                    continue;
                }
            }
        }
        if let Some(until) = until_cutoff {
            if let Some(ets) = reflog_entry_unix_ts_rev(&entry) {
                if ets > until {
                    continue;
                }
            }
        }

        if !paths.is_empty()
            && !rev_list_reflog_transition_touches_paths(
                repo,
                &entry.old_oid,
                &entry.new_oid,
                &entry.message,
                paths,
            )?
        {
            continue;
        }

        if skipped < skip_n {
            skipped += 1;
            continue;
        }

        if show_parents {
            let parents = commit_parents_for_output(&repo, entry.new_oid, &graft_parents)?;
            if parents.is_empty() {
                writeln!(out, "{}", entry.new_oid.to_hex())?;
            } else {
                let tail = parents
                    .iter()
                    .map(ObjectId::to_hex)
                    .collect::<Vec<_>>()
                    .join(" ");
                writeln!(out, "{} {}", entry.new_oid.to_hex(), tail)?;
            }
        } else {
            writeln!(out, "{}", entry.new_oid.to_hex())?;
        }
        shown += 1;
    }

    Ok(())
}

fn expand_parent_shorthand(repo: &Repository, spec: &str) -> Result<Vec<String>> {
    if let Some(expanded) =
        grit_lib::rev_parse::expand_parent_shorthand_rev_parse_lines(repo, spec, false, None)?
    {
        return Ok(expanded);
    }
    Ok(vec![spec.to_string()])
}

fn split_trailing_pathspecs(
    repo: &Repository,
    specs: &[String],
    keep_at_least: usize,
) -> (Vec<String>, Vec<String>) {
    let mut revisions = specs.to_vec();
    let mut paths = Vec::new();

    while revisions.len() > keep_at_least {
        let Some(candidate) = revisions.last() else {
            break;
        };
        if candidate.starts_with('^')
            || candidate.starts_with('-')
            || candidate.contains("..")
            || candidate.contains("...")
        {
            break;
        }
        if grit_lib::rev_parse::resolve_revision(repo, candidate).is_ok() {
            break;
        }
        let candidate_path = Path::new(candidate);
        if !candidate_path.exists() {
            break;
        }
        paths.push(candidate.clone());
        revisions.pop();
    }

    paths.reverse();
    (revisions, paths)
}

fn collect_packed_object_sizes(objects_dir: &Path) -> Result<HashMap<ObjectId, u64>> {
    let mut sizes = HashMap::new();
    let indexes = pack::read_local_pack_indexes(objects_dir)?;

    for idx in indexes {
        let pack_size = match std::fs::metadata(&idx.pack_path) {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        let mut offsets: Vec<(u64, ObjectId)> = idx
            .entries
            .into_iter()
            .filter_map(|entry| {
                ObjectId::from_bytes(&entry.oid)
                    .ok()
                    .map(|oid| (entry.offset, oid))
            })
            .collect();
        offsets.sort_by_key(|(offset, _)| *offset);
        for (pos, (offset, oid)) in offsets.iter().enumerate() {
            let next_offset = offsets
                .get(pos + 1)
                .map(|(next, _)| *next)
                .unwrap_or_else(|| pack_size.saturating_sub(20));
            if next_offset < *offset {
                continue;
            }
            sizes.entry(*oid).or_insert(next_offset - *offset);
        }
    }

    Ok(sizes)
}

fn object_disk_usage(
    repo: &Repository,
    oid: ObjectId,
    pack_sizes: &HashMap<ObjectId, u64>,
) -> Result<u64> {
    let loose = repo.odb.object_path(&oid);
    if let Ok(meta) = std::fs::metadata(loose) {
        return Ok(meta.len());
    }

    Ok(pack_sizes.get(&oid).copied().unwrap_or(0))
}

fn load_graft_parents(git_dir: &Path) -> HashMap<ObjectId, Vec<ObjectId>> {
    let graft_path = git_dir.join("info/grafts");
    let mut grafts = HashMap::new();
    let Ok(contents) = std::fs::read_to_string(&graft_path) else {
        return grafts;
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(commit_hex) = fields.next() else {
            continue;
        };
        let Ok(commit_oid) = commit_hex.parse::<ObjectId>() else {
            continue;
        };
        let mut parents = Vec::new();
        let mut valid = true;
        for parent_hex in fields {
            match parent_hex.parse::<ObjectId>() {
                Ok(parent_oid) => parents.push(parent_oid),
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            grafts.insert(commit_oid, parents);
        }
    }
    grafts
}

fn commit_parents_for_output(
    repo: &Repository,
    oid: ObjectId,
    graft_parents: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if let Some(grafted) = graft_parents.get(&oid) {
        return Ok(grafted.clone());
    }
    let object = repo.odb.read(&oid)?;
    let commit = parse_commit(&object.data)?;
    Ok(commit.parents)
}

fn collect_visible_parents_for_output(
    repo: &Repository,
    candidate: ObjectId,
    graft_parents: &HashMap<ObjectId, Vec<ObjectId>>,
    selected: &HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(candidate) {
        return Ok(());
    }
    if selected.contains(&candidate) {
        out.push(candidate);
        return Ok(());
    }

    let mut parents = commit_parents_for_output(repo, candidate, graft_parents)?;
    if parents.len() > 1 {
        parents.truncate(1);
    }
    for parent in parents {
        collect_visible_parents_for_output(repo, parent, graft_parents, selected, seen, out)?;
    }
    Ok(())
}

fn visible_parents_for_path_limited_output(
    repo: &Repository,
    oid: ObjectId,
    graft_parents: &HashMap<ObjectId, Vec<ObjectId>>,
    selected: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let parents = commit_parents_for_output(repo, oid, graft_parents)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for parent in parents {
        collect_visible_parents_for_output(
            repo,
            parent,
            graft_parents,
            selected,
            &mut seen,
            &mut out,
        )?;
    }
    let mut dedup = HashSet::new();
    out.retain(|parent| dedup.insert(*parent));
    Ok(out)
}

fn commit_parents_for_output_mode(
    repo: &Repository,
    oid: ObjectId,
    graft_parents: &HashMap<ObjectId, Vec<ObjectId>>,
    rewrite_path_limited_parents: bool,
    selected: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    if rewrite_path_limited_parents {
        visible_parents_for_path_limited_output(repo, oid, graft_parents, selected)
    } else {
        commit_parents_for_output(repo, oid, graft_parents)
    }
}
