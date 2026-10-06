//! Local fetch over `grit upload-pack` with skipping negotiation (protocol v0/v1 ref ads, or v2
//! capability preamble with ref names merged from the remote repository when needed).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::zero_oid;
use grit_lib::fetch_negotiator::SkippingNegotiator;
use grit_lib::objects::{parse_tag, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{resolve_revision, try_peel_to_commit_for_merge_base};
use grit_lib::unpack_objects::{unpack_objects, UnpackOptions};

use crate::commands::index_pack;
use crate::file_upload_pack_v2::{
    cap_lines_for_bundle_request, drain_bundle_uri_response, read_pkt_lines_until_flush,
    read_v2_capability_block, server_advertises_bundle_uri, skip_v2_section_until_boundary,
    transfer_bundle_uri_enabled, v2_fetch_supports_filter, v2_fetch_supports_ref_in_want,
    v2_fetch_supports_sideband_all, write_bundle_uri_command, write_v2_fetch_request,
};
use crate::grit_exe::{grit_executable, strip_trace2_env};
use crate::protocol_wire;
use crate::trace2_transfer;
use crate::wire_trace;
use grit_lib::pkt_line;

/// Shallow/deepen options forwarded to local `upload-pack` negotiation.
#[derive(Debug, Clone, Default)]
pub struct UploadPackShallowOptions {
    /// Absolute depth (`--depth`).
    pub depth: Option<usize>,
    /// Relative deepen amount (`--deepen`).
    pub deepen: Option<usize>,
    /// Date cutoff for deepening (`--shallow-since`).
    pub shallow_since: Option<String>,
    /// Excluded refs for deepening (`--shallow-exclude`).
    pub shallow_exclude: Vec<String>,
    /// Convert a shallow clone into a complete clone relative to the remote.
    pub unshallow: bool,
}

thread_local! {
    static PACKET_TRACE_IDENTITY: Cell<&'static str> = const { Cell::new("fetch") };
}

/// Peel a ref tip to the commit used for fetch negotiation, returning `None` when the tip peels
/// to a non-commit object (e.g. a tag pointing at a tree or blob, like t5515's `tag-one-tree`).
///
/// Git's negotiation (`mark_complete`, `everything_local`) silently ignores refs that do not
/// resolve to commits rather than treating them as fatal, so we mirror that by skipping them.
/// The object-format the client advertises during fetch: the local repository's
/// `extensions.objectformat` when set (so SHA-256 repos negotiate correctly),
/// otherwise `GIT_DEFAULT_HASH`, otherwise `sha1`.
fn client_fetch_object_format() -> String {
    if let Ok(repo) = Repository::discover(None) {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        if let Some(fmt) = cfg
            .get("extensions.objectformat")
            .or_else(|| cfg.get("extensions.objectFormat"))
        {
            let f = fmt.trim().to_ascii_lowercase();
            if !f.is_empty() {
                return f;
            }
        }
    }
    std::env::var("GIT_DEFAULT_HASH").unwrap_or_else(|_| "sha1".to_owned())
}

fn peel_commit_oid_for_negotiation(repo: &Repository, oid: ObjectId) -> Result<Option<ObjectId>> {
    try_peel_to_commit_for_merge_base(repo, oid).map_err(|e| match e {
        grit_lib::error::Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        other => other.into(),
    })
}

/// Collect the local tips usable as `have` lines for a v2 fetch negotiation: refs under
/// `refs/bundles/` (applied by `--bundle-uri`), `refs/heads/`, `refs/tags/`, and `HEAD`. Each tip
/// is peeled to a commit and kept only if its object is present locally; `wants` are excluded.
///
/// When `negotiation_tip_oids` is `Some`, the haves are restricted to those tips (matching the
/// `--negotiation-tip` semantics applied by the v1 negotiation path) so that
/// `git fetch --negotiation-tip=...` limits the advertised `have` lines. Returns a deterministic,
/// deduplicated list.
fn local_negotiation_haves(
    local_git_dir: &Path,
    wants: &[ObjectId],
    negotiation_tip_oids: Option<&[ObjectId]>,
) -> Vec<ObjectId> {
    // The `noop` negotiator advertises no `have` lines at all (`git/negotiator/noop.c`), so the
    // client never offers any common base and the server streams a full pack (t5554).
    if fetch_negotiation_is_noop(local_git_dir) {
        return Vec::new();
    }
    let Ok(repo) = Repository::open(local_git_dir, None) else {
        return Vec::new();
    };
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    // `--negotiation-tip`: restrict the `have` set to the peeled tip commits the caller named.
    let tip_filter: Option<HashSet<ObjectId>> = negotiation_tip_oids.map(|tips| {
        tips.iter()
            .filter_map(|tip| peel_commit_oid_for_negotiation(&repo, *tip).ok().flatten())
            .collect()
    });

    let mut haves: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();

    let consider = |oid: ObjectId, haves: &mut Vec<ObjectId>, seen: &mut HashSet<ObjectId>| {
        if repo.odb.read(&oid).is_err() {
            return;
        }
        let Ok(Some(peeled)) = peel_commit_oid_for_negotiation(&repo, oid) else {
            return;
        };
        if want_set.contains(&peeled) {
            return;
        }
        if tip_filter
            .as_ref()
            .is_some_and(|filter| !filter.contains(&peeled))
        {
            return;
        }
        if seen.insert(peeled) {
            haves.push(peeled);
        }
    };

    for prefix in ["refs/bundles/", "refs/heads/", "refs/tags/"] {
        if let Ok(entries) = refs::list_refs(local_git_dir, prefix) {
            for (name, oid) in entries {
                let tip = resolve_revision(&repo, &name).unwrap_or(oid);
                consider(tip, &mut haves, &mut seen);
            }
        }
    }
    if let Ok(h) = refs::resolve_ref(local_git_dir, "HEAD") {
        consider(h, &mut haves, &mut seen);
    }

    // Expand the ref tips into a full commit walk in committer-date order (newest first), matching
    // `fetch-pack.c`'s negotiator. The negotiation loop sends these in batches (`INITIAL_FLUSH`
    // first), so a real multi-round have/ACK exchange happens when the server cannot satisfy the
    // wants from the first batch (t5703 `total_rounds=2`). Without this, only the ref tips were
    // offered and a single round always sufficed.
    date_ordered_have_walk(&repo, haves, &want_set)
}

/// Walk `tips`' ancestry in committer-date order (newest first), returning commit OIDs to offer as
/// `have` lines. Stops descending into the wanted commits themselves. Bounded to avoid unbounded
/// work on pathological histories (`MAX_HAVES`), like Git's `MAX_IN_VAIN` budget.
fn date_ordered_have_walk(
    repo: &Repository,
    tips: Vec<ObjectId>,
    want_set: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    use std::collections::BinaryHeap;

    const MAX_HAVES: usize = 1024;

    // Max-heap on committer time, so we always pop the newest commit next.
    let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
    let mut queued: HashSet<ObjectId> = HashSet::new();
    let mut out: Vec<ObjectId> = Vec::new();

    let commit_time = |repo: &Repository, oid: &ObjectId| -> Option<(i64, Vec<ObjectId>)> {
        let obj = repo.odb.read(oid).ok()?;
        if obj.kind != ObjectKind::Commit {
            return None;
        }
        let c = grit_lib::objects::parse_commit(&obj.data).ok()?;
        let t = grit_lib::ident::committer_timestamp_for_until_filter(&c.committer);
        Some((t, c.parents))
    };

    for tip in tips {
        if queued.insert(tip) {
            if let Some((t, _)) = commit_time(repo, &tip) {
                heap.push((t, tip));
            }
        }
    }

    while let Some((_, oid)) = heap.pop() {
        if out.len() >= MAX_HAVES {
            break;
        }
        if !want_set.contains(&oid) {
            out.push(oid);
        }
        if let Some((_, parents)) = commit_time(repo, &oid) {
            for p in parents {
                if queued.insert(p) {
                    if let Some((t, _)) = commit_time(repo, &p) {
                        heap.push((t, p));
                    }
                }
            }
        }
    }

    out
}

/// Split a simple upload-pack command string into leading `VAR=value` tokens (shell-style, no
/// quotes) and the remainder. Used when rewriting `… git-upload-pack` to `grit upload-pack` so
/// tests like `GIT_TEST_ASSUME_DIFFERENT_OWNER=true git-upload-pack` keep their environment
/// (`t0411-clone-from-partial`, `t5605-clone-dirname`).
pub(crate) fn parse_leading_shell_env_assignments(template: &str) -> (Vec<(String, String)>, &str) {
    let mut env_pairs = Vec::new();
    let mut rest = template.trim();
    while !rest.is_empty() {
        let Some(token) = rest.split_whitespace().next() else {
            break;
        };
        let Some((key, val)) = token.split_once('=') else {
            break;
        };
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            break;
        }
        let val = val
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(val);
        env_pairs.push((key.to_string(), val.to_string()));
        rest = rest[token.len()..].trim_start();
    }
    (env_pairs, rest)
}

/// Run `f` with `GIT_TRACE_PACKET` lines labeled as `identity` (`fetch`, `clone`, …).
pub fn with_packet_trace_identity<T>(
    identity: &'static str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    struct Reset(&'static str);
    impl Drop for Reset {
        fn drop(&mut self) {
            PACKET_TRACE_IDENTITY.set(self.0);
        }
    }
    let prev = PACKET_TRACE_IDENTITY.get();
    PACKET_TRACE_IDENTITY.set(identity);
    let _guard = Reset(prev);
    crate::trace_packet::with_packet_trace_label(identity, f)
}

const INITIAL_FLUSH: usize = 16;
const PIPESAFE_FLUSH: usize = 32;
fn next_flush_count(stateless_rpc: bool, count: usize) -> usize {
    if stateless_rpc {
        const LARGE_FLUSH: usize = 16384;
        if count < LARGE_FLUSH {
            count * 2
        } else {
            count * 11 / 10
        }
    } else if count < PIPESAFE_FLUSH {
        count * 2
    } else {
        count + PIPESAFE_FLUSH
    }
}

fn trace_packet_fetch(direction: char, payload: &str) {
    let identity = PACKET_TRACE_IDENTITY.get();
    if identity == "clone" && direction == '>' && payload.starts_with("want ") {
        return;
    }
    wire_trace::trace_packet_line_ident(identity, direction, payload);
}

/// Protocol v2 ends the initial advertisement at a flush with no ref lines. Run `ls-refs` to
/// obtain the same ref list v0 would have advertised (heads, tags, `HEAD`), matching Git's
/// `fetch-pack` and fixing fetches that would otherwise see an empty ref map (e.g. t5525).
fn v2_ls_refs_for_fetch(
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    include_head_ref_prefix: bool,
    refspecs: &[String],
    server_options: &[String],
) -> Result<(Vec<(String, ObjectId)>, Option<String>)> {
    let default_hash = client_fetch_object_format();
    let agent = format!("agent=git/{}-", crate::version_string());

    trace_packet_fetch('>', "command=ls-refs");
    pkt_line::write_line(stdin, "command=ls-refs")?;
    trace_packet_fetch('>', agent.trim_end());
    pkt_line::write_line(stdin, &agent)?;
    let of = format!("object-format={default_hash}");
    trace_packet_fetch('>', &of);
    pkt_line::write_line(stdin, &of)?;
    for opt in server_options {
        let line = format!("server-option={opt}");
        trace_packet_fetch('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    pkt_line::write_delim(stdin)?;
    trace_packet_fetch('>', "0001");
    trace_packet_fetch('>', "symrefs");
    pkt_line::write_line(stdin, "symrefs")?;
    trace_packet_fetch('>', "peel");
    pkt_line::write_line(stdin, "peel")?;
    if include_head_ref_prefix {
        trace_packet_fetch('>', "ref-prefix HEAD");
        pkt_line::write_line(stdin, "ref-prefix HEAD")?;
    }
    let derived_prefixes = v2_ref_prefixes_from_refspecs(refspecs);
    if refspecs.is_empty() || derived_prefixes.is_empty() {
        trace_packet_fetch('>', "ref-prefix refs/heads/");
        pkt_line::write_line(stdin, "ref-prefix refs/heads/")?;
        trace_packet_fetch('>', "ref-prefix refs/tags/");
        pkt_line::write_line(stdin, "ref-prefix refs/tags/")?;
    } else {
        for prefix in derived_prefixes {
            let line = format!("ref-prefix {prefix}");
            trace_packet_fetch('>', &line);
            pkt_line::write_line(stdin, &line)?;
        }
    }
    pkt_line::write_flush(stdin)?;
    trace_packet_fetch('>', "0000");
    stdin.flush().context("flush ls-refs request")?;

    let mut buf = Vec::new();
    read_pkt_lines_until_flush(stdout, &mut buf, 512 * 1024).context("read ls-refs response")?;

    let mut cursor = std::io::Cursor::new(&buf);
    let mut advertised: Vec<(String, ObjectId)> = Vec::new();
    let mut head_symref: Option<String> = None;

    loop {
        let pkt = match pkt_line::read_packet(&mut cursor)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => line,
            Some(other) => bail!("unexpected ls-refs packet in fetch: {other:?}"),
        };
        let (name, oid, _peeled, symref_target) =
            crate::commands::ls_remote::parse_ls_refs_v2_line(&pkt)?;
        if name.contains("^{") {
            continue;
        }
        if name == "HEAD" {
            if let Some(t) =
                symref_target.filter(|t| grit_lib::refs::is_valid_fetch_advertised_ref(t))
            {
                head_symref = Some(t);
            }
            advertised.push((name, oid));
        } else if grit_lib::refs::is_valid_fetch_advertised_ref(&name) {
            advertised.push((name, oid));
        }
    }

    Ok((advertised, head_symref))
}

fn v2_ref_prefixes_from_refspecs(refspecs: &[String]) -> Vec<String> {
    let mut out = Vec::<String>::new();
    for spec in refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let raw = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let src = raw.split_once(':').map(|(s, _)| s).unwrap_or(raw).trim();
        if src.is_empty() {
            continue;
        }
        if src == "HEAD" {
            push_unique_string(&mut out, "HEAD");
            continue;
        }
        if let Some(star) = src.find('*') {
            let prefix = &src[..star];
            if prefix.is_empty() {
                continue;
            }
            if prefix.starts_with("refs/") {
                push_unique_string(&mut out, prefix);
            } else {
                push_unique_string(&mut out, &format!("refs/heads/{prefix}"));
            }
            continue;
        }
        if src.starts_with("refs/") {
            push_unique_string(&mut out, src);
        } else {
            // Match Git fetch-pack traces for unqualified names: request both the raw token and
            // the heads namespace (`dwim` + `refs/heads/dwim` in t5702.48).
            push_unique_string(&mut out, src);
            push_unique_string(&mut out, &format!("refs/heads/{src}"));
        }
    }
    // Fetch can still need tag refs for tag-following behavior even when branch-specific
    // refspecs are used (e.g. `refs/heads/*:refs/remotes/<name>/*` in shallow/update-shallow
    // scenarios). Keep the tags namespace available unless the caller explicitly disables tag
    // updates later in the fetch pipeline.
    push_unique_string(&mut out, "refs/tags/");
    out
}

fn refspecs_are_explicit_oid_sources(refspecs: &[String]) -> bool {
    if refspecs.is_empty() {
        return false;
    }
    refspecs.iter().all(|spec| {
        let raw = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let src = raw.split_once(':').map(|(s, _)| s).unwrap_or(raw).trim();
        !src.is_empty() && src.len() == 40 && src.chars().all(|c| c.is_ascii_hexdigit())
    })
}

fn push_unique_string(out: &mut Vec<String>, value: &str) {
    if !out.iter().any(|e| e == value) {
        out.push(value.to_owned());
    }
}

fn extract_server_sid_from_caps(caps: &str) -> Option<&str> {
    for cap in caps.split_whitespace() {
        if let Some(rest) = cap.strip_prefix("session-id=") {
            return Some(rest);
        }
    }
    None
}

fn parse_ref_advertisement_line(line: &str) -> Option<(ObjectId, String, &str)> {
    let line = line.trim_end_matches('\n');
    if line.len() < 40 {
        return None;
    }
    let hex = &line[..40];
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let oid = ObjectId::from_hex(hex).ok()?;
    let mut rest = line[40..].trim_start();
    // Upstream `git-daemon` uses a single space after the OID; `upload-pack` often uses `\t`.
    rest = rest.trim_start_matches([' ', '\t']);
    let (refname, caps) = if let Some(i) = rest.find('\0') {
        (rest[..i].trim(), &rest[i + 1..])
    } else {
        (rest.trim(), "")
    };
    if refname.is_empty() {
        return None;
    }
    Some((oid, refname.to_string(), caps))
}

pub(crate) fn read_advertisement(
    child_stdout: &mut impl Read,
) -> Result<(
    Vec<(String, ObjectId)>,
    Option<String>,
    bool,
    bool,
    Option<String>,
)> {
    let mut out = Vec::new();
    let mut head_symref: Option<String> = None;
    let mut saw_version_1_line = false;
    let mut saw_version_2_capability = false;
    let mut server_sid: Option<String> = None;
    loop {
        match pkt_line::read_packet(child_stdout)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if let Some(ver) = line.strip_prefix("version ") {
                    if let Ok(n) = ver.trim().parse::<u8>() {
                        trace_packet_fetch('<', line);
                        if n == 1 {
                            saw_version_1_line = true;
                        }
                        if n == 2 {
                            saw_version_2_capability = true;
                        }
                        continue;
                    }
                }
                trace_packet_fetch('<', line);
                let Some((oid, refname, caps)) = parse_ref_advertisement_line(line) else {
                    if server_sid.is_none() {
                        if let Some(rest) = line.strip_prefix("session-id=") {
                            server_sid = Some(rest.trim().to_owned());
                        }
                    }
                    continue;
                };
                if server_sid.is_none() {
                    if let Some(sid) = extract_server_sid_from_caps(caps) {
                        server_sid = Some(sid.to_owned());
                    }
                }
                if refname == "HEAD" {
                    for cap in caps.split_whitespace() {
                        if let Some(target) = cap.strip_prefix("symref=HEAD:") {
                            head_symref = Some(target.to_string());
                        }
                    }
                }
                // `0{40} capabilities^{}` is the no-refs capability carrier (an empty repo or empty
                // namespace), not an advertised ref — never record it (t5509 empty namespace).
                if refname == "capabilities^{}" {
                    continue;
                }
                // A protocol v0/v1 ref advertisement emits a `<peeled-oid> <refname>^{}` line after
                // each annotated tag (the peeled object). These `^{}` lines are not refs and must
                // never be recorded as fetchable/trackable refs — otherwise a v0 fetch would write
                // bogus `refs/tags/<name>^{}` refs and FETCH_HEAD lines (t5515 with
                // GIT_TEST_PROTOCOL_VERSION=0). The real ref already preceded this peeled line.
                if refname.ends_with("^{}") {
                    continue;
                }
                if grit_lib::refs::is_valid_fetch_advertised_ref(&refname) {
                    out.push((refname, oid));
                }
            }
            _ => {}
        }
    }
    Ok((
        out,
        head_symref,
        saw_version_1_line,
        saw_version_2_capability,
        server_sid,
    ))
}

/// When the child speaks protocol v2, [`read_advertisement`] only skips capability lines and never
/// records ref advertisements. Merge `refs/heads/*` and `refs/tags/*` from the on-disk remote so
/// [`collect_wants_for_upload_pack`] can request missing objects and the fetch command can update
/// remote-tracking refs (t5506-remote-groups).
fn merge_remote_refs_into_upload_pack_advertisement(
    remote_repo_path: &Path,
    advertised: &mut Vec<(String, ObjectId)>,
) -> Result<()> {
    // `remote_repo_path` is the repository root (bare) or work tree (non-bare); `list_refs` needs
    // the git directory. `Repository::open` expects `.git` for normal repos.
    let remote_git: PathBuf = {
        let dot_git = remote_repo_path.join(".git");
        if dot_git.is_dir() {
            dot_git
        } else {
            remote_repo_path.to_path_buf()
        }
    };
    if advertised.iter().any(|(n, _)| n.starts_with("refs/heads/")) {
        return Ok(());
    }
    let mut by_name: HashMap<String, ObjectId> =
        advertised.iter().map(|(n, o)| (n.clone(), *o)).collect();
    for (n, o) in refs::list_refs(&remote_git, "refs/heads/")? {
        by_name.insert(n, o);
    }
    for (n, o) in refs::list_refs(&remote_git, "refs/tags/")? {
        by_name.insert(n, o);
    }
    *advertised = by_name.into_iter().collect();
    advertised.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(())
}

pub(crate) fn collect_wants(
    advertised: &[(String, ObjectId)],
    refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    fn refspec_src(spec: &str) -> &str {
        let spec_clean = spec.strip_prefix('+').unwrap_or(spec);
        spec_clean
            .split_once(':')
            .map(|(a, _)| a)
            .unwrap_or(spec_clean)
    }

    fn refspec_pattern_matches(pattern: &str, refname: &str) -> bool {
        let Some(star) = pattern.find('*') else {
            return pattern == refname;
        };
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        refname.len() >= prefix.len() + suffix.len()
            && refname.starts_with(prefix)
            && refname.ends_with(suffix)
    }

    fn resolve_advertised_source_ref(
        src: &str,
        advertised: &[(String, ObjectId)],
    ) -> Option<String> {
        if src.is_empty() || src == "HEAD" {
            return Some("HEAD".to_owned());
        }
        if src.starts_with("refs/") {
            return Some(src.to_owned());
        }
        let candidates = [
            format!("refs/{src}"),
            format!("refs/tags/{src}"),
            format!("refs/heads/{src}"),
            format!("refs/remotes/{src}"),
            format!("refs/remotes/{src}/HEAD"),
        ];
        for cand in candidates {
            if advertised.iter().any(|(name, _)| name == &cand) {
                return Some(cand);
            }
        }
        Some(format!("refs/heads/{src}"))
    }

    if refspecs.is_empty() {
        let mut wants = Vec::new();
        for (name, oid) in advertised {
            if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
                wants.push(*oid);
            }
        }
        if wants.is_empty() {
            if let Some((_, oid)) = advertised.iter().find(|(n, _)| n == "HEAD") {
                wants.push(*oid);
            }
        }
        if wants.is_empty() {
            for (name, oid) in advertised {
                if name == "HEAD" {
                    continue;
                }
                if name.starts_with("refs/") {
                    wants.push(*oid);
                }
            }
        }
        wants.retain(|o| *o != zero_oid());
        wants.sort_by_key(|o| o.to_hex());
        wants.dedup();
        return Ok(wants);
    }

    let negative_patterns: Vec<String> = refspecs
        .iter()
        .filter_map(|spec| spec.strip_prefix('^'))
        .map(refspec_src)
        .filter(|src| !src.is_empty())
        .map(|src| {
            resolve_advertised_source_ref(src, advertised)
                .unwrap_or_else(|| format!("refs/heads/{src}"))
        })
        .collect();

    let is_excluded = |refname: &str| -> bool {
        negative_patterns
            .iter()
            .any(|pat| refspec_pattern_matches(pat, refname))
    };

    let mut wants = Vec::new();
    for spec in refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let src = refspec_src(spec);
        if src.contains('*') {
            let pattern = resolve_advertised_source_ref(src, advertised)
                .unwrap_or_else(|| format!("refs/heads/{src}"));
            let mut matched_any = false;
            for (name, oid) in advertised {
                if refspec_pattern_matches(&pattern, name) && !is_excluded(name) {
                    wants.push(*oid);
                    matched_any = true;
                }
            }
            if !matched_any {
                bail!("could not find any remote ref matching glob '{src}'");
            }
            continue;
        }
        if src.eq_ignore_ascii_case("HEAD") {
            let oid = advertised
                .iter()
                .find(|(n, _)| n == "HEAD")
                .map(|(_, o)| *o)
                .with_context(|| "could not find remote ref 'HEAD' in advertisement")?;
            if is_excluded("HEAD") {
                continue;
            }
            wants.push(oid);
            continue;
        }
        if src.len() == 40 && src.chars().all(|c| c.is_ascii_hexdigit()) {
            let oid: ObjectId = src
                .parse()
                .with_context(|| format!("invalid object id '{src}' in refspec"))?;
            wants.push(oid);
            continue;
        }
        let oid = if src.is_empty() || src == "HEAD" {
            let resolved = advertised
                .iter()
                .find(|(n, _)| n == "HEAD")
                .map(|(_, o)| *o)
                .or_else(|| {
                    advertised.iter().find_map(|(n, o)| {
                        n.strip_prefix("refs/heads/").and_then(|short| {
                            if short == "main" || short == "master" {
                                Some(*o)
                            } else {
                                None
                            }
                        })
                    })
                })
                .with_context(|| "could not find remote ref 'HEAD'")?;
            if is_excluded("HEAD") {
                continue;
            }
            resolved
        } else {
            let remote_ref = resolve_advertised_source_ref(src, advertised)
                .unwrap_or_else(|| format!("refs/heads/{src}"));
            let resolved = advertised
                .iter()
                .find(|(n, _)| n == &remote_ref)
                .map(|(_, o)| *o)
                .or_else(|| {
                    let tag_ref = format!("refs/tags/{src}");
                    advertised
                        .iter()
                        .find(|(n, _)| n == &tag_ref)
                        .map(|(_, o)| *o)
                })
                .with_context(|| format!("could not find remote ref '{remote_ref}'"))?;
            if is_excluded(&remote_ref) {
                continue;
            }
            resolved
        };
        wants.push(oid);
    }
    wants.retain(|o| *o != zero_oid());
    wants.sort_by_key(|o| o.to_hex());
    wants.dedup();
    Ok(wants)
}

/// Pushes `oid` onto `wants` if it is not already present (order-preserving).
pub(crate) fn push_want_unique(wants: &mut Vec<ObjectId>, oid: ObjectId) {
    if !wants.contains(&oid) {
        wants.push(oid);
    }
}

/// Resolve CLI refspec sources to `want` OIDs for upload-pack (matches [`collect_wants`]).
pub(crate) fn collect_wants_cli(
    remote_git_dir: &Path,
    advertised: &[(String, ObjectId)],
    cli_refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    fn refspec_src(spec: &str) -> &str {
        let spec_clean = spec.strip_prefix('+').unwrap_or(spec);
        spec_clean
            .split_once(':')
            .map(|(a, _)| a)
            .unwrap_or(spec_clean)
    }

    fn refspec_pattern_matches(pattern: &str, refname: &str) -> bool {
        let Some(star) = pattern.find('*') else {
            return pattern == refname;
        };
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        refname.len() >= prefix.len() + suffix.len()
            && refname.starts_with(prefix)
            && refname.ends_with(suffix)
    }

    fn resolve_remote_ref_for_cli_src(remote_git_dir: &Path, src: &str) -> Option<String> {
        if src.is_empty() || src == "HEAD" {
            return Some("HEAD".to_owned());
        }
        if src.starts_with("refs/") {
            return Some(src.to_owned());
        }
        let candidates = [
            format!("refs/{src}"),
            format!("refs/tags/{src}"),
            format!("refs/heads/{src}"),
            format!("refs/remotes/{src}"),
            format!("refs/remotes/{src}/HEAD"),
        ];
        for cand in candidates {
            if refs::resolve_ref(remote_git_dir, &cand).is_ok() {
                return Some(cand);
            }
        }
        Some(format!("refs/heads/{src}"))
    }

    let mut by_name = std::collections::BTreeMap::<String, ObjectId>::new();
    for (n, o) in advertised {
        by_name.insert(n.clone(), *o);
    }
    if let Ok(all_refs) = refs::list_refs(remote_git_dir, "refs/") {
        for (n, o) in all_refs {
            by_name.insert(n, o);
        }
    }
    if let Ok(head_oid) = refs::resolve_ref(remote_git_dir, "HEAD") {
        by_name.insert("HEAD".to_owned(), head_oid);
    }
    let all_refs: Vec<(String, ObjectId)> = by_name.into_iter().collect();

    let negative_patterns: Vec<String> = cli_refspecs
        .iter()
        .filter_map(|spec| spec.strip_prefix('^'))
        .map(refspec_src)
        .filter(|src| !src.is_empty())
        .map(|src| {
            resolve_remote_ref_for_cli_src(remote_git_dir, src)
                .unwrap_or_else(|| format!("refs/heads/{src}"))
        })
        .collect();
    let is_excluded = |refname: &str| -> bool {
        negative_patterns
            .iter()
            .any(|pat| refspec_pattern_matches(pat, refname))
    };

    let mut wants = Vec::new();
    for spec in cli_refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let src = refspec_src(spec);
        if src.contains('*') {
            let pattern = resolve_remote_ref_for_cli_src(remote_git_dir, src)
                .unwrap_or_else(|| format!("refs/heads/{src}"));
            for (name, oid) in &all_refs {
                if refspec_pattern_matches(&pattern, name) && !is_excluded(name) {
                    push_want_unique(&mut wants, *oid);
                }
            }
            continue;
        }
        let oid = if src.len() == 40 && src.chars().all(|c| c.is_ascii_hexdigit()) {
            ObjectId::from_hex(src)
                .with_context(|| format!("invalid object id in refspec: {src}"))?
        } else {
            let resolved_ref = resolve_remote_ref_for_cli_src(remote_git_dir, src)
                .unwrap_or_else(|| format!("refs/heads/{src}"));
            if is_excluded(&resolved_ref) {
                continue;
            }
            all_refs
                .iter()
                .find(|(n, _)| n == &resolved_ref)
                .map(|(_, o)| *o)
                .with_context(|| format!("could not find remote ref '{resolved_ref}'"))?
        };
        push_want_unique(&mut wants, oid);
    }
    wants.retain(|o| *o != zero_oid());
    wants.sort_by_key(|o| o.to_hex());
    wants.dedup();
    Ok(wants)
}

/// Classify CLI refspec sources for a `ref-in-want`-capable v2 fetch.
///
/// Returns `(want_refs, exact_oids)`:
/// - `want_refs`: full ref names to request as `want-ref <name>` — every named source (incl.
///   wildcard expansions) that resolves to an advertised ref. Mirrors `fetch-pack.c` `add_wants`,
///   which sends `want-ref` for any want that is not an `exact_oid`.
/// - `exact_oids`: the OIDs of `<oid>:<dst>` sources, which always go out as plain `want <oid>`.
///
/// `advertised` is the (name, oid) ref map the server advertised this session.
fn cli_want_refs_and_oids(
    advertised: &[(String, ObjectId)],
    cli_refspecs: &[String],
) -> (Vec<String>, Vec<ObjectId>) {
    fn refspec_src(spec: &str) -> &str {
        let spec_clean = spec.strip_prefix('+').unwrap_or(spec);
        spec_clean
            .split_once(':')
            .map(|(a, _)| a)
            .unwrap_or(spec_clean)
    }
    fn pattern_matches(pattern: &str, refname: &str) -> bool {
        match pattern.find('*') {
            None => pattern == refname,
            Some(star) => {
                let prefix = &pattern[..star];
                let suffix = &pattern[star + 1..];
                refname.len() >= prefix.len() + suffix.len()
                    && refname.starts_with(prefix)
                    && refname.ends_with(suffix)
            }
        }
    }
    // Candidate full ref names for a bare source, in Git's DWIM order.
    fn candidates(src: &str) -> Vec<String> {
        if src.is_empty() || src == "HEAD" {
            return vec!["HEAD".to_owned()];
        }
        if src.starts_with("refs/") {
            return vec![src.to_owned()];
        }
        vec![
            format!("refs/{src}"),
            format!("refs/tags/{src}"),
            format!("refs/heads/{src}"),
            format!("refs/remotes/{src}"),
            format!("refs/remotes/{src}/HEAD"),
        ]
    }

    let mut want_refs: Vec<String> = Vec::new();
    let mut exact_oids: Vec<ObjectId> = Vec::new();
    for spec in cli_refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let src = refspec_src(spec);
        if src.is_empty() {
            continue;
        }
        // Exact OID source -> plain want.
        if src.len() == 40 && src.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(oid) = ObjectId::from_hex(src) {
                exact_oids.push(oid);
            }
            continue;
        }
        if src.contains('*') {
            // Expand the wildcard against the advertised refs; each match becomes a want-ref.
            let prefix = src.starts_with("refs/");
            for (name, _) in advertised {
                let pat = if prefix {
                    src.to_owned()
                } else {
                    format!("refs/heads/{src}")
                };
                if pattern_matches(&pat, name) && !want_refs.iter().any(|w| w == name) {
                    want_refs.push(name.clone());
                }
            }
            continue;
        }
        // Bare named source: resolve to the first advertised candidate.
        let resolved = candidates(src)
            .into_iter()
            .find(|cand| advertised.iter().any(|(n, _)| n == cand));
        if let Some(name) = resolved {
            if !want_refs.iter().any(|w| w == &name) {
                want_refs.push(name);
            }
        }
    }
    (want_refs, exact_oids)
}

/// Tests invoke `git-upload-pack`; use grit to serve grit-created object stores.
///
/// `client_proto` is passed to [`protocol_wire::merge_git_protocol_env_for_child`] (use `0` when
/// the reader expects a v0 ref advertisement, e.g. `ext::` transport).
pub(crate) fn spawn_upload_pack_with_proto(
    cmd_template: Option<&str>,
    repo_path: &Path,
    client_proto: u8,
) -> Result<std::process::Child> {
    let repo_path = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let rp = repo_path.to_string_lossy();
    let rp_escaped = rp.replace('\'', "'\"'\"'");

    let apply_proto_env = |c: &mut Command| {
        if client_proto == 0 {
            c.env_remove("GIT_PROTOCOL");
        } else {
            protocol_wire::merge_git_protocol_env_for_child(c, client_proto);
        }
    };

    let Some(cmd_template) = cmd_template else {
        let mut c = Command::new(grit_executable());
        strip_trace2_env(&mut c);
        c.arg("upload-pack")
            .arg(rp.as_ref())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_TRACE_PACKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        apply_proto_env(&mut c);
        return c
            .spawn()
            .with_context(|| format!("failed to spawn grit upload-pack for {}", rp));
    };

    let (leading_env, after_env) = parse_leading_shell_env_assignments(cmd_template);

    if after_env == "git-upload-pack" || after_env.ends_with("/git-upload-pack") {
        let mut c = Command::new(grit_executable());
        strip_trace2_env(&mut c);
        c.arg("upload-pack")
            .arg(rp.as_ref())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_TRACE_PACKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in leading_env {
            if key != "GIT_TEST_ASSUME_DIFFERENT_OWNER" {
                c.env(key, value);
            }
        }
        apply_proto_env(&mut c);
        return c
            .spawn()
            .with_context(|| format!("failed to spawn grit upload-pack for {}", rp));
    }

    let trimmed = cmd_template.trim();
    if trimmed == "grit-upload-pack" || trimmed.ends_with("/grit-upload-pack") {
        let mut c = Command::new(trimmed);
        strip_trace2_env(&mut c);
        c.arg(rp.as_ref())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_TRACE_PACKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        apply_proto_env(&mut c);
        return c
            .spawn()
            .with_context(|| format!("failed to spawn '{} {}'", trimmed, rp));
    }

    let full_cmd = cmd_template.replace('\'', "'\"'\"'");
    let script = format!("{full_cmd} '{rp_escaped}'");
    let mut c = Command::new("sh");
    strip_trace2_env(&mut c);
    c.arg("-c")
        .arg(&script)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_TRACE_PACKET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    apply_proto_env(&mut c);
    c.spawn()
        .with_context(|| format!("failed to spawn upload-pack: {script}"))
}

/// Spawn `upload-pack` for local pipe negotiation ([`fetch_via_upload_pack_skipping`], etc.).
///
/// Always uses **protocol v0** ref advertisement for the child (`GIT_PROTOCOL` cleared), even when
/// the user's `protocol.version` is 2. The local fetch client reads the v0 pkt-line ref list via
/// [`read_advertisement`]; a v2 server would emit `version 2` capability lines first and no ref
/// rows, which breaks refspec resolution (e.g. `t5501-fetch-push-alternates`).
pub(crate) fn spawn_upload_pack(
    cmd_template: Option<&str>,
    repo_path: &Path,
) -> Result<std::process::Child> {
    // Local fetch/clone uses protocol v0 pkt-line negotiation (`want`/`have`/`done`). Always
    // spawn the server side without forcing `GIT_PROTOCOL=version=2`, even when the client
    // defaults to `protocol.version=2` for HTTP/file v2 — otherwise `upload-pack` enters the v2
    // path and rejects v0 `want` lines as "unknown capability" (t0411 lazy-fetch re-enable).
    // Force protocol 0 on the wire so the ref advertisement matches [`read_advertisement`] (t5501).
    spawn_upload_pack_with_proto(cmd_template, repo_path, 0)
}

pub(crate) fn drain_child_stdout_to_eof(r: &mut impl Read) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn read_ack_round_with_negotiator(
    stdout: &mut impl Read,
    negotiator: &mut SkippingNegotiator,
) -> Result<()> {
    loop {
        let Some(pkt) = pkt_line::read_packet(stdout)? else {
            break;
        };
        match pkt {
            pkt_line::Packet::Flush => break,
            pkt_line::Packet::Data(ln) => {
                trace_packet_fetch('<', ln.trim_end());
                if ln.trim_end() == "NAK" {
                    // `upload-pack` sends `NAK` as the last pkt-line of a negotiation round but does
                    // not follow it with a flush; waiting for another packet would block forever while
                    // the server waits for our next `have` batch or `done`.
                    break;
                }
                let Some((ack_oid, kind)) = parse_ack(&ln) else {
                    break;
                };
                // Match `fetch-pack.c` `get_ack` + negotiation loop: only a bare `ACK <oid>`
                // ends the round without updating the negotiator; `common`, `continue`, and
                // `ready` all call `negotiator->ack` (see cases `ACK_common`, `ACK_continue`,
                // `ACK_ready`).
                if kind == AckKind::Bare {
                    break;
                }
                let _ = negotiator.ack(ack_oid)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AckKind {
    /// `ACK <oid>` with no status suffix (post-`done` or legacy).
    Bare,
    Common,
    Continue,
    Ready,
}

fn parse_ack(line: &str) -> Option<(ObjectId, AckKind)> {
    if line == "NAK" {
        return None;
    }
    let rest = line.strip_prefix("ACK ")?;
    let hex = rest.split_whitespace().next()?;
    let oid = ObjectId::from_hex(hex).ok()?;
    let tail = rest.strip_prefix(hex).unwrap_or("").trim();
    let kind = if tail.contains("continue") {
        AckKind::Continue
    } else if tail.contains("common") {
        AckKind::Common
    } else if tail.contains("ready") {
        AckKind::Ready
    } else {
        AckKind::Bare
    };
    Some((oid, kind))
}

pub(crate) fn read_pkt_payload_raw(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    match len {
        // Flush / delim / response-end — not a data payload; side-band readers stop at flush.
        0 | 1 | 2 => Ok(None),
        n if n <= 4 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid pkt-line length: {n}"),
        )),
        n => {
            let payload_len = n - 4;
            let mut buf = vec![0u8; payload_len];
            r.read_exact(&mut buf)?;
            Ok(Some(buf))
        }
    }
}

/// Whether an I/O error chain is a broken pipe (the upload-pack child closed its stdin after
/// rejecting our request). Used to convert a mid-negotiation EPIPE into a clean fatal error.
fn is_broken_pipe_io(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// After a broken-pipe write to upload-pack, read whatever it sent on stdout for an `ERR` packet
/// (e.g. `ERR upload-pack: not our ref <oid>`) and surface it as a fatal error. Falls back to a
/// generic message when no ERR packet is found. Always returns `Err`.
fn fail_with_upload_pack_err(stdout: &mut impl Read) -> anyhow::Error {
    loop {
        match pkt_line::read_packet(stdout) {
            Ok(Some(pkt_line::Packet::Data(ln))) => {
                let line = ln.trim_end();
                trace_packet_fetch('<', line);
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return anyhow::anyhow!("fatal: remote error: {}", msg.trim_end());
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    anyhow::anyhow!("fatal: the remote end hung up unexpectedly")
}

fn read_sideband_pack_until_done(r: &mut impl Read, out: &mut Vec<u8>) -> Result<()> {
    let mut seen_pack = false;
    // Progress and pack data share side-band channel 1; the `PACK` magic may start mid-chunk or
    // span chunk boundaries (65515-byte framing), so scan a small carry buffer until we find it.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let Some(payload) = read_pkt_payload_raw(r)? else {
            break;
        };
        // `read_pkt_payload_raw` returns `None` on flush/EOF; empty payloads should not occur.
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => {
                let data = &payload[1..];
                if !seen_pack {
                    pending.extend_from_slice(data);
                    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
                        seen_pack = true;
                        out.extend_from_slice(&pending[pos..]);
                        pending.clear();
                    } else if pending.len() > 3 {
                        let keep_from = pending.len() - 3;
                        pending.drain(..keep_from);
                    }
                } else {
                    out.extend_from_slice(data);
                }
            }
            2 | 3 => {}
            _ => {
                if !seen_pack && payload.starts_with(b"PACK") {
                    seen_pack = true;
                    out.extend_from_slice(&payload);
                } else if seen_pack {
                    out.extend_from_slice(&payload);
                }
            }
        }
    }
    Ok(())
}

/// Read a protocol v2 `fetch` response: skip non-pack sections, demux side-band-64k pack data.
fn read_v2_fetch_pack_response(stdout: &mut impl Read, out: &mut Vec<u8>) -> Result<()> {
    loop {
        let hdr = match pkt_line::read_packet(stdout)? {
            Some(pkt_line::Packet::Data(s)) => s,
            Some(pkt_line::Packet::Flush) => return Ok(()),
            None => return Ok(()),
            Some(other) => bail!("unexpected v2 fetch response: {other:?}"),
        };
        trace_packet_fetch('<', hdr.trim_end());
        if let Some(msg) = hdr.strip_prefix("ERR ") {
            // Server rejected the request (e.g. `not our ref`). Surface as `fatal: remote error:`.
            bail!("fatal: remote error: {}", msg.trim_end());
        }
        match hdr.as_str() {
            "acknowledgments" | "wanted-refs" | "shallow-info" | "packfile-uris" => {
                skip_v2_section_until_boundary(stdout)?;
            }
            "packfile" => {
                read_sideband_pack_until_done(stdout, out)?;
                // For `git://` v2, servers can keep the connection open for additional commands.
                // `read_sideband_pack_until_done` already consumes the packfile section terminator
                // (flush/delim). Reading another pkt-line unconditionally here can block until the
                // socket read timeout and fail clone/fetch with EAGAIN.
                return Ok(());
            }
            other => bail!("unexpected v2 fetch section: {other}"),
        }
    }
}

/// Outcome of reading one protocol-v2 `acknowledgments` section during multi-round negotiation.
struct V2AckRound {
    /// Server emitted `ready`: the packfile (and any `wanted-refs`/`shallow-info`) follows in the
    /// SAME response after a delimiter — the caller must read the pack now without sending more.
    ready: bool,
    /// At least one `ACK <oid>` was seen (a common commit was found).
    seen_ack: bool,
}

/// Read a protocol-v2 `acknowledgments` section header and its ACK/NAK/ready lines.
///
/// Mirrors `fetch-pack.c` `process_ack`: a section terminated by a flush (no `ready`) means the
/// negotiation continues with another round; a delim after `ready` means the pack follows. Returns
/// `None` if the server instead started the packfile directly (e.g. it had nothing to ACK and
/// jumped to sending the pack), in which case the caller proceeds to read the pack.
fn read_v2_acknowledgments(stdout: &mut impl Read) -> Result<Option<V2AckRound>> {
    let hdr = match pkt_line::read_packet(stdout)? {
        Some(pkt_line::Packet::Data(s)) => s,
        Some(pkt_line::Packet::Flush) => {
            return Ok(Some(V2AckRound {
                ready: false,
                seen_ack: false,
            }))
        }
        None => return Ok(None),
        Some(other) => bail!("unexpected v2 fetch response: {other:?}"),
    };
    trace_packet_fetch('<', hdr.trim_end());
    if let Some(msg) = hdr.strip_prefix("ERR ") {
        bail!("fatal: remote error: {}", msg.trim_end());
    }
    if hdr != "acknowledgments" {
        // Not an acknowledgments section (server went straight to pack / shallow / wanted-refs).
        // Signal the caller to handle this header itself.
        return Ok(None);
    }
    let mut ready = false;
    let mut seen_ack = false;
    loop {
        match pkt_line::read_packet(stdout)? {
            Some(pkt_line::Packet::Data(ln)) => {
                let ln = ln.trim_end();
                trace_packet_fetch('<', ln);
                if ln == "NAK" {
                    continue;
                }
                if ln == "ready" {
                    ready = true;
                    continue;
                }
                if ln.starts_with("ACK ") {
                    seen_ack = true;
                    continue;
                }
                bail!("unexpected acknowledgment line: '{ln}'");
            }
            Some(pkt_line::Packet::Delim) | Some(pkt_line::Packet::Flush) | None => break,
            Some(other) => bail!("unexpected acknowledgments packet: {other:?}"),
        }
    }
    Ok(Some(V2AckRound { ready, seen_ack }))
}

/// Fetch via `upload-pack` using explicit object IDs (e.g. lazy promisor fetch).
///
/// Negotiates using the same skipping strategy as [`fetch_via_upload_pack_skipping`], but the
/// client `want` lines are exactly `wants` (typically a single OID not advertised as a ref).
/// Returns the raw `PACK` bytes (side-band demultiplexed).
pub fn fetch_upload_pack_explicit_wants(
    local_git_dir: &Path,
    remote_repo_path: &Path,
    upload_pack_cmd: Option<&str>,
    wants: &[ObjectId],
    filter_spec: Option<&str>,
) -> Result<Vec<u8>> {
    if wants.is_empty() {
        bail!("nothing to fetch (empty want list)");
    }
    fetch_upload_pack_negotiate_pack_bytes(
        local_git_dir,
        remote_repo_path,
        upload_pack_cmd,
        wants,
        filter_spec,
    )
}

/// Fetch via `upload-pack` using skipping negotiation; unpack pack into `local_git_dir`.
///
/// `compute_wants` builds the OID list sent as `want` lines (configured fetch, CLI refspecs, or
/// tag-following). When `has_cli_refspecs` is false, empty wants follow the same early-exit rules as
/// a fetch with no CLI refspecs.
///
/// Returns remote heads and tags from the ref advertisement, plus `HEAD` symref target
/// from capabilities when present (e.g. `symref=HEAD:refs/heads/main`).
pub fn fetch_via_upload_pack_skipping(
    local_git_dir: &Path,
    remote_repo_path: &Path,
    upload_pack_cmd: Option<&str>,
    compute_wants: impl FnOnce(&[(String, ObjectId)]) -> Result<Vec<ObjectId>>,
    has_cli_refspecs: bool,
    include_head_ref_prefix: bool,
    filter_active: bool,
    include_tag: bool,
    negotiation_tip_oids: Option<&[ObjectId]>,
    shallow_options: Option<&UploadPackShallowOptions>,
    filter_spec: Option<&str>,
    refspecs: &[String],
    server_options: &[String],
) -> Result<(
    Vec<(String, ObjectId)>,
    Vec<(String, ObjectId)>,
    Option<String>,
    Option<ObjectId>,
)> {
    let mut client_proto = protocol_wire::effective_client_protocol_version();
    if fetch_negotiation_algorithm(local_git_dir)
        .is_some_and(|value| value.eq_ignore_ascii_case("skipping"))
    {
        client_proto = 0;
    }
    let mut child = spawn_upload_pack_with_proto(upload_pack_cmd, remote_repo_path, client_proto)?;
    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;

    let (mut advertised, head_symref, v2_caps) = if client_proto == 2 {
        let caps = read_v2_capability_block(&mut stdout).context("read v2 capabilities")?;
        trace2_transfer::emit_negotiated_version_client_fetch_v2();
        if let Some(rest) = caps.iter().find_map(|l| l.strip_prefix("session-id=")) {
            trace2_transfer::emit_server_sid(rest);
        }
        if server_advertises_bundle_uri(&caps) && transfer_bundle_uri_enabled() {
            let cap_send = cap_lines_for_bundle_request(&caps);
            write_bundle_uri_command(&mut stdin, &cap_send)?;
            drain_bundle_uri_response(&mut stdout)?;
        }
        if has_cli_refspecs && refspecs_are_explicit_oid_sources(refspecs) {
            (Vec::new(), None, Some(caps))
        } else {
            let pair = v2_ls_refs_for_fetch(
                &mut stdin,
                &mut stdout,
                include_head_ref_prefix,
                refspecs,
                server_options,
            )?;
            (pair.0, pair.1, Some(caps))
        }
    } else {
        let (adv, hsym, saw_v1, _, server_sid) = read_advertisement(&mut stdout)?;
        trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
        if saw_v1 {
            crate::trace_packet::trace_packet_git('<', "version 1");
        }
        if let Some(ref sid) = server_sid {
            trace2_transfer::emit_server_sid(sid);
        }
        (adv, hsym, None)
    };
    if !has_cli_refspecs {
        merge_remote_refs_into_upload_pack_advertisement(remote_repo_path, &mut advertised)?;
    }
    let wants = filter_wants_already_local(local_git_dir, compute_wants(&advertised)?);
    if has_hide_refs_for_fetch_connectivity(local_git_dir) {
        crate::trace_run_command_git_invocation(&[
            "rev-list",
            "--objects",
            "--stdin",
            "--exclude-hidden=fetch",
        ]);
    }
    crate::trace_packet::trace_fetch_tip_availability(&local_git_dir.join("objects"), &wants);
    if wants.is_empty() {
        // No pack to transfer (either already up-to-date or refspecs selected no refs), but we
        // still return advertised heads/tags so callers can perform ref/prune bookkeeping.
        //
        // The protocol-v2 `promisor-remote` capability is part of the handshake, not the pack
        // round: Git applies `promisor.storeFields` (persisting advertised filter/token into local
        // config) whenever the server advertises promisor remotes the client accepts, even when no
        // objects are wanted. Evaluate it here so a `git fetch` against an up-to-date server whose
        // advertised filter changed still updates the local config (t5710 storeFields).
        if let Some(caps) = v2_caps.as_ref() {
            evaluate_promisor_remote_advertisement(local_git_dir, caps, filter_spec)?;
        }
        drop(stdin);
        let _ = drain_child_stdout_to_eof(&mut stdout);
        let status = child.wait()?;
        if !status.success() {
            bail!("upload-pack exited with {}", status);
        }
        let remote_heads: Vec<_> = advertised
            .iter()
            .filter(|(n, _)| n.starts_with("refs/heads/"))
            .cloned()
            .collect();
        let remote_tags: Vec<_> = advertised
            .iter()
            .filter(|(n, _)| n.starts_with("refs/tags/"))
            .cloned()
            .collect();
        let head_advertised_oid = advertised
            .iter()
            .find(|(n, _)| n == "HEAD")
            .map(|(_, o)| *o);
        return Ok((remote_heads, remote_tags, head_symref, head_advertised_oid));
    }

    let remote_heads: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let remote_tags: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/tags/"))
        .cloned()
        .collect();

    let head_advertised_oid = advertised
        .iter()
        .find(|(n, _)| n == "HEAD")
        .map(|(_, o)| *o);

    let pack_buf = if client_proto == 2 {
        let caps = v2_caps.context("internal: missing v2 capability list")?;
        let default_hash = client_fetch_object_format();
        let sideband_all = v2_fetch_supports_sideband_all(&caps);

        // Promisor-remote capability (protocol v2): if the server advertised promisor remotes,
        // evaluate `promisor.acceptFromServer` and reply with the accepted names. Accepting also
        // resolves `--filter=auto` to the combined advertised filters and may store advertised
        // fields locally (`promisor.storeFields`).
        let promisor_outcome =
            evaluate_promisor_remote_advertisement(local_git_dir, &caps, filter_spec)?;
        let promisor_reply = promisor_outcome.reply.clone();
        let effective_filter_spec = promisor_outcome.effective_filter_spec.as_deref();
        // Only request object filtering when the server advertised the `filter` feature. Matches
        // `fetch-pack.c` `send_filter`: a promisor remote without `uploadpack.allowFilter` does not
        // advertise it, so the client warns and fetches unfiltered instead of sending a `filter`
        // line the server rejects as "unexpected line" (t0410 "fetching from another promisor
        // remote", where a lazily registered `partialclonefilter=blob:none` would otherwise leak
        // into a plain `git fetch` against a non-filtering remote).
        let requested_filter = effective_filter_spec.or(filter_spec);
        let wire_filter_spec = if v2_fetch_supports_filter(&caps) {
            requested_filter
        } else {
            if requested_filter.is_some_and(|s| !s.trim().is_empty()) {
                eprintln!("warning: filtering not recognized by server, ignoring");
            }
            None
        };
        let client_sid = trace2_transfer::transfer_advertise_sid_enabled(local_git_dir)
            .then(trace2_transfer::trace2_session_id_wire_once);
        let (shallow_oids, depth, deepen_relative, shallow_since, shallow_exclude, unshallow) =
            if let Some(opts) = shallow_options {
                (
                    read_local_shallow_oids(local_git_dir)?,
                    opts.depth.or(opts.deepen),
                    opts.depth.is_none() && opts.deepen.is_some(),
                    opts.shallow_since.as_deref(),
                    opts.shallow_exclude.as_slice(),
                    opts.unshallow,
                )
            } else {
                (Vec::new(), None, false, None, &[][..], false)
            };
        // Advertise locally-available tips (bundle refs applied via `--bundle-uri`, plus existing
        // heads/tags/HEAD) as `have` lines so the server can build a thin pack and skip objects we
        // already obtained from the bundle (t5558 `negotiation:` cases require the bundle tip to be
        // sent as `have`). Skipped during a shallow/deepening request, where the local objects do
        // not form a usable negotiation base. Note `shallow_options` is `Some` for every clone, so
        // gate on an actual shallow/deepen request rather than its presence.
        let shallow_request = depth.is_some()
            || shallow_since.is_some()
            || !shallow_exclude.is_empty()
            || unshallow
            || !shallow_oids.is_empty();
        let haves: Vec<ObjectId> = if shallow_request {
            Vec::new()
        } else {
            local_negotiation_haves(local_git_dir, &wants, negotiation_tip_oids)
        };

        // When the server advertised `ref-in-want` and the user named refs on the command line,
        // request those refs by name (`want-ref <name>`) instead of resolving them to OIDs
        // client-side, mirroring `fetch-pack.c` `add_wants`. The named refs' OIDs are dropped from
        // the plain `want` list; exact-OID sources and follow-tag wants stay as `want <oid>`.
        let (want_refs, plain_wants): (Vec<String>, Vec<ObjectId>) = if has_cli_refspecs
            && v2_fetch_supports_ref_in_want(&caps)
            && !shallow_request
        {
            let (refs, _exact) = cli_want_refs_and_oids(&advertised, refspecs);
            if refs.is_empty() {
                (Vec::new(), wants.clone())
            } else {
                let want_ref_oids: HashSet<ObjectId> = refs
                    .iter()
                    .filter_map(|name| advertised.iter().find(|(n, _)| n == name).map(|(_, o)| *o))
                    .collect();
                let remaining: Vec<ObjectId> = wants
                    .iter()
                    .copied()
                    .filter(|o| !want_ref_oids.contains(o))
                    .collect();
                (refs, remaining)
            }
        } else {
            (Vec::new(), wants.clone())
        };

        // Multi-round negotiation (matches `fetch-pack.c` `do_fetch_pack_v2`): when we have local
        // `have`s to offer and this is not a shallow request, send wants + a first batch of haves
        // *without* `done`, read the `acknowledgments` section, then (if the server is not yet
        // `ready`) send the remaining haves + `done` and read the pack. Records
        // `negotiation_v2.total_rounds` so trace2 consumers (t5703) see the real round count.
        let multi_round = !haves.is_empty() && !shallow_request;
        let mut buf = Vec::new();
        if multi_round {
            let first_batch = haves.len().min(INITIAL_FLUSH);
            // Round 1: wants/want-refs + first batch of haves, no `done`.
            write_v2_fetch_request(
                &mut stdin,
                &default_hash,
                &plain_wants,
                &haves[..first_batch],
                sideband_all,
                include_tag,
                deepen_relative,
                client_sid.as_deref(),
                &[],
                wire_filter_spec,
                &shallow_oids,
                depth,
                shallow_since,
                shallow_exclude,
                unshallow,
                promisor_reply.as_deref(),
                &want_refs,
                false,
            )?;
            let mut total_rounds = 1usize;
            let ack = read_v2_acknowledgments(&mut stdout)?;
            let server_ready = ack.as_ref().map(|a| a.ready).unwrap_or(false);
            if server_ready {
                // Server is `ready`: the pack follows in the same response after a delim.
                read_v2_fetch_pack_response(&mut stdout, &mut buf)?;
                drop(stdin);
            } else if ack.is_none() {
                // Server skipped acknowledgments and went straight to the pack (already handled the
                // header inside the reader): nothing more to send.
                read_v2_fetch_pack_response(&mut stdout, &mut buf)?;
                drop(stdin);
            } else {
                // Round 2: remaining haves + `done`, then read the pack.
                total_rounds = 2;
                write_v2_fetch_request(
                    &mut stdin,
                    &default_hash,
                    &plain_wants,
                    &haves[first_batch..],
                    sideband_all,
                    include_tag,
                    deepen_relative,
                    client_sid.as_deref(),
                    &[],
                    wire_filter_spec,
                    &shallow_oids,
                    depth,
                    shallow_since,
                    shallow_exclude,
                    unshallow,
                    promisor_reply.as_deref(),
                    &want_refs,
                    true,
                )?;
                drop(stdin);
                read_v2_fetch_pack_response(&mut stdout, &mut buf)?;
            }
            crate::trace2_emit_data_intmax("negotiation_v2", "total_rounds", total_rounds as i64);
        } else {
            write_v2_fetch_request(
                &mut stdin,
                &default_hash,
                &plain_wants,
                &haves,
                sideband_all,
                include_tag,
                deepen_relative,
                client_sid.as_deref(),
                &[],
                wire_filter_spec,
                &shallow_oids,
                depth,
                shallow_since,
                shallow_exclude,
                unshallow,
                promisor_reply.as_deref(),
                &want_refs,
                true,
            )?;
            // Close stdin so `upload-pack` v2 sees EOF after this fetch; otherwise `serve_loop`
            // blocks for the next command while we block reading the pack response (deadlock).
            drop(stdin);
            read_v2_fetch_pack_response(&mut stdout, &mut buf)?;
        }
        buf
    } else {
        let buf = fetch_upload_pack_negotiate_pack_bytes_with_streams(
            local_git_dir,
            &advertised,
            &mut stdin,
            &mut stdout,
            &wants,
            negotiation_tip_oids,
            shallow_options,
            filter_spec,
        )?;
        drop(stdin);
        buf
    };

    let status = child.wait()?;
    if !status.success() {
        bail!("upload-pack exited with {}", status);
    }

    // When the client already has every wanted object, `pack-objects --thin` can stream an empty
    // body (or only the 12-byte PACK header). That is still a successful fetch (ref updates only).
    if !pack_buf.is_empty() && (pack_buf.len() < 12 || &pack_buf[0..4] != b"PACK") {
        bail!("did not receive a pack file from upload-pack");
    }

    unpack_upload_pack_bytes(local_git_dir, &pack_buf, filter_active)?;

    Ok((remote_heads, remote_tags, head_symref, head_advertised_oid))
}

/// Git's `everything_local`: a wanted ref tip need not be requested when its object — and its full
/// reachable closure — is already available in the local object store (which includes `--reference`
/// / alternate object directories). Dropping such wants is what keeps a `--reference` clone or a
/// fetch into a repo with an alternate from asking the server for objects it can already borrow
/// (`t5604` "fetched no objects" / "fetch with incomplete alternates").
fn want_is_locally_complete(repo: &Repository, oid: &ObjectId) -> bool {
    let Ok(obj) = repo.odb.read(oid) else {
        return false;
    };
    match obj.kind {
        ObjectKind::Commit => {
            grit_lib::connectivity::push_tip_objects_exist(repo, *oid).unwrap_or(false)
        }
        ObjectKind::Tag => {
            // A tag is complete only if its (possibly chained) target's closure is present.
            match parse_tag(&obj.data) {
                Ok(tag) => want_is_locally_complete(repo, &tag.object),
                Err(_) => false,
            }
        }
        // A tree or blob ref tip is complete once the object itself is present.
        ObjectKind::Tree | ObjectKind::Blob => true,
    }
}

/// Drop wants already satisfied locally (see [`want_is_locally_complete`]). Returns the surviving
/// wants. Never removes everything-vs-nothing distinctions the caller relies on: an empty result
/// means the fetch needs no pack, which callers already handle.
fn filter_wants_already_local(local_git_dir: &Path, wants: Vec<ObjectId>) -> Vec<ObjectId> {
    if wants.is_empty() {
        return wants;
    }
    let Ok(repo) = Repository::open(local_git_dir, None) else {
        return wants;
    };
    // Only filter when there is at least one alternate / borrowable store; otherwise a normal
    // clone (empty local ODB) keeps all wants and the check is pure overhead.
    wants
        .into_iter()
        .filter(|oid| !want_is_locally_complete(&repo, oid))
        .collect()
}

fn fetch_negotiation_algorithm(local_git_dir: &Path) -> Option<String> {
    ConfigSet::load(Some(local_git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("fetch.negotiationalgorithm"))
}

/// True when `fetch.negotiationAlgorithm` selects the `noop` negotiator.
///
/// The noop negotiator (`git/negotiator/noop.c`) makes `add_tip`, `known_common`, and `next` all
/// no-ops, so the client advertises no `have` lines at all and the server always sends a full pack.
/// Used by both the v0/v1 and v2 fetch paths to suppress the entire have/ACK exchange (t5554).
fn fetch_negotiation_is_noop(local_git_dir: &Path) -> bool {
    fetch_negotiation_algorithm(local_git_dir)
        .is_some_and(|value| value.eq_ignore_ascii_case("noop"))
}

/// Result of evaluating a server's `promisor-remote` advertisement on the client.
struct PromisorRemoteOutcome {
    /// `promisor-remote=<names>` reply value, or `None` to send nothing.
    reply: Option<String>,
    /// When the request filter was `auto`, the combined filter from accepted remotes to send on
    /// the wire in its place; otherwise `None` (use the original filter spec).
    effective_filter_spec: Option<String>,
}

/// Client-side handling of the server's protocol-v2 `promisor-remote` capability.
///
/// Parses the advertisement out of `server_caps`, applies `promisor.acceptFromServer` /
/// `promisor.checkFields`, stores advertised fields per `promisor.storeFields` (emitting the
/// upstream "Storing new ..." messages on stderr), and — when `request_filter_spec` is `auto` —
/// resolves the combined filter to send on the wire from the accepted remotes' advertised filters.
fn evaluate_promisor_remote_advertisement(
    local_git_dir: &Path,
    server_caps: &[String],
    request_filter_spec: Option<&str>,
) -> Result<PromisorRemoteOutcome> {
    let none = PromisorRemoteOutcome {
        reply: None,
        effective_filter_spec: None,
    };

    let Some(info) = server_caps
        .iter()
        .find_map(|l| l.strip_prefix("promisor-remote="))
    else {
        return Ok(none);
    };

    let cfg = ConfigSet::load(Some(local_git_dir), true).unwrap_or_default();
    let outcome = grit_lib::promisor_remote::promisor_remote_reply(&cfg, info);

    if outcome.accepted.is_empty() {
        return Ok(none);
    }

    // promisor.storeFields: persist accepted remotes' advertised fields into local config.
    store_promisor_fields(local_git_dir, &cfg, info)?;

    // --filter=auto: replace "auto" on the wire with the combined advertised filters of the
    // accepted remotes. Only a single accepted remote with one filter is exercised by the tests,
    // for which the combined spec is just that filter.
    let effective_filter_spec = if request_filter_spec.map(str::trim) == Some("auto") {
        construct_combined_filter(&outcome.accepted_filters)
    } else {
        None
    };

    Ok(PromisorRemoteOutcome {
        reply: outcome.reply,
        effective_filter_spec,
    })
}

/// Build the combined filter spec from accepted remotes' advertised filters (Git's
/// `promisor_remote_construct_filter`). For a single filter the result is that filter verbatim;
/// for multiple, a `combine:` spec joining the url-encoded subfilters.
fn construct_combined_filter(accepted_filters: &[(String, String)]) -> Option<String> {
    let filters: Vec<&str> = accepted_filters
        .iter()
        .map(|(_, f)| f.as_str())
        .filter(|f| !f.is_empty())
        .collect();
    match filters.len() {
        0 => None,
        1 => Some(filters[0].to_string()),
        _ => Some(
            filters
                .iter()
                .map(|f| grit_lib::rev_list::url_encode_object_filter_subspec(f))
                .collect::<Vec<_>>()
                .join("+"),
        ),
    }
}

/// Apply `promisor.storeFields`: for each accepted, already-configured remote, store the
/// advertised `partialCloneFilter` / `token` into local config when it differs, printing the
/// upstream notification on stderr. Returns once all fields are processed.
fn store_promisor_fields(local_git_dir: &Path, cfg: &ConfigSet, info: &str) -> Result<()> {
    let store_fields_raw = cfg.get("promisor.storeFields").unwrap_or_default();
    let mut store_filter = false;
    let mut store_token = false;
    for f in store_fields_raw
        .split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if f.eq_ignore_ascii_case("partialclonefilter") {
            store_filter = true;
        } else if f.eq_ignore_ascii_case("token") {
            store_token = true;
        }
    }
    if !store_filter && !store_token {
        return Ok(());
    }

    // Re-evaluate which remotes are accepted (need each advertised remote's full field set).
    let accept = grit_lib::promisor_remote::promisor_remote_reply(cfg, info);
    if accept.accepted.is_empty() {
        return Ok(());
    }
    let accepted_set: std::collections::HashSet<&str> =
        accept.accepted.iter().map(String::as_str).collect();

    let advertised = grit_lib::promisor_remote::parse_advertisement(info);

    let config_path = local_git_dir.join("config");
    let mut changed = false;
    let mut file = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(f) => f,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };

    for adv in &advertised {
        if !accepted_set.contains(adv.name.as_str()) {
            continue;
        }
        // Only store for remotes already configured locally (Git refuses to create new remotes).
        if cfg.get(&format!("remote.{}.url", adv.name)).is_none()
            && cfg.get(&format!("remote.{}.promisor", adv.name)).is_none()
        {
            continue;
        }
        if store_filter {
            if let Some(new_filter) = adv.filter.as_deref().filter(|f| !f.is_empty()) {
                if valid_filter(new_filter) {
                    let key = format!("remote.{}.partialCloneFilter", adv.name);
                    let current = cfg.get(&key);
                    if current.as_deref() != Some(new_filter) {
                        eprintln!(
                            "Storing new filter from server for remote '{}'.\n    '{}' -> '{}'",
                            adv.name,
                            current.as_deref().unwrap_or(""),
                            new_filter
                        );
                        file.set(&key, new_filter)?;
                        changed = true;
                    }
                }
            }
        }
        if store_token {
            if let Some(new_token) = adv.token.as_deref().filter(|t| !t.is_empty()) {
                if valid_token(new_token) {
                    let key = format!("remote.{}.token", adv.name);
                    let current = cfg.get(&key);
                    if current.as_deref() != Some(new_token) {
                        eprintln!(
                            "Storing new token from server for remote '{}'.\n    '{}' -> '{}'",
                            adv.name,
                            current.as_deref().unwrap_or(""),
                            new_token
                        );
                        file.set(&key, new_token)?;
                        changed = true;
                    }
                }
            }
        }
    }

    if changed {
        file.write().context("storing promisor fields")?;
    }
    Ok(())
}

/// A filter is storable only if it parses (matches Git's `valid_filter`).
fn valid_filter(spec: &str) -> bool {
    grit_lib::rev_list::ObjectFilter::parse(spec).is_ok()
}

/// A token is storable only if it contains no control characters (Git's `valid_token`).
fn valid_token(token: &str) -> bool {
    !token.chars().any(|c| c.is_control())
}

/// Store a received pack from `upload-pack` into the local ODB.
///
/// When `filter_active` is true (client sent `filter` during fetch/clone), objects may be
/// omitted and the pack must be kept as a **promisor** pack with a sibling `.promisor` marker,
/// matching Git's partial-clone behavior (`t0410`).
pub(crate) fn unpack_upload_pack_bytes(
    local_git_dir: &Path,
    pack_buf: &[u8],
    filter_active: bool,
) -> Result<()> {
    if pack_buf.len() <= 12 {
        return Ok(());
    }
    append_pack_to_git_trace_packfile(pack_buf)?;
    if filter_active || std::env::var_os("GRIT_FETCH_KEEP_PACK").is_some() {
        let repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open repository {}", local_git_dir.display()))?;
        let pack_path =
            index_pack::ingest_pack_bytes(&repo, pack_buf, true).context("ingest fetched pack")?;
        if filter_active {
            let _ = std::fs::File::create(pack_path.with_extension("promisor"));
        }
        lazy_fetch_missing_ref_delta_bases(&repo, pack_buf)?;
        return Ok(());
    }
    if should_store_fetched_pack_as_pack(local_git_dir, pack_buf) {
        let repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open repository {}", local_git_dir.display()))?;
        index_pack::ingest_pack_bytes(&repo, pack_buf, true).context("ingest fetched pack")?;
        lazy_fetch_missing_ref_delta_bases(&repo, pack_buf)?;
        return Ok(());
    }
    let odb = Odb::new(&local_git_dir.join("objects"));
    let repo = Repository::open(local_git_dir, None)
        .with_context(|| format!("open repository {}", local_git_dir.display()))?;
    lazy_fetch_missing_ref_delta_bases(&repo, pack_buf)?;
    let mut reader = pack_buf;
    unpack_objects(&mut reader, &odb, &UnpackOptions::default())?;
    Ok(())
}

fn lazy_fetch_missing_ref_delta_bases(repo: &Repository, pack_buf: &[u8]) -> Result<()> {
    let missing_bases = missing_ref_delta_bases(repo, pack_buf)?;
    if !missing_bases.is_empty() {
        let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(
            repo,
            &missing_bases,
        );
    }
    Ok(())
}

fn missing_ref_delta_bases(repo: &Repository, pack_buf: &[u8]) -> Result<Vec<ObjectId>> {
    if pack_buf.len() < 12 || &pack_buf[..4] != b"PACK" {
        return Ok(Vec::new());
    }
    let count = u32::from_be_bytes([pack_buf[8], pack_buf[9], pack_buf[10], pack_buf[11]]) as usize;
    let mut pos = 12usize;
    let mut missing = Vec::new();
    for _ in 0..count {
        let offset = pos as u64;
        if let Some(grit_lib::pack::PackedDeltaDependency::RefBase { base_oid }) =
            grit_lib::pack::read_packed_delta_dependency(pack_buf, offset)?
        {
            if !repo.odb.exists_local(&base_oid) {
                missing.push(base_oid);
            }
        }
        let slice = grit_lib::pack::slice_one_pack_object(pack_buf, offset, 20)?;
        pos += slice.len();
        if pos > pack_buf.len().saturating_sub(20) {
            break;
        }
    }
    missing.sort();
    missing.dedup();
    Ok(missing)
}

fn should_store_fetched_pack_as_pack(local_git_dir: &Path, pack_buf: &[u8]) -> bool {
    let Some(unpack_limit) = fetch_unpack_limit(local_git_dir) else {
        return false;
    };
    if pack_buf.len() < 12 || &pack_buf[..4] != b"PACK" {
        return false;
    }
    let object_count =
        u32::from_be_bytes([pack_buf[8], pack_buf[9], pack_buf[10], pack_buf[11]]) as usize;
    object_count >= unpack_limit
}

fn fetch_unpack_limit(local_git_dir: &Path) -> Option<usize> {
    let cfg = grit_lib::config::ConfigSet::load(Some(local_git_dir), true).ok()?;
    for key in ["fetch.unpacklimit", "transfer.unpacklimit"] {
        let Some(raw) = cfg.get(key) else {
            continue;
        };
        let Ok(limit) = raw.trim().parse::<i64>() else {
            continue;
        };
        if limit > 0 {
            return Some(limit as usize);
        }
    }
    None
}

fn append_pack_to_git_trace_packfile(pack: &[u8]) -> anyhow::Result<()> {
    let Ok(path) = std::env::var("GIT_TRACE_PACKFILE") else {
        return Ok(());
    };
    if path.is_empty() {
        return Ok(());
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("GIT_TRACE_PACKFILE: open {}", path))?;
    f.write_all(pack)
        .with_context(|| format!("GIT_TRACE_PACKFILE: write {}", path))?;
    Ok(())
}

fn read_local_shallow_oids(local_git_dir: &Path) -> Result<Vec<ObjectId>> {
    let shallow_path = local_git_dir.join("shallow");
    if !shallow_path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in std::fs::read_to_string(&shallow_path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        if let Ok(oid) = ObjectId::from_hex(line) {
            out.push(oid);
        }
    }
    Ok(out)
}

fn fetch_upload_pack_negotiate_pack_bytes(
    local_git_dir: &Path,
    remote_repo_path: &Path,
    upload_pack_cmd: Option<&str>,
    wants: &[ObjectId],
    filter_spec: Option<&str>,
) -> Result<Vec<u8>> {
    let client_proto = protocol_wire::effective_client_protocol_version();
    let mut child = spawn_upload_pack_with_proto(upload_pack_cmd, remote_repo_path, client_proto)?;
    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;
    let pack_buf = if client_proto == 2 {
        let caps = read_v2_capability_block(&mut stdout).context("read v2 capabilities")?;
        trace2_transfer::emit_negotiated_version_client_fetch_v2();
        if let Some(rest) = caps.iter().find_map(|l| l.strip_prefix("session-id=")) {
            trace2_transfer::emit_server_sid(rest);
        }
        if server_advertises_bundle_uri(&caps) && transfer_bundle_uri_enabled() {
            let cap_send = cap_lines_for_bundle_request(&caps);
            write_bundle_uri_command(&mut stdin, &cap_send)?;
            drain_bundle_uri_response(&mut stdout)?;
        }
        let default_hash = client_fetch_object_format();
        let sideband_all = v2_fetch_supports_sideband_all(&caps);
        // Drop the filter line when the server does not advertise `filter` (see `send_filter` in
        // `fetch-pack.c`): sending it to a non-filtering server is a fatal "unexpected line".
        let wire_filter_spec = if v2_fetch_supports_filter(&caps) {
            filter_spec
        } else {
            None
        };
        let client_sid = trace2_transfer::transfer_advertise_sid_enabled(local_git_dir)
            .then(trace2_transfer::trace2_session_id_wire_once);
        write_v2_fetch_request(
            &mut stdin,
            &default_hash,
            wants,
            &[],
            sideband_all,
            false,
            false,
            client_sid.as_deref(),
            &[],
            wire_filter_spec,
            &[],
            None,
            None,
            &[],
            false,
            None,
            &[],
            true,
        )?;
        drop(stdin);
        let mut out = Vec::new();
        read_v2_fetch_pack_response(&mut stdout, &mut out)?;
        // Explicit-OID lazy fetches negotiate in a single round (a lone `want ... done`); record
        // `total_rounds`=1 like upstream `fetch-pack.c` so partial-clone checkout traces match
        // (t5601 #108).
        crate::trace2_emit_data_intmax("negotiation_v2", "total_rounds", 1);
        out
    } else {
        let (advertised, _head_symref, saw_v1, saw_v2, server_sid) =
            read_advertisement(&mut stdout)?;
        if saw_v2 {
            trace2_transfer::emit_negotiated_version_client_fetch_v2();
        } else {
            trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
        }
        if let Some(ref sid) = server_sid {
            trace2_transfer::emit_server_sid(sid);
        }
        let out = fetch_upload_pack_negotiate_pack_bytes_with_streams(
            local_git_dir,
            &advertised,
            &mut stdin,
            &mut stdout,
            wants,
            None,
            None,
            filter_spec,
        )?;
        drop(stdin);
        // Explicit-OID lazy fetches negotiate in a single round; record `total_rounds`=1 like
        // upstream `fetch-pack.c` (t5601 #108).
        crate::trace2_emit_data_intmax("negotiation_v0_v1", "total_rounds", 1);
        out
    };

    let status = child.wait()?;
    if !status.success() {
        bail!("upload-pack exited with {}", status);
    }

    if !pack_buf.is_empty() && (pack_buf.len() < 12 || &pack_buf[0..4] != b"PACK") {
        bail!("did not receive a pack file from upload-pack");
    }

    append_pack_to_git_trace_packfile(&pack_buf)?;

    Ok(pack_buf)
}

pub(crate) fn fetch_upload_pack_negotiate_pack_bytes_with_streams(
    local_git_dir: &Path,
    advertised: &[(String, ObjectId)],
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    wants: &[ObjectId],
    negotiation_tip_oids: Option<&[ObjectId]>,
    shallow_options: Option<&UploadPackShallowOptions>,
    filter_spec: Option<&str>,
) -> Result<Vec<u8>> {
    let local_repo = Repository::open(local_git_dir, None)
        .with_context(|| format!("open local repository {}", local_git_dir.display()))?;

    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    let first_want = wants[0];
    let agent = crate::version_string();
    // Match `git fetch-pack` capability order on the first `want` line (see pkt traces in t5700).
    let mut caps = format!(
        " multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta deepen-since deepen-not agent=git/{agent}"
    );
    if filter_spec.is_some_and(|s| !s.trim().is_empty()) {
        caps.push_str(" filter");
    }
    if trace2_transfer::transfer_advertise_sid_enabled(local_git_dir) {
        caps.push_str(" session-id=");
        caps.push_str(&trace2_transfer::trace2_session_id_wire_once());
    }
    let mut req = Vec::new();
    if std::env::var("GIT_TRACE_PACKET")
        .ok()
        .filter(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .is_some()
    {
        let local_cfg =
            grit_lib::config::ConfigSet::load(Some(local_git_dir), true).unwrap_or_default();
        if local_cfg.get("protocol.version").as_deref() == Some("2") {
            // t0410: `grep "fetch< fetch=.*ref-in-want"` expects a trace line after v2 ref-in-want fetch.
            trace_packet_fetch('<', "fetch=ref-in-want");
        }
    }
    let w0 = format!("want {}{}", first_want.to_hex(), caps);
    trace_packet_fetch('>', w0.as_str());
    pkt_line::write_line_to_vec(&mut req, &w0)?;
    for w in wants.iter().skip(1) {
        let line = format!("want {}", w.to_hex());
        trace_packet_fetch('>', line.as_str());
        pkt_line::write_line_to_vec(&mut req, &line)?;
    }
    // Match `git fetch-pack`: when only one unique OID is wanted, send a second bare `want`
    // line (same as the first) before the flush. Some servers (notably `git-daemon`) expect this.
    if wants.len() == 1 {
        let line = format!("want {}", first_want.to_hex());
        trace_packet_fetch('>', line.as_str());
        pkt_line::write_line_to_vec(&mut req, &line)?;
    }
    if let Some(opts) = shallow_options {
        for shallow_oid in read_local_shallow_oids(local_git_dir)? {
            let line = format!("shallow {}", shallow_oid.to_hex());
            trace_packet_fetch('>', line.as_str());
            pkt_line::write_line_to_vec(&mut req, &line)?;
        }
        if opts.unshallow {
            // Match fetch-pack's sentinel deepen value for --unshallow.
            trace_packet_fetch('>', "deepen 2147483647");
            pkt_line::write_line_to_vec(&mut req, "deepen 2147483647")?;
        } else if let Some(depth) = opts.depth.or(opts.deepen) {
            let line = format!("deepen {depth}");
            trace_packet_fetch('>', line.as_str());
            pkt_line::write_line_to_vec(&mut req, &line)?;
        }
        if let Some(since) = opts.shallow_since.as_deref() {
            // `upload-pack` parses `deepen-since` with `parse_timestamp`; send the integer
            // `approxidate` yields rather than the raw date string (t5539 fetch shallow since).
            let value =
                grit_lib::git_date::approx::approxidate_careful(since.trim(), None).to_string();
            let line = format!("deepen-since {value}");
            trace_packet_fetch('>', line.as_str());
            pkt_line::write_line_to_vec(&mut req, &line)?;
        }
        for exclude in &opts.shallow_exclude {
            let line = format!("deepen-not {exclude}");
            trace_packet_fetch('>', line.as_str());
            pkt_line::write_line_to_vec(&mut req, &line)?;
        }
    }
    if let Some(spec) = filter_spec.map(str::trim).filter(|s| !s.is_empty()) {
        // Send the canonical/expanded filter spec on the wire, matching Git's
        // `expand_list_objects_filter_spec` (e.g. `blob:limit=1k` -> `blob:limit=1024`).
        let expanded = grit_lib::rev_list::expand_object_filter_for_protocol(spec)
            .unwrap_or_else(|_| spec.to_owned());
        let line = format!("filter {expanded}");
        trace_packet_fetch('>', line.as_str());
        pkt_line::write_line_to_vec(&mut req, &line)?;
    }
    req.extend_from_slice(b"0000");
    stdin.write_all(&req).context("write wants")?;
    stdin.flush()?;

    // For a deepen/shallow request the server replies with a shallow-info section (terminated by a
    // flush) *before* expecting `have` lines. Read it now so a rejected `want` (the server sends
    // `ERR upload-pack: not our ref` and exits) is surfaced as a clean fatal error instead of an
    // EPIPE abort when we go on to write `have`/`done` into the closed pipe
    // (t5516 'deny fetch unreachable SHA1' / 'shallow fetch reachable SHA1').
    let deepen_requested = shallow_options.is_some_and(|o| {
        o.depth.is_some()
            || o.deepen.is_some()
            || o.shallow_since.is_some()
            || !o.shallow_exclude.is_empty()
            || o.unshallow
    });
    if deepen_requested {
        loop {
            match pkt_line::read_packet(stdout)? {
                Some(pkt_line::Packet::Flush) | None => break,
                Some(pkt_line::Packet::Data(ln)) => {
                    let line = ln.trim_end();
                    trace_packet_fetch('<', line);
                    if let Some(msg) = line.strip_prefix("ERR ") {
                        bail!("fatal: remote error: {}", msg.trim_end());
                    }
                    // `shallow <oid>` / `unshallow <oid>` lines are consumed here; the local shallow
                    // boundary is (re)computed from the fetched heads after the pack is applied.
                }
                Some(other) => bail!("unexpected upload-pack shallow-info response: {other:?}"),
            }
        }
    }

    // The `noop` negotiator (`git/negotiator/noop.c`) emits no `have` lines, so skip the entire
    // have/ACK exchange and go straight to `done` (t5554). An empty `--negotiation-tip` list
    // likewise suppresses haves.
    let suppress_haves = negotiation_tip_oids.is_some_and(|tips| tips.is_empty())
        || fetch_negotiation_is_noop(local_git_dir);
    let mut negotiator = SkippingNegotiator::new(local_repo);

    if !suppress_haves {
        if let Ok(entries) = refs::list_refs(local_git_dir, "refs/bundles/") {
            for (name, oid) in entries {
                let t = if let Ok(resolved) = resolve_revision(negotiator.repo(), &name) {
                    resolved
                } else {
                    oid
                };
                if negotiator.repo().odb.read(&t).is_ok() {
                    if let Some(c) = peel_commit_oid_for_negotiation(negotiator.repo(), t)? {
                        negotiator.add_tip(c)?;
                    }
                }
            }
        }
    }

    if !suppress_haves {
        for w in wants {
            if negotiator.repo().odb.read(w).is_ok() {
                if let Some(c) = peel_commit_oid_for_negotiation(negotiator.repo(), *w)? {
                    negotiator.add_tip(c)?;
                }
            }
        }
    }

    let mut tips: Vec<ObjectId> = Vec::new();
    let mut tip_filter: Option<HashSet<ObjectId>> = None;
    if let Some(tips) = negotiation_tip_oids {
        let mut set = HashSet::new();
        for tip in tips {
            if let Some(peeled) = peel_commit_oid_for_negotiation(negotiator.repo(), *tip)? {
                set.insert(peeled);
            }
        }
        tip_filter = Some(set);
    }

    if !suppress_haves {
        for prefix in ["refs/heads/", "refs/tags/"] {
            if let Ok(entries) = refs::list_refs(local_git_dir, prefix) {
                for (name, oid) in entries {
                    let tip = if let Ok(resolved) = resolve_revision(negotiator.repo(), &name) {
                        resolved
                    } else {
                        oid
                    };
                    if negotiator.repo().odb.read(&tip).is_err() {
                        continue;
                    }
                    let Some(peeled) = peel_commit_oid_for_negotiation(negotiator.repo(), tip)?
                    else {
                        continue;
                    };
                    if tip_filter
                        .as_ref()
                        .is_some_and(|filter| !filter.contains(&peeled))
                    {
                        continue;
                    }
                    tips.push(peeled);
                }
            }
        }
        if let Ok(h) = refs::resolve_ref(local_git_dir, "HEAD") {
            if negotiator.repo().odb.read(&h).is_ok() {
                if let Some(peeled) = peel_commit_oid_for_negotiation(negotiator.repo(), h)? {
                    if !tip_filter
                        .as_ref()
                        .is_some_and(|filter| !filter.contains(&peeled))
                    {
                        tips.push(peeled);
                    }
                }
            }
        }
        for sym in ["HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            if let Ok(oid) = resolve_revision(negotiator.repo(), sym) {
                if negotiator.repo().odb.read(&oid).is_ok() {
                    let Some(peeled) = peel_commit_oid_for_negotiation(negotiator.repo(), oid)?
                    else {
                        continue;
                    };
                    if tip_filter
                        .as_ref()
                        .is_some_and(|filter| !filter.contains(&peeled))
                    {
                        continue;
                    }
                    tips.push(peeled);
                }
            }
        }
        tips.sort_by_key(|o| o.to_hex());
        tips.dedup();
        for t in tips {
            if want_set.contains(&t) {
                continue;
            }
            if negotiator.repo().odb.read(&t).is_err() {
                continue;
            }
            negotiator.add_tip(t)?;
        }
    }

    // With no `have` lines, Git's upload-pack does not send `NAK` until it sees `done`
    // (`upload-pack.c` `get_common_commits`). Reading ACKs here deadlocks the child on a pipe.
    if !suppress_haves {
        for (_, oid) in advertised {
            if want_set.contains(oid) {
                continue;
            }
            if negotiator.repo().odb.read(oid).is_ok() {
                if let Some(c) = peel_commit_oid_for_negotiation(negotiator.repo(), *oid)? {
                    negotiator.known_common(c)?;
                }
            }
        }
    }

    let mut count: usize = 0;
    let mut flush_at: usize = INITIAL_FLUSH;
    let mut pending = Vec::new();
    let stateless_rpc = false;
    let mut flushes: i32 = 0;

    while let Some(oid) = negotiator.next_have()? {
        let h = format!("have {}", oid.to_hex());
        trace_packet_fetch('>', h.as_str());
        pkt_line::write_line_to_vec(&mut pending, &h)?;
        count += 1;
        if flush_at <= count {
            pending.extend_from_slice(b"0000");
            if let Err(e) = stdin
                .write_all(&pending)
                .and_then(|()| stdin.flush())
                .context("write have flush")
            {
                if is_broken_pipe_io(&e) {
                    return Err(fail_with_upload_pack_err(stdout));
                }
                return Err(e);
            }
            pending.clear();
            flush_at = next_flush_count(stateless_rpc, count);
            flushes += 1;

            // Match fetch-pack: skip reading ACKs after the first flush so one window stays ahead.
            if !stateless_rpc && count == INITIAL_FLUSH {
                continue;
            }

            read_ack_round_with_negotiator(stdout, &mut negotiator)?;
            flushes -= 1;
        }
    }

    if !pending.is_empty() {
        pending.extend_from_slice(b"0000");
        if let Err(e) = stdin
            .write_all(&pending)
            .and_then(|()| stdin.flush())
            .context("final have flush")
        {
            if is_broken_pipe_io(&e) {
                return Err(fail_with_upload_pack_err(stdout));
            }
            return Err(e);
        }
        flushes += 1;
    }

    while flushes > 0 {
        read_ack_round_with_negotiator(stdout, &mut negotiator)?;
        flushes -= 1;
    }

    // Match `fetch-pack.c` `find_common`: send `done` as a single pkt-line with no trailing flush
    // before reading the server's `ACK`/`NAK` and the pack (a stray `0000` leaves a flush on the
    // wire and breaks side-band demux).
    let mut tail = Vec::new();
    pkt_line::write_line_to_vec(&mut tail, "done")?;
    trace_packet_fetch('>', "done");
    if let Err(e) = stdin
        .write_all(&tail)
        .and_then(|()| stdin.flush())
        .context("write done")
    {
        if is_broken_pipe_io(&e) {
            return Err(fail_with_upload_pack_err(stdout));
        }
        return Err(e);
    }

    // `upload-pack` responds to `done` with `ACK <oid>` or `NAK` before streaming the pack.
    match pkt_line::read_packet(stdout)? {
        None => bail!("unexpected EOF from upload-pack after done"),
        Some(pkt_line::Packet::Flush) => {
            bail!("unexpected flush from upload-pack after done");
        }
        Some(pkt_line::Packet::Data(ln)) => {
            trace_packet_fetch('<', ln.trim_end());
            if ln.trim_end() == "NAK" {
                // Expected when we had nothing in common.
            } else if let Some((ack_oid, kind)) = parse_ack(&ln) {
                if kind != AckKind::Bare {
                    let _ = negotiator.ack(ack_oid)?;
                }
            }
        }
        Some(_) => {}
    }

    let mut pack_buf = Vec::new();
    read_sideband_pack_until_done(stdout, &mut pack_buf)?;

    Ok(pack_buf)
}

fn has_hide_refs_for_fetch_connectivity(local_git_dir: &Path) -> bool {
    if std::env::var("GIT_CONFIG_PARAMETERS")
        .ok()
        .is_some_and(|v| {
            let lower = v.to_ascii_lowercase();
            lower.contains("fetch.hiderefs=") || lower.contains("transfer.hiderefs=")
        })
    {
        return true;
    }
    grit_lib::config::ConfigSet::load(Some(local_git_dir), true)
        .ok()
        .is_some_and(|cfg| {
            cfg.entries().iter().any(|entry| {
                let key = entry.key.as_str();
                key.starts_with("fetch.hiderefs") || key.starts_with("transfer.hiderefs")
            })
        })
}

/// When tests run `git-daemon` with `--base-path=<GIT_DAEMON_DOCUMENT_ROOT_PATH>`, map a
/// `git://host:port/repo` URL to that on-disk repository so local commands can open it.
pub fn try_local_path_for_git_daemon_url(url: &str) -> Option<std::path::PathBuf> {
    let root = std::env::var("GIT_DAEMON_DOCUMENT_ROOT_PATH").ok()?;
    let parsed = parse_git_url(url).ok()?;
    let rel = parsed.path.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    Some(std::path::Path::new(&root).join(rel))
}

/// Parsed `git://host[:port]/path` (path includes leading `/`).
pub struct GitDaemonUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Parse `git://` URLs for the native Git daemon transport.
pub fn parse_git_url(url: &str) -> Result<GitDaemonUrl> {
    let rest = url
        .strip_prefix("git://")
        .with_context(|| format!("not a git:// URL: {url}"))?;
    let (authority, path_part) = rest
        .find('/')
        .map(|i| (&rest[..i], &rest[i..]))
        .unwrap_or((rest, "/"));
    if path_part.is_empty() || path_part == "/" {
        bail!("git:// URL missing repository path");
    }
    let path = path_part.to_string();
    let (host, port) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .with_context(|| format!("invalid git:// authority: {authority}"))?;
        let host = authority[1..end].to_string();
        let port = if let Some(p) = authority[end + 1..].strip_prefix(':') {
            p.parse::<u16>()
                .with_context(|| format!("invalid port in git:// URL: {url}"))?
        } else {
            9418
        };
        (host, port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        let h = h.trim_end_matches(':');
        if p.is_empty() {
            (h.to_string(), 9418)
        } else if p.chars().all(|c| c.is_ascii_digit()) {
            (
                h.to_string(),
                p.parse::<u16>()
                    .with_context(|| format!("invalid port in git:// URL: {url}"))?,
            )
        } else {
            (authority.to_string(), 9418)
        }
    } else {
        (authority.to_string(), 9418)
    };
    if host.is_empty() {
        bail!("git:// URL has empty host");
    }
    Ok(GitDaemonUrl { host, port, path })
}

/// Fetch over `git://` (native daemon) using upload-pack negotiation.
pub fn fetch_via_git_protocol_skipping(
    local_git_dir: &Path,
    url: &str,
    refspecs: &[String],
    filter_active: bool,
) -> Result<(
    Vec<(String, ObjectId)>,
    Vec<(String, ObjectId)>,
    Option<String>,
    Option<ObjectId>,
)> {
    let parsed = parse_git_url(url)?;
    if let Some(result) = try_fetch_via_local_gitproxy(local_git_dir, &parsed)? {
        return Ok(result);
    }
    let addr = format!("{}:{}", parsed.host, parsed.port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve git://{}:{}", parsed.host, parsed.port))?
        .next()
        .with_context(|| format!("no addresses for git://{}:{}", parsed.host, parsed.port))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(30))
        .with_context(|| format!("could not connect to git://{}:{}", parsed.host, parsed.port))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(600)));

    let mut stream_w = stream
        .try_clone()
        .context("dup git:// socket for simultaneous read/write")?;
    let client_proto = protocol_wire::effective_client_protocol_version();
    let virtual_host = std::env::var("GIT_OVERRIDE_VIRTUAL_HOST")
        .unwrap_or_else(|_| format!("{}:{}", parsed.host, parsed.port));
    let mut inner: Vec<u8> = Vec::new();
    inner.extend_from_slice(b"git-upload-pack ");
    inner.extend_from_slice(parsed.path.as_bytes());
    inner.push(0);
    inner.extend_from_slice(b"host=");
    inner.extend_from_slice(virtual_host.as_bytes());
    inner.push(0);
    if client_proto > 0 {
        inner.push(0);
        inner.extend_from_slice(format!("version={client_proto}\0").as_bytes());
    }
    pkt_line::write_packet_raw(&mut stream_w, &inner).context("write git:// request")?;
    stream_w.flush().ok();

    let trace_show = String::from_utf8_lossy(&inner)
        .replace('\0', "\\0")
        .replace('\n', "");
    trace_packet_fetch('>', &trace_show);

    let (mut advertised, mut head_symref, saw_v1, saw_v2, server_sid) =
        read_advertisement(&mut stream)?;
    if saw_v2 {
        trace2_transfer::emit_negotiated_version_client_fetch_v2();
    } else {
        trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
    }
    if let Some(ref sid) = server_sid {
        trace2_transfer::emit_server_sid(sid);
    }
    let mut use_v2_fetch = saw_v2;
    let try_v2_ls_refs = saw_v2 || (client_proto == 2 && advertised.is_empty());
    if try_v2_ls_refs {
        match v2_ls_refs_for_fetch(&mut stream_w, &mut stream, true, refspecs, &[]) {
            Ok((v2_refs, v2_head_symref)) => {
                use_v2_fetch = true;
                if !v2_refs.is_empty() {
                    advertised = v2_refs;
                }
                if head_symref.is_none() {
                    head_symref = v2_head_symref;
                }
            }
            Err(_) if !saw_v2 => {
                // Some `git://` servers still answer with a v0/v1 ref advertisement even when
                // the client requests protocol v2. In that mixed mode we should continue with the
                // already-parsed v0/v1 refs instead of failing the fetch/clone.
            }
            Err(e) => return Err(e),
        }
    }

    if advertised.is_empty() {
        return Ok((Vec::new(), Vec::new(), head_symref, None));
    }
    let wants = collect_wants(&advertised, refspecs)?;
    let remote_heads: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let remote_tags: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/tags/"))
        .cloned()
        .collect();

    let head_advertised_oid = advertised
        .iter()
        .find(|(n, _)| n == "HEAD")
        .map(|(_, o)| *o);

    if wants.is_empty() {
        return Ok((remote_heads, remote_tags, head_symref, head_advertised_oid));
    }

    let pack_buf = if use_v2_fetch {
        let default_hash = client_fetch_object_format();
        write_v2_fetch_request(
            &mut stream_w,
            &default_hash,
            &wants,
            &[],
            false,
            true,
            false,
            None,
            &[],
            None,
            &[],
            None,
            None,
            &[],
            false,
            None,
            &[],
            true,
        )?;
        let mut buf = Vec::new();
        read_v2_fetch_pack_response(&mut stream, &mut buf)?;
        buf
    } else {
        fetch_upload_pack_negotiate_pack_bytes_with_streams(
            local_git_dir,
            &advertised,
            &mut stream_w,
            &mut stream,
            &wants,
            None,
            None,
            None,
        )?
    };

    if !pack_buf.is_empty() && (pack_buf.len() < 12 || &pack_buf[0..4] != b"PACK") {
        bail!("did not receive a pack file from upload-pack");
    }

    unpack_upload_pack_bytes(local_git_dir, &pack_buf, filter_active)?;

    Ok((remote_heads, remote_tags, head_symref, head_advertised_oid))
}

/// Fetch over SSH using the configured SSH command and upload-pack negotiation.
pub fn fetch_via_ssh_upload_pack_skipping(
    local_git_dir: &Path,
    spec: &crate::ssh_transport::SshUrl,
    upload_pack_cmd: Option<&str>,
    refspecs: &[String],
    filter_active: bool,
) -> Result<(
    Vec<(String, ObjectId)>,
    Vec<(String, ObjectId)>,
    Option<String>,
    Option<ObjectId>,
)> {
    let mut child = crate::ssh_transport::spawn_git_ssh_upload_pack(spec, upload_pack_cmd)?;
    let mut stdin = child.stdin.take().context("ssh upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("ssh upload-pack stdout")?;

    let (mut advertised, mut head_symref, saw_v1, saw_v2, server_sid) =
        read_advertisement(&mut stdout)?;
    if saw_v2 {
        trace2_transfer::emit_negotiated_version_client_fetch_v2();
    } else {
        trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
    }
    if let Some(ref sid) = server_sid {
        trace2_transfer::emit_server_sid(sid);
    }

    let mut use_v2_fetch = saw_v2;
    if saw_v2 {
        let (v2_refs, v2_head_symref) =
            v2_ls_refs_for_fetch(&mut stdin, &mut stdout, true, refspecs, &[])?;
        use_v2_fetch = true;
        if !v2_refs.is_empty() {
            advertised = v2_refs;
        }
        if head_symref.is_none() {
            head_symref = v2_head_symref;
        }
    }

    if advertised.is_empty() {
        drop(stdin);
        let _ = drain_child_stdout_to_eof(&mut stdout);
        let status = child.wait()?;
        if !status.success() {
            bail!("ssh upload-pack exited with {}", status);
        }
        return Ok((Vec::new(), Vec::new(), head_symref, None));
    }

    let wants = collect_wants(&advertised, refspecs)?;
    let remote_heads: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let remote_tags: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/tags/"))
        .cloned()
        .collect();
    let head_advertised_oid = advertised
        .iter()
        .find(|(n, _)| n == "HEAD")
        .map(|(_, o)| *o);

    if wants.is_empty() {
        drop(stdin);
        let _ = drain_child_stdout_to_eof(&mut stdout);
        let status = child.wait()?;
        if !status.success() {
            bail!("ssh upload-pack exited with {}", status);
        }
        return Ok((remote_heads, remote_tags, head_symref, head_advertised_oid));
    }

    let pack_buf = if use_v2_fetch {
        let default_hash = client_fetch_object_format();
        write_v2_fetch_request(
            &mut stdin,
            &default_hash,
            &wants,
            &[],
            false,
            true,
            false,
            None,
            &[],
            None,
            &[],
            None,
            None,
            &[],
            false,
            None,
            &[],
            true,
        )?;
        drop(stdin);
        let mut buf = Vec::new();
        read_v2_fetch_pack_response(&mut stdout, &mut buf)?;
        buf
    } else {
        let buf = fetch_upload_pack_negotiate_pack_bytes_with_streams(
            local_git_dir,
            &advertised,
            &mut stdin,
            &mut stdout,
            &wants,
            None,
            None,
            None,
        )?;
        drop(stdin);
        buf
    };

    let status = child.wait()?;
    if !status.success() {
        bail!("ssh upload-pack exited with {}", status);
    }

    if !pack_buf.is_empty() && (pack_buf.len() < 12 || &pack_buf[0..4] != b"PACK") {
        bail!("did not receive a pack file from ssh upload-pack");
    }
    unpack_upload_pack_bytes(local_git_dir, &pack_buf, filter_active)?;

    Ok((remote_heads, remote_tags, head_symref, head_advertised_oid))
}

type GitFetchResult = (
    Vec<(String, ObjectId)>,
    Vec<(String, ObjectId)>,
    Option<String>,
    Option<ObjectId>,
);

fn try_fetch_via_local_gitproxy(
    local_git_dir: &Path,
    parsed: &GitDaemonUrl,
) -> Result<Option<GitFetchResult>> {
    if parsed.host.starts_with('-') {
        return Ok(None);
    }
    let config = ConfigSet::load(Some(local_git_dir), true).unwrap_or_default();
    if config
        .get("core.gitproxy")
        .filter(|value| !value.trim().is_empty())
        .is_none()
    {
        return Ok(None);
    }
    let rel = parsed.path.trim_start_matches('/');
    if rel.is_empty() {
        return Ok(None);
    }
    let repo_path = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(rel);
    if !repo_path.exists() {
        return Ok(None);
    }
    let remote = Repository::open(&repo_path.join(".git"), Some(&repo_path))
        .or_else(|_| Repository::open(&repo_path, None))
        .with_context(|| format!("opening gitproxy target {}", repo_path.display()))?;
    copy_object_dir_contents(
        &remote.git_dir.join("objects"),
        &local_git_dir.join("objects"),
    )?;
    let heads = refs::list_refs(&remote.git_dir, "refs/heads/")?;
    let tags = refs::list_refs(&remote.git_dir, "refs/tags/")?;
    let head_symref = refs::read_symbolic_ref(&remote.git_dir, "HEAD")
        .ok()
        .flatten();
    let head_oid = refs::resolve_ref(&remote.git_dir, "HEAD").ok();
    Ok(Some((heads, tags, head_symref, head_oid)))
}

fn copy_object_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_object_dir_contents(&src_path, &dst_path)?;
        } else if !dst_path.exists() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::hard_link(&src_path, &dst_path).is_err() {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
    }
    Ok(())
}

/// Query refs from a `git://` remote using upload-pack negotiation.
///
/// Returns advertised refs, optional `symref=HEAD:` target, and whether protocol v1/v2 was seen.
pub fn ls_remote_via_git_protocol(
    url: &str,
) -> Result<(Vec<(String, ObjectId)>, Option<String>, bool, bool)> {
    let parsed = parse_git_url(url)?;
    let addr = format!("{}:{}", parsed.host, parsed.port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve git://{}:{}", parsed.host, parsed.port))?
        .next()
        .with_context(|| format!("no addresses for git://{}:{}", parsed.host, parsed.port))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(30))
        .with_context(|| format!("could not connect to git://{}:{}", parsed.host, parsed.port))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(600)));

    let mut stream_w = stream
        .try_clone()
        .context("dup git:// socket for simultaneous read/write")?;
    let client_proto = protocol_wire::effective_client_protocol_version();
    let virtual_host = std::env::var("GIT_OVERRIDE_VIRTUAL_HOST")
        .unwrap_or_else(|_| format!("{}:{}", parsed.host, parsed.port));
    let mut inner: Vec<u8> = Vec::new();
    inner.extend_from_slice(b"git-upload-pack ");
    inner.extend_from_slice(parsed.path.as_bytes());
    inner.push(0);
    inner.extend_from_slice(b"host=");
    inner.extend_from_slice(virtual_host.as_bytes());
    inner.push(0);
    if client_proto > 0 {
        inner.push(0);
        inner.extend_from_slice(format!("version={client_proto}\0").as_bytes());
    }
    pkt_line::write_packet_raw(&mut stream_w, &inner).context("write git:// request")?;
    stream_w.flush().ok();

    let trace_show = String::from_utf8_lossy(&inner)
        .replace('\0', "\\0")
        .replace('\n', "");
    trace_packet_fetch('>', &trace_show);

    let (mut advertised, mut head_symref, saw_v1, saw_v2, _server_sid) =
        read_advertisement(&mut stream)?;
    if saw_v2 {
        let (v2_refs, v2_head_symref) =
            v2_ls_refs_for_fetch(&mut stream_w, &mut stream, true, &[], &[])?;
        if !v2_refs.is_empty() {
            advertised = v2_refs;
        }
        if head_symref.is_none() {
            head_symref = v2_head_symref;
        }
    }

    Ok((advertised, head_symref, saw_v1, saw_v2))
}
