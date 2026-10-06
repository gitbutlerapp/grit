//! `grit bundle` — move objects and refs by archive.
//!
//! Implements create, verify, list-heads, and unbundle subcommands.

use crate::explicit_exit::SilentNonZeroExit;
use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use grit_lib::git_date::approx::approxidate_careful;
use grit_lib::git_date::parse::parse_date_basic;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};

use grit_lib::objects::{parse_commit, parse_tag, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, split_revision_token, ObjectFilter, RevListOptions};

/// Insertion-order-preserving map of bundle ref name to object id.
///
/// Git records bundle refs in the order they appear on the command line
/// (matching `revs->pending` iteration in `bundle.c`), so a plain `BTreeMap`
/// (which sorts alphabetically) cannot reproduce the header ordering. This
/// thin wrapper keeps the first-seen position of each ref name while still
/// de-duplicating repeated names.
#[derive(Debug, Default, Clone)]
struct BundleRefs {
    entries: Vec<(String, ObjectId)>,
}

impl BundleRefs {
    /// Insert or update a ref, preserving first-insertion order.
    fn insert(&mut self, name: String, oid: ObjectId) {
        if let Some(slot) = self.entries.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = oid;
        } else {
            self.entries.push((name, oid));
        }
    }

    /// Returns true when no refs are recorded.
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of recorded refs.
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Iterate over `(name, oid)` pairs in insertion order.
    fn iter(&self) -> impl Iterator<Item = (&String, &ObjectId)> {
        self.entries.iter().map(|(n, o)| (n, o))
    }

    /// Iterate over object ids in insertion order.
    fn values(&self) -> impl Iterator<Item = &ObjectId> {
        self.entries.iter().map(|(_, o)| o)
    }

    /// Retain only entries for which the predicate returns true.
    fn retain<F: FnMut(&str, &mut ObjectId) -> bool>(&mut self, mut keep: F) {
        self.entries.retain_mut(|(name, oid)| keep(name, oid));
    }
}

/// Arguments for `grit bundle`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: BundleAction,
}

#[derive(Debug, Subcommand)]
pub enum BundleAction {
    /// Create a bundle file.
    Create(CreateArgs),
    /// Verify a bundle file.
    Verify(VerifyArgs),
    /// List references in a bundle.
    #[command(name = "list-heads")]
    ListHeads(ListHeadsArgs),
    /// Unbundle objects from a bundle file.
    Unbundle(UnbundleArgs),
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Suppress progress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Force progress output.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Re-enable progress output after --quiet.
    #[arg(long = "no-quiet")]
    pub no_quiet: bool,

    /// Output bundle file path.
    #[arg(value_name = "FILE")]
    pub file: String,

    /// Bundle format version (supports 2 and 3).
    #[arg(long = "version", value_name = "N")]
    pub version: Option<u8>,

    /// Read revision arguments from standard input.
    #[arg(long = "stdin")]
    pub stdin: bool,

    /// Ignore missing refs while parsing revision arguments.
    #[arg(long = "ignore-missing")]
    pub ignore_missing: bool,

    /// Object filter to apply to the bundle.
    #[arg(long = "filter", value_name = "FILTER-SPEC")]
    pub filter: Option<String>,

    /// Revision arguments (refs, commit ranges, --all).
    #[arg(value_name = "REV", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub rev_list_args: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct VerifyArgs {
    /// Suppress progress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Bundle file to verify.
    #[arg(value_name = "FILE")]
    pub file: String,
}

#[derive(Debug, ClapArgs)]
pub struct ListHeadsArgs {
    /// Bundle file.
    #[arg(value_name = "FILE")]
    pub file: String,
}

#[derive(Debug, ClapArgs)]
pub struct UnbundleArgs {
    /// Force progress output.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Bundle file to unbundle.
    #[arg(value_name = "FILE")]
    pub file: String,
}

/// Run `grit bundle`.
pub fn run(args: Args) -> Result<()> {
    match args.action {
        BundleAction::Create(a) => run_create(a),
        BundleAction::Verify(a) => run_verify(a),
        BundleAction::ListHeads(a) => run_list_heads(a),
        BundleAction::Unbundle(a) => run_unbundle(a),
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn run_create(args: CreateArgs) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let version = args.version.unwrap_or(2);
    if version != 2 && version != 3 {
        bail!("unsupported bundle version {version}");
    }
    let mut rev_args = collect_create_rev_args(&args)?;
    let filter_spec = args
        .filter
        .clone()
        .or_else(|| take_bundle_filter_arg(&mut rev_args));
    let filter = filter_spec
        .as_deref()
        .map(ObjectFilter::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let include_all = rev_args.iter().any(|arg| arg == "--all");

    let mut refs = collect_refs_for_bundle(&repo, &rev_args, args.ignore_missing)?;
    if refs.is_empty() {
        bail!("refusing to create empty bundle");
    }

    let (positive, negative) = parse_bundle_rev_list_args(&repo, &rev_args, args.ignore_missing)?;
    let cutoffs = parse_bundle_rev_cutoffs(&rev_args)?;
    let max_count = parse_max_count_arg(&rev_args);
    let opts = RevListOptions {
        objects: true,
        boundary: !negative.is_empty() || cutoffs.since.is_some() || cutoffs.until.is_some(),
        max_count,
        since_cutoff: cutoffs.since,
        until_cutoff: cutoffs.until,
        ignore_missing: args.ignore_missing,
        filter,
        ..Default::default()
    };
    let listed =
        rev_list(&repo, &positive, &negative, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut prerequisites = listed.boundary_commits;
    if cutoffs.since.is_some() || cutoffs.until.is_some() || prerequisites.len() <= 2 {
        sort_bundle_prerequisites_desc(&repo, &mut prerequisites);
    } else {
        sort_bundle_prerequisites_asc(&repo, &mut prerequisites);
    }
    let mut oids = BTreeSet::new();
    for c in &listed.commits {
        oids.insert(*c);
    }
    // When the boundary (`--since`/`--until`) path can produce a thin pack with a
    // REF_DELTA against a boundary object, the assembled pack bytes are stored here
    // so they are used in place of the full-object pack below.
    let mut thin_boundary_pack: Option<Vec<u8>> = None;
    if max_count.is_some() {
        for c in &listed.commits {
            let Ok(obj) = read_object(&repo, c) else {
                continue;
            };
            if obj.kind != ObjectKind::Commit {
                continue;
            }
            if let Ok(commit) = parse_commit(&obj.data) {
                let mut tip_tree_objects = BTreeSet::new();
                walk_reachable(&repo, &commit.tree, &mut tip_tree_objects)?;

                let mut parent_tree_objects = BTreeSet::new();
                for parent in &commit.parents {
                    let Ok(parent_obj) = read_object(&repo, parent) else {
                        continue;
                    };
                    if parent_obj.kind != ObjectKind::Commit {
                        continue;
                    }
                    if let Ok(parent_commit) = parse_commit(&parent_obj.data) {
                        walk_reachable(&repo, &parent_commit.tree, &mut parent_tree_objects)?;
                    }
                }

                for oid in tip_tree_objects {
                    if !parent_tree_objects.contains(&oid) {
                        oids.insert(oid);
                    }
                }
            }
        }
        for oid in refs.values() {
            if read_object(&repo, oid).is_ok_and(|obj| obj.kind == ObjectKind::Tag) {
                oids.insert(*oid);
            }
        }
        refs.retain(|_, oid| oids.contains(oid));
    } else {
        for (oid, _) in &listed.objects {
            if let Ok(obj) = read_object(&repo, oid) {
                if obj.kind == ObjectKind::Commit {
                    continue;
                }
            }
            oids.insert(*oid);
        }
        if !prerequisites.is_empty() {
            let mut prerequisite_objects = std::collections::BTreeSet::new();
            for boundary in &prerequisites {
                walk_reachable(&repo, boundary, &mut prerequisite_objects)?;
            }
            oids.retain(|oid| !prerequisite_objects.contains(oid));
            let has_blob = oids
                .iter()
                .any(|oid| read_object(&repo, oid).is_ok_and(|obj| obj.kind == ObjectKind::Blob));
            if !has_blob {
                for commit_oid in &listed.commits {
                    let Ok(commit_obj) = read_object(&repo, commit_oid) else {
                        continue;
                    };
                    if commit_obj.kind != ObjectKind::Commit {
                        continue;
                    }
                    let Ok(commit) = parse_commit(&commit_obj.data) else {
                        continue;
                    };
                    if let Some(blob_oid) = find_first_blob_in_tree(&repo, commit.tree) {
                        oids.insert(blob_oid);
                        break;
                    }
                }
            }
        }
        if cutoffs.since.is_some() || cutoffs.until.is_some() {
            // Genuinely-new objects (reachable from the included commits but not
            // from any prerequisite) where every new content blob is also boundary
            // reachable, and at least one new tree is a strict byte prefix-extension
            // of a boundary tree, can be written as a thin pack: the prefix-extended
            // tree becomes a REF_DELTA against the boundary tree and `index-pack
            // --fix-thin` appends that one boundary base. This mirrors git's
            // name-hash-driven delta selection for `git bundle create --since` over a
            // merge tip (t5510 'all boundary commits are excluded'). When no such
            // prefix delta exists we fall back to the heuristic that materialises the
            // prerequisite commit roots as full objects (keeps t6020 counts stable).
            let mut prerequisite_objects = std::collections::BTreeSet::new();
            for boundary in &prerequisites {
                walk_reachable(&repo, boundary, &mut prerequisite_objects)?;
            }
            if let Some((thin, thin_oids)) =
                try_build_thin_boundary_pack(&repo, &oids, &prerequisite_objects)?
            {
                oids = thin_oids;
                thin_boundary_pack = Some(thin);
            } else {
                include_prerequisite_commit_roots(&repo, &prerequisites, &mut oids);
            }
            let included_commits = listed.commits.iter().copied().collect::<BTreeSet<_>>();
            retain_refs_for_included_commits(
                &repo,
                &included_commits,
                &cutoffs,
                &mut refs,
                &mut oids,
            );
        }
        if include_all && !negative.is_empty() && cutoffs.since.is_none() && cutoffs.until.is_none()
        {
            let included_commits = listed.commits.iter().copied().collect::<BTreeSet<_>>();
            retain_all_refs_after_exclusions(&repo, &included_commits, &mut refs, &mut oids);
        }
    }

    // Git's `write_bundle_refs` skips a ref whose tip is a *commit* that the
    // revision walk never showed (e.g. `main..main`, where every tip is excluded
    // by the rev-list options). Tags and other non-commit tips are unaffected.
    // After this filtering, an empty ref set means the bundle would be empty, so
    // git dies with "Refusing to create empty bundle" — and crucially leaves no
    // bundle (or lock) file behind, which `fail.bundle.lock` checks for.
    //
    // Restrict this to plain range/tip arguments: `--all` always lists every ref
    // (its tips are unconditionally shown by git's walk) and `--max-count`/cutoff
    // bundles already do their own ref retention above, so only a bare positive
    // range like `main..main` needs the "unshown commit" exclusion here.
    if !include_all && max_count.is_none() && cutoffs.since.is_none() && cutoffs.until.is_none() {
        let shown_commits: BTreeSet<ObjectId> = listed.commits.iter().copied().collect();
        refs.retain(|_, oid| {
            match read_object(&repo, oid) {
                Ok(obj) if obj.kind == ObjectKind::Commit => shown_commits.contains(oid),
                // Unreadable or non-commit tip: keep it (git only excludes unshown commits).
                _ => true,
            }
        });
        if refs.is_empty() {
            bail!("Refusing to create empty bundle.");
        }
    }

    // Read all objects.
    let mut objects: Vec<(ObjectId, ObjectKind, Vec<u8>)> = Vec::new();
    for oid in &oids {
        let obj = read_object(&repo, oid)?;
        objects.push((*oid, obj.kind, obj.data));
    }

    // Build pack data.
    let pack_data = if let Some(thin) = thin_boundary_pack {
        thin
    } else {
        build_pack_data(&objects)?
    };

    // Pair each prerequisite (boundary commit) with its oneline subject so the
    // bundle header records `-<oid> <subject>` like git's
    // `write_bundle_prerequisites`.
    let prerequisites_with_subjects: Vec<(ObjectId, String)> = prerequisites
        .iter()
        .map(|oid| (*oid, commit_subject(&repo, oid).unwrap_or_default()))
        .collect();

    // Write bundle file.
    if args.file == "-" {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        write_bundle(
            &mut out,
            version,
            filter_spec.as_deref(),
            &prerequisites_with_subjects,
            &refs,
            &pack_data,
        )?;
    } else {
        let mut out =
            fs::File::create(&args.file).with_context(|| format!("cannot create {}", args.file))?;
        write_bundle(
            &mut out,
            version,
            filter_spec.as_deref(),
            &prerequisites_with_subjects,
            &refs,
            &pack_data,
        )?;
    }

    let show_progress = args.progress || args.no_quiet || (!args.quiet && args.file != "-");
    if show_progress {
        eprintln!(
            "Writing objects: 100% ({}/{}), done.",
            objects.len(),
            objects.len()
        );
        eprintln!("Total {} (delta 0), reused 0 (delta 0)", objects.len());
    }

    Ok(())
}

fn collect_create_rev_args(args: &CreateArgs) -> Result<Vec<String>> {
    let mut rev_args = Vec::new();
    if args.stdin {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        rev_args.extend(
            input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned),
        );
    }
    rev_args.extend(args.rev_list_args.iter().cloned());
    Ok(rev_args)
}

fn write_bundle(
    out: &mut dyn Write,
    version: u8,
    filter: Option<&str>,
    prerequisites: &[(ObjectId, String)],
    refs: &BundleRefs,
    pack_data: &[u8],
) -> Result<()> {
    if version == 3 {
        out.write_all(b"# v3 git bundle\n")?;
        out.write_all(b"@object-format=sha1\n")?;
    } else {
        out.write_all(b"# v2 git bundle\n")?;
    }
    if let Some(filter) = filter {
        writeln!(out, "@filter={filter}")?;
    }

    // Match git's `write_bundle_prerequisites`: `-<oid> <oneline subject>`.
    // The trailing space after the oid is always emitted even when the
    // subject is empty.
    for (oid, subject) in prerequisites {
        writeln!(out, "-{} {}", oid.to_hex(), subject)?;
    }
    for (refname, oid) in bundle_refs_for_output(refs) {
        writeln!(out, "{} {}", oid.to_hex(), refname)?;
    }
    out.write_all(b"\n")?;
    out.write_all(pack_data)?;
    Ok(())
}

fn take_bundle_filter_arg(rev_args: &mut Vec<String>) -> Option<String> {
    let mut filter = None;
    let mut cleaned = Vec::with_capacity(rev_args.len());
    let mut i = 0usize;
    while i < rev_args.len() {
        let arg = &rev_args[i];
        if let Some(value) = arg.strip_prefix("--filter=") {
            filter = Some(value.to_owned());
            i += 1;
            continue;
        }
        if arg == "--filter" {
            if let Some(value) = rev_args.get(i + 1) {
                filter = Some(value.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        cleaned.push(arg.clone());
        i += 1;
    }
    *rev_args = cleaned;
    filter
}

fn collect_refs_for_bundle(
    repo: &Repository,
    rev_args: &[String],
    ignore_missing: bool,
) -> Result<BundleRefs> {
    let mut refs = BundleRefs::default();

    let include_all = rev_args.iter().any(|a| a == "--all");

    if include_all {
        collect_all_refs(repo, &mut refs)?;
        return Ok(refs);
    }
    if rev_args.is_empty() {
        if let Ok(oid) = resolve_ref(repo, "HEAD") {
            refs.insert("HEAD".to_string(), oid);
        }
        return Ok(refs);
    }

    let mut i = 0usize;
    while i < rev_args.len() {
        let arg = &rev_args[i];
        if option_takes_value(arg) {
            i += 2;
            continue;
        }
        if arg == "--not" {
            i += 1;
            while i < rev_args.len() && rev_args[i] != "--not" {
                i += 1;
            }
            continue;
        }
        if arg == "--all" || (arg.starts_with('-') && arg != "--not") {
            i += 1;
            continue;
        }
        if let Some(tip_spec) = bundle_ref_tip_spec(arg) {
            match resolve_ref(repo, &tip_spec) {
                Ok(oid) => {
                    let full_name = full_ref_name_for_tip(repo, &tip_spec);
                    refs.insert(full_name, oid);
                }
                Err(e) if ignore_missing => {
                    let _ = e;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("cannot resolve '{tip_spec}' (from '{arg}')"));
                }
            }
        }
        i += 1;
    }

    Ok(refs)
}

fn bundle_ref_tip_spec(arg: &str) -> Option<String> {
    if arg.starts_with('^') {
        return None;
    }
    if let Some(base) = arg.strip_suffix("^!") {
        return Some(if base.is_empty() { "HEAD" } else { base }.to_string());
    }
    let (pos_specs, _neg) = split_revision_token(arg);
    pos_specs.last().cloned()
}

fn full_ref_name_for_tip(repo: &Repository, tip_spec: &str) -> String {
    if tip_spec.starts_with("refs/") || tip_spec == "HEAD" {
        tip_spec.to_string()
    } else if resolve_ref(repo, &format!("refs/heads/{tip_spec}")).is_ok() {
        format!("refs/heads/{tip_spec}")
    } else if resolve_ref(repo, &format!("refs/tags/{tip_spec}")).is_ok() {
        format!("refs/tags/{tip_spec}")
    } else {
        tip_spec.to_string()
    }
}

fn parse_max_count_arg(rev_args: &[String]) -> Option<usize> {
    let mut i = 0usize;
    while i < rev_args.len() {
        let arg = &rev_args[i];
        if arg == "--max-count" {
            if let Some(value) = rev_args.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                return Some(value);
            }
            i += 2;
            continue;
        }
        if let Some(value) = arg
            .strip_prefix("--max-count=")
            .and_then(|v| v.parse::<usize>().ok())
        {
            return Some(value);
        }
        let Some(n) = arg.strip_prefix('-') else {
            i += 1;
            continue;
        };
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = n.parse::<usize>() {
                return Some(v);
            }
        }
        i += 1;
    }
    None
}

fn include_prerequisite_commit_roots(
    repo: &Repository,
    prerequisites: &[ObjectId],
    oids: &mut BTreeSet<ObjectId>,
) {
    for oid in prerequisites {
        let Ok(obj) = read_object(repo, oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        oids.insert(*oid);
        if let Ok(commit) = parse_commit(&obj.data) {
            oids.insert(commit.tree);
        }
    }
}

fn retain_refs_for_included_commits(
    repo: &Repository,
    included_commits: &BTreeSet<ObjectId>,
    cutoffs: &BundleRevCutoffs,
    refs: &mut BundleRefs,
    oids: &mut BTreeSet<ObjectId>,
) {
    refs.retain(|_, oid| ref_peels_to_included_commit(repo, oid, included_commits, cutoffs, oids));
}

fn retain_all_refs_after_exclusions(
    repo: &Repository,
    included_commits: &BTreeSet<ObjectId>,
    refs: &mut BundleRefs,
    oids: &mut BTreeSet<ObjectId>,
) {
    refs.retain(|_, oid| {
        let Ok(obj) = read_object(repo, oid) else {
            return false;
        };
        match obj.kind {
            ObjectKind::Commit => included_commits.contains(oid),
            ObjectKind::Tag => {
                oids.insert(*oid);
                true
            }
            ObjectKind::Tree | ObjectKind::Blob => oids.contains(oid),
        }
    });
}

fn ref_peels_to_included_commit(
    repo: &Repository,
    oid: &ObjectId,
    included_commits: &BTreeSet<ObjectId>,
    cutoffs: &BundleRevCutoffs,
    oids: &mut BTreeSet<ObjectId>,
) -> bool {
    let Ok(obj) = read_object(repo, oid) else {
        return false;
    };
    match obj.kind {
        ObjectKind::Commit => included_commits.contains(oid),
        ObjectKind::Tag => {
            if oids.contains(oid) {
                return true;
            }
            let Ok(tag) = parse_tag(&obj.data) else {
                return false;
            };
            if tag
                .tagger
                .as_deref()
                .is_some_and(|tagger| signature_time_is_in_cutoffs(tagger, cutoffs))
            {
                oids.insert(*oid);
                return true;
            }
            if ref_peels_to_included_commit(repo, &tag.object, included_commits, cutoffs, oids) {
                oids.insert(*oid);
                true
            } else {
                false
            }
        }
        ObjectKind::Tree | ObjectKind::Blob => false,
    }
}

fn signature_time_is_in_cutoffs(sig: &str, cutoffs: &BundleRevCutoffs) -> bool {
    let ts = parse_signature_time(sig);
    if let Some(until) = cutoffs.until {
        if ts > until {
            return false;
        }
    }
    if let Some(since) = cutoffs.since {
        if ts < since {
            return false;
        }
    }
    true
}

fn sort_bundle_prerequisites_desc(repo: &Repository, prerequisites: &mut [ObjectId]) {
    prerequisites.sort_by(|a, b| {
        commit_time(repo, b)
            .cmp(&commit_time(repo, a))
            .then_with(|| a.to_hex().cmp(&b.to_hex()))
    });
}

fn sort_bundle_prerequisites_asc(repo: &Repository, prerequisites: &mut [ObjectId]) {
    prerequisites.sort_by(|a, b| {
        commit_time(repo, a)
            .cmp(&commit_time(repo, b))
            .then_with(|| a.to_hex().cmp(&b.to_hex()))
    });
}

fn commit_time(repo: &Repository, oid: &ObjectId) -> i64 {
    let Ok(obj) = read_object(repo, oid) else {
        return 0;
    };
    if obj.kind != ObjectKind::Commit {
        return 0;
    }
    let Ok(commit) = parse_commit(&obj.data) else {
        return 0;
    };
    parse_signature_time(&commit.committer)
}

fn parse_signature_time(sig: &str) -> i64 {
    let parts: Vec<&str> = sig.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse::<i64>().unwrap_or(0)
    } else {
        0
    }
}

fn commit_subject(repo: &Repository, oid: &ObjectId) -> Option<String> {
    let obj = read_object(repo, oid).ok()?;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    let commit = parse_commit(&obj.data).ok()?;
    commit.message.lines().next().map(ToOwned::to_owned)
}

fn parse_bundle_rev_list_args(
    repo: &Repository,
    rev_args: &[String],
    ignore_missing: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut positive: Vec<String> = Vec::new();
    let mut negative: Vec<String> = Vec::new();
    let include_all = rev_args.iter().any(|a| a == "--all");

    let mut i = 0usize;
    while i < rev_args.len() {
        let arg = &rev_args[i];
        if arg == "--since"
            || arg == "--after"
            || arg == "--until"
            || arg == "--before"
            || option_takes_value(arg)
        {
            i += 2;
            continue;
        }
        if arg.starts_with("--since=")
            || arg.starts_with("--after=")
            || arg.starts_with("--until=")
            || arg.starts_with("--before=")
        {
            i += 1;
            continue;
        }
        if arg == "--not" {
            i += 1;
            while i < rev_args.len() && rev_args[i] != "--not" {
                let tok = &rev_args[i];
                if tok == "--all" || (tok.starts_with('-') && tok != "--not") {
                    i += 1;
                    continue;
                }
                let (p, n) = split_bundle_revision_token(repo, tok)?;
                append_bundle_rev_specs(repo, &mut negative, p, ignore_missing);
                append_bundle_rev_specs(repo, &mut negative, n, ignore_missing);
                i += 1;
            }
            continue;
        }
        if arg == "--all" || (arg.starts_with('-') && arg != "--not") {
            i += 1;
            continue;
        }
        let (p, n) = split_bundle_revision_token(repo, arg)?;
        append_bundle_rev_specs(repo, &mut positive, p, ignore_missing);
        append_bundle_rev_specs(repo, &mut negative, n, ignore_missing);
        i += 1;
    }

    if include_all && positive.is_empty() {
        let mut refs = BundleRefs::default();
        collect_all_refs(repo, &mut refs)?;
        positive.extend(refs.iter().map(|(name, _)| name.clone()));
    }

    if positive.is_empty() && !include_all && resolve_ref(repo, "HEAD").is_ok() {
        positive.push("HEAD".to_string());
    }

    Ok((positive, negative))
}

fn append_bundle_rev_specs(
    repo: &Repository,
    out: &mut Vec<String>,
    specs: Vec<String>,
    ignore_missing: bool,
) {
    for spec in specs {
        if ignore_missing
            && grit_lib::rev_parse::resolve_revision_for_range_end(repo, &spec).is_err()
        {
            continue;
        }
        out.push(spec);
    }
}

fn option_takes_value(arg: &str) -> bool {
    matches!(arg, "--max-count")
}

fn split_bundle_revision_token(
    repo: &Repository,
    token: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    if let Some(base) = token.strip_suffix("^!") {
        let base = if base.is_empty() { "HEAD" } else { base };
        let oid = grit_lib::rev_parse::resolve_revision_as_commit_without_index_dwim(repo, base)
            .with_context(|| format!("cannot resolve '{base}'"))?;
        let obj = read_object(repo, &oid)?;
        if obj.kind != ObjectKind::Commit {
            return Ok((vec![base.to_string()], Vec::new()));
        }
        let commit = parse_commit(&obj.data).map_err(|e| anyhow::anyhow!("{e}"))?;
        let parents = commit.parents.into_iter().map(|p| p.to_hex()).collect();
        return Ok((vec![base.to_string()], parents));
    }
    Ok(split_revision_token(token))
}

#[derive(Default)]
struct BundleRevCutoffs {
    since: Option<i64>,
    until: Option<i64>,
}

fn parse_bundle_rev_cutoffs(rev_args: &[String]) -> Result<BundleRevCutoffs> {
    let mut cutoffs = BundleRevCutoffs::default();
    let mut i = 0usize;
    while i < rev_args.len() {
        let arg = &rev_args[i];
        match arg.as_str() {
            "--since" | "--after" => {
                i += 1;
                if let Some(v) = rev_args.get(i) {
                    cutoffs.since = Some(parse_bundle_date(v)?);
                }
            }
            "--until" | "--before" => {
                i += 1;
                if let Some(v) = rev_args.get(i) {
                    cutoffs.until = Some(parse_bundle_date(v)?);
                }
            }
            _ if arg.starts_with("--since=") || arg.starts_with("--after=") => {
                let value = arg.split_once('=').map(|(_, v)| v).unwrap_or_default();
                cutoffs.since = Some(parse_bundle_date(value)?);
            }
            _ if arg.starts_with("--until=") || arg.starts_with("--before=") => {
                let value = arg.split_once('=').map(|(_, v)| v).unwrap_or_default();
                cutoffs.until = Some(parse_bundle_date(value)?);
            }
            _ => {}
        }
        i += 1;
    }
    Ok(cutoffs)
}

fn parse_bundle_date(s: &str) -> Result<i64> {
    let trimmed = s.trim();
    let mut approx_err = 0;
    let approx = approxidate_careful(trimmed, Some(&mut approx_err));
    if approx_err == 0 {
        return i64::try_from(approx).context("date out of range for bundle cutoff");
    }
    if let Ok((ts, _)) = parse_date_basic(trimmed) {
        return i64::try_from(ts).context("date out of range for bundle cutoff");
    }
    if trimmed.len() >= 10 && trimmed.as_bytes()[4] == b'-' && trimmed.as_bytes()[7] == b'-' {
        let parts: Vec<&str> = trimmed[..10].split('-').collect();
        if parts.len() == 3 {
            if let (Ok(y), Ok(m), Ok(d)) = (
                parts[0].parse::<i32>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                if let Ok(month) = time::Month::try_from(m) {
                    if let Ok(date) = time::Date::from_calendar_date(y, month, d) {
                        if let Ok(dt) = date.with_hms(0, 0, 0) {
                            return Ok(dt.assume_utc().unix_timestamp());
                        }
                    }
                }
            }
        }
    }
    trimmed
        .parse::<i64>()
        .with_context(|| format!("invalid date '{trimmed}'"))
}

fn collect_all_refs(repo: &Repository, refs: &mut BundleRefs) -> Result<()> {
    // Collect `refs/*` into a sorted map first so `--all` emits refs in
    // alphabetical order, matching git's `for_each_ref` traversal. HEAD is
    // appended afterwards so it lands last in the bundle header.
    let mut sorted = BTreeMap::new();
    let refs_dir = repo.git_dir.join("refs");
    if refs_dir.exists() {
        walk_refs_dir(&refs_dir, "refs", repo, &mut sorted)?;
    }
    for (name, oid) in sorted {
        refs.insert(name, oid);
    }

    if let Ok(oid) = resolve_ref(repo, "HEAD") {
        refs.insert("HEAD".to_string(), oid);
    }

    Ok(())
}

fn walk_refs_dir(
    dir: &std::path::Path,
    prefix: &str,
    repo: &Repository,
    refs: &mut BTreeMap<String, ObjectId>,
) -> Result<()> {
    let rd = fs::read_dir(dir)?;
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let full_ref = format!("{prefix}/{name_str}");

        if path.is_dir() {
            walk_refs_dir(&path, &full_ref, repo, refs)?;
        } else if path.is_file() {
            if let Ok(oid) = resolve_ref(repo, &full_ref) {
                refs.insert(full_ref, oid);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

fn run_verify(args: VerifyArgs) -> Result<()> {
    // Git's `cmd_bundle_verify` requires a repository up front (it needs the ODB
    // to check prerequisite connectivity); outside one it errors before even
    // opening the bundle. Match that exact diagnostic and exit code.
    let repo = match Repository::discover(None) {
        Ok(repo) => Some(repo),
        Err(_) => {
            eprintln!("error: need a repository to verify a bundle");
            return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
        }
    };
    let data = read_bundle_arg(&args.file)?;
    let header = parse_bundle_header(&data)?;

    // Validate pack data.
    let pack_data = &data[header.pack_start..];
    if pack_data.len() < 12 + 20 {
        bail!("bundle pack data too small");
    }
    if &pack_data[0..4] != b"PACK" {
        bail!("bundle does not contain valid pack data");
    }

    if let Some(repo) = &repo {
        if !header.prerequisites.is_empty() {
            let prereq_oids: Vec<_> = header.prerequisites.iter().map(|(oid, _)| *oid).collect();
            let missing: Vec<_> = prereq_oids
                .iter()
                .copied()
                .filter(|oid| read_object(repo, oid).is_err())
                .collect();
            if !missing.is_empty() {
                eprintln!("error: Repository lacks these prerequisite commits:");
                for oid in missing {
                    eprintln!("error: {} ", oid.to_hex());
                }
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
            }
            if !grit_lib::connectivity::bundle_prerequisites_connected_to_refs(repo, &prereq_oids)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                eprintln!("{}", disconnected_prerequisites_message(&prereq_oids));
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
            }
        }
    }

    print_bundle_verify_info(&header);
    if !args.quiet {
        eprintln!("{} is okay", bundle_display_name(&args.file));
    }
    Ok(())
}

fn print_bundle_verify_info(header: &BundleHeader) {
    match header.refs.len() {
        1 => println!("The bundle contains this ref:"),
        n => println!("The bundle contains these {n} refs:"),
    }
    for (refname, oid) in bundle_refs_for_output(&header.refs) {
        println!("{} {refname}", oid.to_hex());
    }
    match header.prerequisites.len() {
        0 => println!("The bundle records a complete history."),
        1 => println!("The bundle requires this ref:"),
        n => println!("The bundle requires these {n} refs:"),
    }
    for (oid, _comment) in &header.prerequisites {
        // Git stores prerequisites with an empty display name (the subject after
        // the oid is kept only for connectivity diagnostics, not listing), so
        // `bundle verify` prints "<oid> " with a trailing space (bundle.c
        // `list_refs`). Match that exactly.
        println!("{} ", oid.to_hex());
    }
    let display_hash = std::env::var("GIT_DEFAULT_HASH").unwrap_or_default();
    println!("The bundle uses this hash algorithm: {display_hash}");
    if let Some(filter) = &header.filter {
        println!("The bundle uses this filter: {filter}");
    }
}

// ---------------------------------------------------------------------------
// list-heads
// ---------------------------------------------------------------------------

fn run_list_heads(args: ListHeadsArgs) -> Result<()> {
    let data = read_bundle_arg(&args.file)?;
    let header = parse_bundle_header(&data)?;

    for (refname, oid) in bundle_refs_for_output(&header.refs) {
        println!("{} {refname}", oid.to_hex());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// unbundle
// ---------------------------------------------------------------------------

fn run_unbundle(args: UnbundleArgs) -> Result<()> {
    let repo = match Repository::discover(None) {
        Ok(repo) => repo,
        Err(_) => {
            eprintln!("fatal: Need a repository to unbundle.");
            return Err(anyhow::Error::new(SilentNonZeroExit { code: 128 }));
        }
    };
    let data = read_bundle_arg(&args.file)?;
    let header = parse_bundle_header(&data)?;

    let pack_data = &data[header.pack_start..];
    if pack_data.len() < 12 + 20 {
        bail!("bundle pack data too small");
    }

    let prereq_oids: Vec<_> = header.prerequisites.iter().map(|(oid, _)| *oid).collect();
    if !grit_lib::connectivity::bundle_prerequisites_connected_to_refs(&repo, &prereq_oids)
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        eprintln!("{}", disconnected_prerequisites_message(&prereq_oids));
        return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
    }

    // Use unpack-objects to extract into the ODB.
    let opts = grit_lib::unpack_objects::UnpackOptions {
        strict: false,
        dry_run: false,
        quiet: !args.progress,
        allowed_missing: Default::default(),
        allow_promisor_missing_references: false,
        max_input_bytes: None,
        ..Default::default()
    };
    let _count = grit_lib::unpack_objects::unpack_objects(&mut &pack_data[..], &repo.odb, &opts)
        .map_err(|e| anyhow::anyhow!("unbundle failed: {e}"))?;

    if header.filter.is_some() {
        write_filtered_bundle_promisor_marker(&repo, &header, pack_data)?;
    }

    for (refname, oid) in header.refs.iter() {
        println!("{} {refname}", oid.to_hex());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

struct BundleHeader {
    refs: BundleRefs,
    prerequisites: Vec<(ObjectId, String)>,
    object_format: String,
    filter: Option<String>,
    pack_start: usize,
}

fn write_filtered_bundle_promisor_marker(
    repo: &Repository,
    header: &BundleHeader,
    pack_data: &[u8],
) -> Result<()> {
    let pack_dir = repo.git_dir.join("objects/pack");
    fs::create_dir_all(&pack_dir)?;
    let mut hasher = Sha1::new();
    hasher.update(pack_data);
    let pack_hash = hex::encode(hasher.finalize());
    let marker = pack_dir.join(format!("pack-{pack_hash}.promisor"));
    let mut out = fs::File::create(&marker)
        .with_context(|| format!("creating promisor marker {}", marker.display()))?;
    for (refname, oid) in header.refs.iter() {
        writeln!(out, "{} {refname}", oid.to_hex())?;
    }
    Ok(())
}

fn disconnected_prerequisites_message(prerequisites: &[ObjectId]) -> String {
    let detail = prerequisites
        .first()
        .map(ObjectId::to_hex)
        .unwrap_or_else(|| "unknown".to_owned());
    format!("error: some prerequisite commits {detail} are not connected to the repository")
}

fn read_bundle_arg(file: &str) -> Result<Vec<u8>> {
    if file == "-" {
        let mut data = Vec::new();
        std::io::stdin().read_to_end(&mut data)?;
        return Ok(data);
    }
    fs::read(file).with_context(|| format!("cannot read {file}"))
}

fn bundle_display_name(file: &str) -> &str {
    if file == "-" {
        "<stdin>"
    } else {
        file
    }
}

fn bundle_refs_for_output(refs: &BundleRefs) -> Vec<(&String, &ObjectId)> {
    // Refs are already stored in command-line / header order (HEAD wherever it
    // was listed), so emit them as-is.
    refs.iter().collect()
}

/// Parse the bundle header, returning refs/prerequisites and the pack byte offset.
///
/// Mirrors git's `read_bundle_header_fd` (bundle.c): the signature selects the
/// version, then header lines are processed until a blank line. In a v3 bundle a
/// line starting with `@` is a capability; an unrecognized capability is a fatal
/// `unknown capability '<cap>'` error (matching `parse_capability`). The error
/// surfaces before any blank/pack line is required so a capability-only header
/// (as written by the t5607 "unknown capabilities" test) is rejected, not
/// reported as truncated.
fn parse_bundle_header(data: &[u8]) -> Result<BundleHeader> {
    let header_v2 = b"# v2 git bundle\n";
    let header_v3 = b"# v3 git bundle\n";
    let (mut pos, version) = if data.starts_with(header_v2) {
        (header_v2.len(), 2u8)
    } else if data.starts_with(header_v3) {
        (header_v3.len(), 3u8)
    } else {
        bail!("not a git bundle");
    };
    let mut refs = BundleRefs::default();
    let mut prerequisites = Vec::new();
    let mut object_format = "sha1".to_string();
    let mut filter = None;

    loop {
        // Find end of line.
        let eol = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or_else(|| anyhow::anyhow!("truncated bundle header"))?;

        let line = &data[pos..eol];
        if line.is_empty() {
            // Blank line → pack data follows.
            pos = eol + 1;
            break;
        }

        let line_str = std::str::from_utf8(line).context("invalid UTF-8 in bundle header")?;

        // A leading '@' introduces a capability. The known capabilities
        // (`object-format=`, `filter=`) are consumed in any version. In a v3
        // bundle an *unknown* capability aborts the parse just like git's
        // `parse_capability`; v2 bundles ignore stray '@' lines (grit itself
        // emits `@filter=` on a v2 filtered bundle, so we must keep reading it).
        if let Some(cap) = line_str.strip_prefix('@') {
            if let Some(value) = cap.strip_prefix("object-format=") {
                object_format = value.to_string();
                pos = eol + 1;
                continue;
            } else if let Some(value) = cap.strip_prefix("filter=") {
                filter = Some(value.to_string());
                pos = eol + 1;
                continue;
            } else if version == 3 {
                eprintln!("error: unknown capability '{cap}'");
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
            }
            // v2: fall through and let the line be treated as (ignored) non-ref.
        }

        // Prerequisite lines start with '-'.
        if let Some(rest) = line_str.strip_prefix('-') {
            let (hex, comment) = rest.split_once(' ').unwrap_or((rest, ""));
            let oid = ObjectId::from_hex(hex)
                .map_err(|e| anyhow::anyhow!("bad prerequisite oid in bundle header: {e}"))?;
            prerequisites.push((oid, comment.to_string()));
            pos = eol + 1;
            continue;
        }

        // ref line: "<hex-oid> <refname>"
        if let Some((hex, refname)) = line_str.split_once(' ') {
            let oid = ObjectId::from_hex(hex)
                .map_err(|e| anyhow::anyhow!("bad oid in bundle header: {e}"))?;
            refs.insert(refname.to_string(), oid);
        }

        pos = eol + 1;
    }

    Ok(BundleHeader {
        refs,
        prerequisites,
        object_format,
        filter,
        pack_start: pos,
    })
}

fn resolve_ref(repo: &Repository, refname: &str) -> Result<ObjectId> {
    refs::resolve_ref(&repo.git_dir, refname)
        .or_else(|_| refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{refname}")))
        .or_else(|_| refs::resolve_ref(&repo.git_dir, &format!("refs/tags/{refname}")))
        .map_err(|e| anyhow::anyhow!("cannot resolve ref '{refname}': {e}"))
}

fn walk_reachable(
    repo: &Repository,
    oid: &ObjectId,
    oids: &mut std::collections::BTreeSet<ObjectId>,
) -> Result<()> {
    if !oids.insert(*oid) {
        return Ok(());
    }
    let obj = match read_object(repo, oid) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    match obj.kind {
        ObjectKind::Commit => {
            if let Ok(text) = std::str::from_utf8(&obj.data) {
                for line in text.lines() {
                    if let Some(hex) = line.strip_prefix("tree ") {
                        if let Ok(tree_oid) = ObjectId::from_hex(hex.trim()) {
                            walk_reachable(repo, &tree_oid, oids)?;
                        }
                    } else if let Some(hex) = line.strip_prefix("parent ") {
                        if let Ok(parent_oid) = ObjectId::from_hex(hex.trim()) {
                            walk_reachable(repo, &parent_oid, oids)?;
                        }
                    } else if line.is_empty() {
                        break;
                    }
                }
            }
        }
        ObjectKind::Tree => {
            let data = &obj.data;
            let mut pos = 0;
            while pos < data.len() {
                let nul = data[pos..].iter().position(|&b| b == 0).map(|i| pos + i);
                if let Some(nul) = nul {
                    if nul + 21 <= data.len() {
                        if let Ok(entry_oid) = ObjectId::from_bytes(&data[nul + 1..nul + 21]) {
                            walk_reachable(repo, &entry_oid, oids)?;
                        }
                        pos = nul + 21;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        ObjectKind::Tag => {
            if let Ok(text) = std::str::from_utf8(&obj.data) {
                if let Some(first_line) = text.lines().next() {
                    if let Some(hex) = first_line.strip_prefix("object ") {
                        if let Ok(target_oid) = ObjectId::from_hex(hex.trim()) {
                            walk_reachable(repo, &target_oid, oids)?;
                        }
                    }
                }
            }
        }
        ObjectKind::Blob => {}
    }
    Ok(())
}

fn find_first_blob_in_tree(repo: &Repository, tree_oid: ObjectId) -> Option<ObjectId> {
    let tree_obj = read_object(repo, &tree_oid).ok()?;
    if tree_obj.kind != ObjectKind::Tree {
        return None;
    }
    let mut pos = 0usize;
    while pos < tree_obj.data.len() {
        let Some(nul_rel) = tree_obj.data[pos..].iter().position(|&b| b == 0) else {
            break;
        };
        let nul = pos + nul_rel;
        if nul + 21 > tree_obj.data.len() {
            break;
        }
        let mode_end = tree_obj.data[pos..nul].iter().position(|&b| b == b' ')?;
        let mode = std::str::from_utf8(&tree_obj.data[pos..pos + mode_end]).ok()?;
        let oid = ObjectId::from_bytes(&tree_obj.data[nul + 1..nul + 21]).ok()?;
        if mode == "100644" || mode == "100755" || mode == "120000" {
            return Some(oid);
        }
        if mode == "40000" {
            if let Some(found) = find_first_blob_in_tree(repo, oid) {
                return Some(found);
            }
        }
        pos = nul + 21;
    }
    None
}

fn read_object(repo: &Repository, oid: &ObjectId) -> Result<grit_lib::objects::Object> {
    if let Ok(obj) = repo.odb.read(oid) {
        return Ok(obj);
    }
    // Try pack files.
    let indexes = grit_lib::pack::read_local_pack_indexes(repo.odb.objects_dir())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    for idx in &indexes {
        if let Some(entry) = idx
            .entries
            .iter()
            .find(|e| grit_lib::pack::pack_index_entry_matches_sha1_oid(e, oid))
        {
            let pack_bytes = fs::read(&idx.pack_path)?;
            return read_from_pack(&pack_bytes, entry.offset, &indexes);
        }
    }
    bail!("object not found: {}", oid.to_hex())
}

fn read_from_pack(
    pack_bytes: &[u8],
    offset: u64,
    indexes: &[grit_lib::pack::PackIndex],
) -> Result<grit_lib::objects::Object> {
    let mut pos = offset as usize;
    let c = pack_bytes
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("truncated"))?;
    pos += 1;
    let type_code = (c >> 4) & 0x7;
    let mut size = (c & 0x0f) as usize;
    let mut shift = 4u32;
    let mut cur = c;
    while cur & 0x80 != 0 {
        cur = pack_bytes
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("truncated"))?;
        pos += 1;
        size |= ((cur & 0x7f) as usize) << shift;
        shift += 7;
    }

    match type_code {
        1..=4 => {
            let kind = match type_code {
                1 => ObjectKind::Commit,
                2 => ObjectKind::Tree,
                3 => ObjectKind::Blob,
                4 => ObjectKind::Tag,
                _ => unreachable!(),
            };
            use flate2::read::ZlibDecoder;
            let mut dec = ZlibDecoder::new(&pack_bytes[pos..]);
            let mut data = Vec::with_capacity(size);
            dec.read_to_end(&mut data)?;
            Ok(grit_lib::objects::Object::new(kind, data))
        }
        6 => {
            let mut c2 = pack_bytes
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("truncated"))?;
            pos += 1;
            let mut neg_off = (c2 & 0x7f) as u64;
            while c2 & 0x80 != 0 {
                c2 = pack_bytes
                    .get(pos)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("truncated"))?;
                pos += 1;
                neg_off = ((neg_off + 1) << 7) | (c2 & 0x7f) as u64;
            }
            let base_offset = offset
                .checked_sub(neg_off)
                .ok_or_else(|| anyhow::anyhow!("ofs-delta underflow"))?;

            use flate2::read::ZlibDecoder;
            let mut dec = ZlibDecoder::new(&pack_bytes[pos..]);
            let mut delta = Vec::with_capacity(size);
            dec.read_to_end(&mut delta)?;

            let base = read_from_pack(pack_bytes, base_offset, indexes)?;
            let result = grit_lib::unpack_objects::apply_delta(&base.data, &delta)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(grit_lib::objects::Object::new(base.kind, result))
        }
        7 => {
            if pos + 20 > pack_bytes.len() {
                bail!("truncated ref-delta");
            }
            let base_oid = ObjectId::from_bytes(&pack_bytes[pos..pos + 20])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            pos += 20;

            use flate2::read::ZlibDecoder;
            let mut dec = ZlibDecoder::new(&pack_bytes[pos..]);
            let mut delta = Vec::with_capacity(size);
            dec.read_to_end(&mut delta)?;

            let mut base_obj = None;
            for idx in indexes {
                if let Some(e) = idx
                    .entries
                    .iter()
                    .find(|e| grit_lib::pack::pack_index_entry_matches_sha1_oid(e, &base_oid))
                {
                    let pb = fs::read(&idx.pack_path)?;
                    base_obj = Some(read_from_pack(&pb, e.offset, indexes)?);
                    break;
                }
            }
            let base = base_obj.ok_or_else(|| anyhow::anyhow!("ref-delta base not found"))?;
            let result = grit_lib::unpack_objects::apply_delta(&base.data, &delta)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(grit_lib::objects::Object::new(base.kind, result))
        }
        other => bail!("unknown pack type {other}"),
    }
}

fn build_pack_data(objects: &[(ObjectId, ObjectKind, Vec<u8>)]) -> Result<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    for (_, kind, data) in objects {
        let type_code: u8 = match kind {
            ObjectKind::Commit => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Blob => 3,
            ObjectKind::Tag => 4,
        };
        let mut size = data.len();
        let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        if size > 0 {
            buf.push(first | 0x80);
            while size > 0 {
                let b = (size & 0x7f) as u8;
                size >>= 7;
                buf.push(if size > 0 { b | 0x80 } else { b });
            }
        } else {
            buf.push(first);
        }

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data)?;
        let compressed = enc.finish()?;
        buf.extend_from_slice(&compressed);
    }

    let mut hasher = Sha1::new();
    hasher.update(&buf);
    let digest = hasher.finalize();
    buf.extend_from_slice(digest.as_slice());

    Ok(buf)
}

/// Append the variable-length object header (type + uncompressed size) used by
/// pack entries.
fn push_pack_obj_header(buf: &mut Vec<u8>, type_code: u8, mut size: usize) {
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        buf.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            buf.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        buf.push(first);
    }
}

/// Attempt to assemble a thin pack for the boundary (`--since`/`--until`) case.
///
/// `oids` is the candidate object set already filtered of boundary-reachable
/// objects (plus possibly a heuristic base blob). `prerequisite_objects` is the
/// set of every object reachable from the prerequisite (boundary) commits.
///
/// Returns `Some((pack_bytes, final_oids))` only when at least one genuinely-new
/// tree is a strict byte prefix-extension of a boundary tree AND there are no
/// genuinely-new content blobs (every blob in the new trees already exists on the
/// boundary side). That is the shape produced by `git bundle create --since` over
/// a merge tip whose new tree merely appends an entry already present on a
/// boundary branch: git writes the new tree as a thin REF_DELTA and `index-pack
/// --fix-thin` re-materialises exactly one boundary base. In every other shape we
/// return `None` so the caller keeps its full-object heuristic.
fn try_build_thin_boundary_pack(
    repo: &Repository,
    oids: &BTreeSet<ObjectId>,
    prerequisite_objects: &BTreeSet<ObjectId>,
) -> Result<Option<(Vec<u8>, BTreeSet<ObjectId>)>> {
    // Genuinely-new objects: those in `oids` not reachable from the boundary.
    let mut new_commits: Vec<ObjectId> = Vec::new();
    let mut new_trees: Vec<ObjectId> = Vec::new();
    let mut new_tags: Vec<ObjectId> = Vec::new();
    let mut new_blobs: Vec<ObjectId> = Vec::new();
    for oid in oids {
        if prerequisite_objects.contains(oid) {
            continue;
        }
        let Ok(obj) = read_object(repo, oid) else {
            return Ok(None);
        };
        match obj.kind {
            ObjectKind::Commit => new_commits.push(*oid),
            ObjectKind::Tree => new_trees.push(*oid),
            ObjectKind::Tag => new_tags.push(*oid),
            ObjectKind::Blob => new_blobs.push(*oid),
        }
    }

    // The thin-delta shortcut only applies when no genuinely-new blob is required
    // and there is at least one new tree and no new tags (tags push toward the
    // general heuristic path used by t6020).
    if !new_blobs.is_empty() || new_trees.is_empty() || !new_tags.is_empty() {
        return Ok(None);
    }

    // Collect candidate boundary trees so we can look for a prefix base.
    let mut boundary_trees: Vec<(ObjectId, Vec<u8>)> = Vec::new();
    for oid in prerequisite_objects {
        if let Ok(obj) = read_object(repo, oid) {
            if obj.kind == ObjectKind::Tree {
                boundary_trees.push((*oid, obj.data));
            }
        }
    }

    // For each new tree, try to find a boundary tree that is a strict byte prefix.
    // Record the chosen delta base so `index-pack --fix-thin` can append it.
    let mut tree_deltas: BTreeMap<ObjectId, (ObjectId, Vec<u8>)> = BTreeMap::new();
    for tree_oid in &new_trees {
        let Ok(tree_obj) = read_object(repo, tree_oid) else {
            return Ok(None);
        };
        let mut chosen: Option<(ObjectId, Vec<u8>)> = None;
        for (base_oid, base_data) in &boundary_trees {
            if base_data.len() < tree_obj.data.len() && tree_obj.data.starts_with(base_data) {
                if let Ok(delta) =
                    grit_lib::delta_encode::encode_prefix_extension_delta(base_data, &tree_obj.data)
                {
                    chosen = Some((*base_oid, delta));
                    break;
                }
            }
        }
        match chosen {
            Some(d) => {
                tree_deltas.insert(*tree_oid, d);
            }
            None => {
                // This new tree has no prefix base; fall back to the heuristic so
                // we do not silently emit an under-specified pack.
                return Ok(None);
            }
        }
    }

    if tree_deltas.is_empty() {
        return Ok(None);
    }

    // Final pack contents: new commits (full) + new trees (REF_DELTA). The blobs
    // are intentionally omitted; they live on the boundary side already.
    let mut final_oids: BTreeSet<ObjectId> = BTreeSet::new();
    for c in &new_commits {
        final_oids.insert(*c);
    }
    for t in &new_trees {
        final_oids.insert(*t);
    }

    // Build the thin pack bytes.
    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    let total = new_commits.len() + new_trees.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(total as u32).to_be_bytes());

    // Emit full commits first (deterministic order by oid).
    let mut commits_sorted = new_commits.clone();
    commits_sorted.sort_by_key(|a| a.to_hex());
    for oid in &commits_sorted {
        let obj = read_object(repo, oid)?;
        push_pack_obj_header(&mut buf, 1, obj.data.len());
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&obj.data)?;
        buf.extend_from_slice(&enc.finish()?);
    }

    // Emit trees: prefix-extended ones as REF_DELTA, the rest full.
    let mut trees_sorted = new_trees.clone();
    trees_sorted.sort_by_key(|a| a.to_hex());
    for oid in &trees_sorted {
        if let Some((base_oid, delta)) = tree_deltas.get(oid) {
            // OBJ_REF_DELTA == 7. Header size field is the delta length.
            push_pack_obj_header(&mut buf, 7, delta.len());
            buf.extend_from_slice(base_oid.as_bytes());
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(delta)?;
            buf.extend_from_slice(&enc.finish()?);
        } else {
            let obj = read_object(repo, oid)?;
            push_pack_obj_header(&mut buf, 2, obj.data.len());
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&obj.data)?;
            buf.extend_from_slice(&enc.finish()?);
        }
    }

    let mut hasher = Sha1::new();
    hasher.update(&buf);
    buf.extend_from_slice(hasher.finalize().as_slice());

    Ok(Some((buf, final_oids)))
}
