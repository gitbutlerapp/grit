//! Patch-ID computation for commit equivalence detection.
//!
//! A patch-ID is a digest of the normalised diff a commit introduces, using the
//! repository's object hash algorithm (SHA-1 or SHA-256).
//! Whitespace is stripped from every changed line before hashing, so two
//! commits whose diffs differ only in whitespace (spaces, tabs, newlines)
//! produce identical patch-IDs.  This is the semantics required by
//! `git cherry` and `git format-patch --ignore-if-in-upstream`.
//!
//! Two complementary entry points are provided:
//!
//! - [`compute_patch_id`] operates on a repository's object database, computing
//!   the diff from the commit's tree objects.
//! - [`compute_patch_ids_from_text`] parses unified diff text from stdin (e.g.
//!   output of `git log -p` or `git diff-tree --patch --stdin`), matching the
//!   behaviour of `git patch-id`.

use similar::{ChangeTag, TextDiff};

use crate::diff::{diff_trees_with_renames, zero_oid, DiffEntry, DiffStatus};
use crate::error::Result;
use crate::hash::ObjectHasher;
use crate::merge_file;
use crate::objects::{parse_commit, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;

const PATCH_ID_DIGEST_MAX: usize = 32;

/// How to compute a patch-ID from unified diff text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchIdMode {
    /// Accumulate file diffs into one rolling SHA-1; file order affects the
    /// result.  Compatible with Git 1.9 and older.  This is the default.
    Unstable,
    /// Hash each file's diff independently and accumulate results with a
    /// carry-addition, so file order does not affect the patch-ID.
    Stable,
    /// Like [`PatchIdMode::Stable`] but whitespace is not stripped before
    /// hashing.
    Verbatim,
}

/// Compute patch-IDs from unified diff text (e.g. `git log -p` output).
///
/// Parses the text line by line.  Each time a `commit <oid>` or
/// `From <oid> …` marker is found the accumulated hash for the previous patch
/// is finalised and emitted as `(patch_id, commit_id)`.  Commits with no diff
/// content (empty patches, merge commits) are silently skipped.
///
/// # Parameters
///
/// - `algo` — object hash algorithm (must match the repository that produced the diff).
/// - `input` — raw bytes of the unified diff stream.
/// - `mode`  — which patch-ID algorithm to use (see [`PatchIdMode`]).
///
/// # Returns
///
/// A `Vec` of `(patch_id, commit_id)` pairs in the order they were encountered
/// in the stream.
pub fn compute_patch_ids_from_text(
    algo: HashAlgo,
    input: &[u8],
    mode: PatchIdMode,
) -> Vec<(ObjectId, ObjectId)> {
    let stable = mode != PatchIdMode::Unstable;
    let verbatim = mode == PatchIdMode::Verbatim;
    let digest_len = algo.len();

    let mut results: Vec<(ObjectId, ObjectId)> = Vec::new();

    // Current accumulated state for the patch being processed.
    let mut ctx = ObjectHasher::new(algo);
    let mut result = [0u8; PATCH_ID_DIGEST_MAX];
    let mut patchlen: usize = 0;
    // before/after: -1 = parsing file header, 0 = awaiting @@ hunk, >0 = in hunk
    let mut before: i32 = -1;
    let mut after: i32 = -1;
    let mut diff_is_binary = false;
    let mut pre_oid_str = String::new();
    let mut post_oid_str = String::new();
    let mut current_commit: Option<ObjectId> = None;
    // When the input starts directly with a diff (no commit header),
    // use the zero OID as the commit ID.
    let mut implicit_commit = true;

    // Iterate lines; we keep the trailing '\n' so remove_space mirrors git's
    // behaviour (newline counts as whitespace and is stripped).
    let lines = split_lines_with_nl(input);

    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        i += 1;

        // The line as a &str for prefix checks; non-UTF-8 treated as empty.
        let line = std::str::from_utf8(raw).unwrap_or("");

        // Try to strip "commit " or "From " prefix to reach a potential OID.
        let oid_candidate: Option<&str> = if let Some(rest) = line.strip_prefix("commit ") {
            Some(rest)
        } else if let Some(rest) = line.strip_prefix("From ") {
            Some(rest)
        } else {
            None
        };

        if let Some(candidate) = oid_candidate {
            if let Some(oid) = try_parse_oid_prefix(candidate, algo) {
                // Finalise the patch we've been accumulating.
                text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
                if patchlen > 0 {
                    if let Some(coid) = current_commit.take() {
                        if let Ok(pid) = ObjectId::from_bytes(&result[..digest_len]) {
                            results.push((pid, coid));
                        }
                    }
                }
                // Reset for the new patch.
                result = [0u8; PATCH_ID_DIGEST_MAX];
                ctx = ObjectHasher::new(algo);
                patchlen = 0;
                before = -1;
                after = -1;
                diff_is_binary = false;
                pre_oid_str.clear();
                post_oid_str.clear();
                current_commit = Some(oid);
                implicit_commit = false;
                continue;
            }
        }

        // "\ No newline at end of file" markers.
        if line.starts_with("\\ ") && line.len() > 12 {
            if verbatim {
                ctx.update(raw);
                patchlen += raw.len();
            }
            continue;
        }

        // Skip commit metadata before the first diff hunk.
        if patchlen == 0 && !line.starts_with("diff ") {
            continue;
        }

        // If we see a diff line without a preceding commit marker,
        // treat it as a single patch with zero commit OID.
        if implicit_commit && line.starts_with("diff ") && current_commit.is_none() {
            current_commit = Some(ObjectId::zero());
            implicit_commit = false;
        }

        // Parsing the per-file header (before == -1).
        if before == -1 {
            if line.starts_with("GIT binary patch") || line.starts_with("Binary files") {
                // Binary: only hash the blob OID strings.
                diff_is_binary = true;
                before = 0;
                let pre = pre_oid_str.clone();
                let post = post_oid_str.clone();
                ctx.update(pre.as_bytes());
                ctx.update(post.as_bytes());
                patchlen += pre.len() + post.len();
                if stable {
                    text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
                }
                continue;
            } else if let Some(rest) = line.strip_prefix("index ") {
                // index <pre>..<post>[  <mode>]
                if let Some(dd) = rest.find("..") {
                    pre_oid_str = rest[..dd].to_owned();
                    let tail = &rest[dd + 2..];
                    let end = tail
                        .find(|c: char| c.is_ascii_whitespace())
                        .unwrap_or_else(|| {
                            tail.trim_end_matches('\n').trim_end_matches('\r').len()
                        });
                    post_oid_str = tail[..end].to_owned();
                }
                continue;
            } else if line.starts_with("--- ") {
                before = 1;
                after = 1;
                // Fall through to hunk-content processing below.
            } else if !line.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                // Non-alpha first char signals end of this patch's diffs.
                // Re-use the `continue` path; treat as patch boundary.
                text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
                if patchlen > 0 {
                    if let Some(coid) = current_commit.take() {
                        if let Ok(pid) = ObjectId::from_bytes(&result[..digest_len]) {
                            results.push((pid, coid));
                        }
                    }
                }
                result = [0u8; PATCH_ID_DIGEST_MAX];
                ctx = ObjectHasher::new(algo);
                patchlen = 0;
                before = -1;
                after = -1;
                diff_is_binary = false;
                continue;
            }
        }

        // Skip body of binary diffs; reset on the next `diff ` line.
        if diff_is_binary {
            if line.starts_with("diff ") {
                diff_is_binary = false;
                before = -1;
                // Process this `diff ` line as a new file header.
                i -= 1; // re-process
            }
            continue;
        }

        // Waiting for a hunk header or the next file.
        if before == 0 && after == 0 {
            if line.starts_with("@@ -") {
                let (b, a) = scan_hunk_header(line);
                before = b;
                after = a;
                continue;
            }
            if !line.starts_with("diff ") {
                // End of this file's content; nothing special to do—
                // if it's a commit boundary the loop head will handle it.
                continue;
            }
            // Another file diff starts; flush per-file hash in stable mode.
            if stable {
                text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
            }
            before = -1;
            after = -1;
            // Re-process this `diff ` line as a new file header.
            i -= 1;
            continue;
        }

        // Inside a hunk — update the line counters.
        let first = raw.first().copied().unwrap_or(b' ');
        if first == b'-' || first == b' ' {
            before -= 1;
        }
        if first == b'+' || first == b' ' {
            after -= 1;
        }

        // Hash the line, optionally stripping whitespace.
        let hashed = if verbatim {
            ctx.update(raw);
            raw.len()
        } else {
            hash_without_whitespace(&mut ctx, raw)
        };
        patchlen += hashed;
    }

    // Flush the final patch.
    text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
    if patchlen > 0 {
        if let Some(coid) = current_commit {
            if let Ok(pid) = ObjectId::from_bytes(&result[..digest_len]) {
                results.push((pid, coid));
            }
        }
    }

    results
}

/// Finalise `ctx`, accumulate its digest into `result` with byte-wise
/// carry-addition (mirrors git's `flush_one_hunk`), and reset `ctx`.
fn text_flush_one_hunk(
    result: &mut [u8],
    digest_len: usize,
    ctx: &mut ObjectHasher,
    algo: HashAlgo,
) {
    let mut hash_buf = [0u8; PATCH_ID_DIGEST_MAX];
    let old = std::mem::replace(ctx, ObjectHasher::new(algo));
    let _ = old.finalize_into(&mut hash_buf);
    let mut carry: u16 = 0;
    for i in 0..digest_len {
        carry = carry + result[i] as u16 + hash_buf[i] as u16;
        result[i] = carry as u8;
        carry >>= 8;
    }
}

/// Hash `raw` bytes into `ctx`, skipping ASCII whitespace.
///
/// Returns the number of non-whitespace bytes fed to the hasher.
fn hash_without_whitespace(ctx: &mut ObjectHasher, raw: &[u8]) -> usize {
    let mut count = 0;
    for &b in raw {
        if !b.is_ascii_whitespace() {
            ctx.update(&[b]);
            count += 1;
        }
    }
    count
}

/// Parse a full hex OID at the start of `s` for `algo`.
///
/// Returns `None` when `s` does not start with a valid hex digest for `algo`
/// optionally followed by ASCII whitespace.
fn try_parse_oid_prefix(s: &str, algo: HashAlgo) -> Option<ObjectId> {
    let s = s.trim_end_matches('\n').trim_end_matches('\r');
    let hex_len = algo.len() * 2;
    if s.len() < hex_len {
        return None;
    }
    let hex = &s[..hex_len];
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if s.len() > hex_len && !s.as_bytes()[hex_len].is_ascii_whitespace() {
        return None;
    }
    let mut bytes = [0u8; PATCH_ID_DIGEST_MAX];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    ObjectId::from_bytes(&bytes[..algo.len()]).ok()
}

/// Convert a single ASCII hex digit to its value.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse the before/after line counts from a `@@ -<old>[,<n>] +<new>[,<m>] @@` header.
///
/// Returns `(before, after)` counts, defaulting to 1 when the count is absent.
fn scan_hunk_header(line: &str) -> (i32, i32) {
    // line starts with "@@ -"
    let rest = match line.strip_prefix("@@ -") {
        Some(r) => r,
        None => return (1, 1),
    };
    // Parse old count: skip start line number, grab optional comma + count.
    let before = parse_hunk_count(rest);
    // Find " +" separator.
    let after = rest
        .find(" +")
        .and_then(|p| parse_hunk_count_opt(&rest[p + 2..]))
        .unwrap_or(1);
    (before, after)
}

/// Parse `<start>[,<count>]` and return `count` (or 1 if absent).
fn parse_hunk_count(s: &str) -> i32 {
    // Skip digits of the start line number.
    let after_start = s.trim_start_matches(|c: char| c.is_ascii_digit());
    if let Some(rest) = after_start.strip_prefix(',') {
        rest.split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(1)
    } else {
        1
    }
}

/// Same as [`parse_hunk_count`] but returns `Option`.
fn parse_hunk_count_opt(s: &str) -> Option<i32> {
    Some(parse_hunk_count(s))
}

/// Split `input` into lines, preserving the trailing `\n` on each line.
///
/// The final slice may lack a trailing newline if the input doesn't end with
/// one.
fn split_lines_with_nl(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in input.iter().enumerate() {
        if b == b'\n' {
            lines.push(&input[start..=i]);
            start = i + 1;
        }
    }
    if start < input.len() {
        lines.push(&input[start..]);
    }
    lines
}

/// Compute the patch-ID for a single commit.
///
/// Returns `None` for merge commits (more than one parent), since those do not
/// have a well-defined single-parent diff.  Root commits (no parents) are
/// compared against the empty tree.
///
/// # Parameters
///
/// - `algo` — object hash algorithm (must match `odb`'s configured format).
/// - `odb` — object database used to read commit, tree, and blob objects.
/// - `commit_oid` — OID of the commit to compute the patch-ID for.
///
/// # Errors
///
/// Returns errors from object-database reads or object-parse failures.
pub fn compute_patch_id(
    algo: HashAlgo,
    odb: &Odb,
    commit_oid: &ObjectId,
    mode: PatchIdMode,
) -> Result<Option<ObjectId>> {
    compute_patch_id_filtered(algo, odb, commit_oid, &[], mode)
}

/// Compute the patch-ID for a single commit, limited to matching pathspecs.
///
/// This follows [`compute_patch_id`] semantics, but ignores file diffs whose
/// old and new paths do not match `paths`.
///
/// # Parameters
///
/// - `algo` — object hash algorithm (must match `odb`'s configured format).
/// - `odb` — object database used to read commit, tree, and blob objects.
/// - `commit_oid` — OID of the commit to compute the patch-ID for.
/// - `paths` — pathspec strings used to limit the files included in the diff.
///
/// # Errors
///
/// Returns errors from object-database reads or object-parse failures.
pub fn compute_patch_id_for_paths(
    algo: HashAlgo,
    odb: &Odb,
    commit_oid: &ObjectId,
    paths: &[String],
    mode: PatchIdMode,
) -> Result<Option<ObjectId>> {
    compute_patch_id_filtered(algo, odb, commit_oid, paths, mode)
}

fn compute_patch_id_filtered(
    algo: HashAlgo,
    odb: &Odb,
    commit_oid: &ObjectId,
    paths: &[String],
    mode: PatchIdMode,
) -> Result<Option<ObjectId>> {
    let _ = mode;
    let digest_len = algo.len();
    let obj = odb.read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Ok(None);
    }
    let commit = parse_commit(&obj.data)?;

    // Merge commits (>1 parent) have no defined patch-id.
    if commit.parents.len() > 1 {
        return Ok(None);
    }

    // Resolve the parent tree (None = empty tree for root commits).
    let parent_tree_oid = if commit.parents.is_empty() {
        None
    } else {
        let parent_obj = odb.read(&commit.parents[0])?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        Some(parent_commit.tree)
    };

    // Match `git show` / `git diff-tree -M`: pair renames before hashing.
    let mut diffs =
        diff_trees_with_renames(odb, parent_tree_oid.as_ref(), Some(&commit.tree), "", 50)?;
    if !paths.is_empty() {
        diffs.retain(|entry| diff_entry_matches_paths(entry, paths));
    }
    if diffs.is_empty() {
        return Ok(None);
    }

    // Sort by primary path (lexicographic), matching diffcore_std ordering.
    diffs.sort_by(|a, b| a.path().cmp(b.path()));

    if diffs.len() == 1 {
        let entry = &diffs[0];
        if entry.old_oid != entry.new_oid {
            let old_bytes = read_blob(odb, &entry.old_oid)?;
            let new_bytes = read_blob(odb, &entry.new_oid)?;
            if merge_file::is_binary(&old_bytes) || merge_file::is_binary(&new_bytes) {
                let mut patch = Vec::new();
                patch_id_write_line(&mut patch, &format!("commit {}", commit_oid.to_hex()));
                write_patch_id_binary_entry(&mut patch, entry)?;
                let ids = compute_patch_ids_from_text(algo, &patch, mode);
                return Ok(ids.into_iter().next().map(|(patch_id, _)| patch_id));
            }
        }
    }

    let mut result = [0u8; PATCH_ID_DIGEST_MAX];

    for entry in &diffs {
        if entry.status == DiffStatus::Renamed {
            patch_id_hash_rename_entry(odb, algo, entry, &mut result, digest_len)?;
            continue;
        }
        // Git's patch-id header (`diff --git a/<x> b/<y>`) uses the present path on both sides for
        // pure additions/deletions (e.g. `diff --git a/file b/file` for a new file), so fall back to
        // the other side when one path is absent.
        let old_path = entry
            .old_path
            .as_deref()
            .or(entry.new_path.as_deref())
            .unwrap_or("");
        let new_path = entry
            .new_path
            .as_deref()
            .or(entry.old_path.as_deref())
            .unwrap_or("");
        let mut old_path_buf = old_path.as_bytes().to_vec();
        let mut new_path_buf = new_path.as_bytes().to_vec();
        let len1 = remove_space_bytes(&mut old_path_buf);
        let len2 = remove_space_bytes(&mut new_path_buf);

        let old_mode = parse_mode_u32(&entry.old_mode);
        let new_mode = parse_mode_u32(&entry.new_mode);

        let mut ctx = ObjectHasher::new(algo);
        patch_id_add_string(&mut ctx, b"diff--git");
        patch_id_add_string(&mut ctx, b"a/");
        ctx.update(&old_path_buf[..len1]);
        patch_id_add_string(&mut ctx, b"b/");
        ctx.update(&new_path_buf[..len2]);

        if old_mode == 0 {
            patch_id_add_string(&mut ctx, b"newfilemode");
            patch_id_add_mode(&mut ctx, new_mode);
        } else if new_mode == 0 {
            patch_id_add_string(&mut ctx, b"deletedfilemode");
            patch_id_add_mode(&mut ctx, old_mode);
        } else if old_mode != new_mode {
            patch_id_add_string(&mut ctx, b"oldmode");
            patch_id_add_mode(&mut ctx, old_mode);
            patch_id_add_string(&mut ctx, b"newmode");
            patch_id_add_mode(&mut ctx, new_mode);
        }

        if entry.old_oid == entry.new_oid {
            // Mode-only commits (and other metadata-only tree diffs) omit `---`/`+++` hunks.
        } else {
            let old_bytes = read_blob(odb, &entry.old_oid)?;
            let new_bytes = read_blob(odb, &entry.new_oid)?;

            if merge_file::is_binary(&old_bytes) || merge_file::is_binary(&new_bytes) {
                let a = patch_id_index_oid(&entry.old_oid);
                let b = patch_id_index_oid(&entry.new_oid);
                ctx.update(a.as_bytes());
                ctx.update(b.as_bytes());
            } else {
                let old_str = std::str::from_utf8(&old_bytes).unwrap_or("");
                let new_str = std::str::from_utf8(&new_bytes).unwrap_or("");

                if old_mode == 0 {
                    patch_id_add_string(&mut ctx, b"---/dev/null");
                    patch_id_add_string(&mut ctx, b"+++b/");
                    ctx.update(&new_path_buf[..len2]);
                } else if new_mode == 0 {
                    patch_id_add_string(&mut ctx, b"---a/");
                    ctx.update(&old_path_buf[..len1]);
                    patch_id_add_string(&mut ctx, b"+++/dev/null");
                } else {
                    patch_id_add_string(&mut ctx, b"---a/");
                    ctx.update(&old_path_buf[..len1]);
                    patch_id_add_string(&mut ctx, b"+++b/");
                    ctx.update(&new_path_buf[..len2]);
                }

                patch_id_hash_text_hunks(&mut ctx, old_str, new_str);
            }
        }

        text_flush_one_hunk(&mut result[..digest_len], digest_len, &mut ctx, algo);
    }

    if result.iter().take(digest_len).all(|&b| b == 0) {
        return Ok(None);
    }
    ObjectId::from_bytes(&result[..digest_len]).map(Some)
}

fn diff_entry_matches_paths(entry: &DiffEntry, paths: &[String]) -> bool {
    entry
        .old_path
        .as_deref()
        .into_iter()
        .chain(entry.new_path.as_deref())
        .any(|path| crate::pathspec::matches_pathspec_list(path, paths))
}

fn parse_mode_u32(mode: &str) -> u32 {
    u32::from_str_radix(mode.trim(), 8).unwrap_or(0)
}

/// Abbreviated OID as it appears on `index` lines in `git show -p` (7 hex chars, or `0000000`).
fn patch_id_write_line(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
}

/// `git show -p` headers for a binary file (no unified hunks).
fn write_patch_id_binary_entry(out: &mut Vec<u8>, entry: &DiffEntry) -> Result<()> {
    let old_path = entry
        .old_path
        .as_deref()
        .or(entry.new_path.as_deref())
        .unwrap_or("");
    let new_path = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("");
    patch_id_write_line(out, &format!("diff --git a/{old_path} b/{new_path}"));
    let old_mode = parse_mode_u32(&entry.old_mode);
    let new_mode = parse_mode_u32(&entry.new_mode);
    if old_mode == 0 {
        patch_id_write_line(out, &format!("new file mode {}", entry.new_mode.trim()));
    } else if new_mode == 0 {
        patch_id_write_line(out, &format!("deleted file mode {}", entry.old_mode.trim()));
    }
    patch_id_write_line(
        out,
        &format!(
            "index {}..{}",
            patch_id_index_oid(&entry.old_oid),
            patch_id_index_oid(&entry.new_oid),
        ),
    );
    let old_side = entry
        .old_path
        .as_ref()
        .map(|p| format!("a/{p}"))
        .unwrap_or_else(|| "/dev/null".to_owned());
    let new_side = entry
        .new_path
        .as_ref()
        .map(|p| format!("b/{p}"))
        .unwrap_or_else(|| "/dev/null".to_owned());
    patch_id_write_line(
        out,
        &format!("Binary files {old_side} and {new_side} differ"),
    );
    Ok(())
}

fn patch_id_index_oid(oid: &ObjectId) -> String {
    if *oid == zero_oid() {
        return "0000000".to_owned();
    }
    oid.to_hex().chars().take(7).collect()
}

fn patch_id_add_string(ctx: &mut ObjectHasher, s: &[u8]) {
    ctx.update(s);
}

fn patch_id_add_mode(ctx: &mut ObjectHasher, mode: u32) {
    let text = format!("{mode:06o}");
    ctx.update(text.as_bytes());
}

/// Hash a rename entry the same way `git patch-id` hashes `similarity index` /
/// `rename from` / `rename to` lines from `git show --patch`.
fn patch_id_hash_rename_entry(
    odb: &Odb,
    algo: HashAlgo,
    entry: &DiffEntry,
    result: &mut [u8],
    digest_len: usize,
) -> Result<()> {
    let old_path = entry.old_path.as_deref().unwrap_or("");
    let new_path = entry.new_path.as_deref().unwrap_or("");
    let mut old_path_buf = old_path.as_bytes().to_vec();
    let mut new_path_buf = new_path.as_bytes().to_vec();
    let len1 = remove_space_bytes(&mut old_path_buf);
    let len2 = remove_space_bytes(&mut new_path_buf);

    let mut ctx = ObjectHasher::new(algo);
    patch_id_add_string(&mut ctx, b"diff--git");
    patch_id_add_string(&mut ctx, b"a/");
    ctx.update(&old_path_buf[..len1]);
    patch_id_add_string(&mut ctx, b"b/");
    ctx.update(&new_path_buf[..len2]);

    let score = entry.score.unwrap_or(100);
    patch_id_add_string(&mut ctx, b"similarityindex");
    ctx.update(format!("{score}%").as_bytes());
    patch_id_add_string(&mut ctx, b"renamefrom");
    ctx.update(&old_path_buf[..len1]);
    patch_id_add_string(&mut ctx, b"renameto");
    ctx.update(&new_path_buf[..len2]);

    let old_mode = parse_mode_u32(&entry.old_mode);
    let new_mode = parse_mode_u32(&entry.new_mode);
    if old_mode != new_mode {
        patch_id_add_string(&mut ctx, b"oldmode");
        patch_id_add_mode(&mut ctx, old_mode);
        patch_id_add_string(&mut ctx, b"newmode");
        patch_id_add_mode(&mut ctx, new_mode);
    }

    if entry.old_oid != entry.new_oid {
        let old_bytes = read_blob(odb, &entry.old_oid)?;
        let new_bytes = read_blob(odb, &entry.new_oid)?;
        if merge_file::is_binary(&old_bytes) || merge_file::is_binary(&new_bytes) {
            let a = entry.old_oid.to_hex();
            let b = entry.new_oid.to_hex();
            ctx.update(a.as_bytes());
            ctx.update(b.as_bytes());
        } else {
            let old_str = std::str::from_utf8(&old_bytes).unwrap_or("");
            let new_str = std::str::from_utf8(&new_bytes).unwrap_or("");
            patch_id_add_string(&mut ctx, b"---a/");
            ctx.update(&old_path_buf[..len1]);
            patch_id_add_string(&mut ctx, b"+++b/");
            ctx.update(&new_path_buf[..len2]);
            patch_id_hash_text_hunks(&mut ctx, old_str, new_str);
        }
    }

    text_flush_one_hunk(result, digest_len, &mut ctx, algo);
    Ok(())
}

fn patch_id_hash_text_hunks(ctx: &mut ObjectHasher, old_str: &str, new_str: &str) {
    let diff = TextDiff::from_lines(old_str, new_str);
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Equal => b' ',
            ChangeTag::Delete => b'-',
            ChangeTag::Insert => b'+',
        };
        let text = change.as_str().unwrap_or("");
        for piece in text.split_inclusive('\n') {
            let line_body = piece.strip_suffix('\n').unwrap_or(piece);
            let mut line_buf = Vec::with_capacity(1 + line_body.len() + 1);
            line_buf.push(prefix);
            line_buf.extend_from_slice(line_body.as_bytes());
            line_buf.push(b'\n');
            let n = remove_space_bytes(&mut line_buf);
            ctx.update(&line_buf[..n]);
        }
    }
}

/// Strip ASCII whitespace in-place; returns new length (prefix of `buf` is valid).
fn remove_space_bytes(buf: &mut [u8]) -> usize {
    let mut dst = 0usize;
    let mut src = 0usize;
    while src < buf.len() {
        let c = buf[src];
        src += 1;
        if !c.is_ascii_whitespace() {
            buf[dst] = c;
            dst += 1;
        }
    }
    dst
}

/// Read a blob's raw bytes from the ODB.
///
/// Returns an empty `Vec` for the zero OID (representing an absent file).
fn read_blob(odb: &Odb, oid: &ObjectId) -> Result<Vec<u8>> {
    if *oid == zero_oid() {
        return Ok(Vec::new());
    }
    let obj = odb.read(oid)?;
    Ok(obj.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use std::process::Command;

    use tempfile::TempDir;

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn stable_patch_ids_match_git_patch_id_on_log_p() {
        let dir = TempDir::new().expect("tempdir");
        git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
        std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
        git_in(dir.path(), &["add", "f.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("f.txt"), b"one\ntwo\n").unwrap();
        git_in(dir.path(), &["add", "f.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "second"]);
        std::fs::write(dir.path().join("g.txt"), b"new\n").unwrap();
        git_in(dir.path(), &["add", "g.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "third"]);

        let log_p = git_in(dir.path(), &["log", "-p", "--reverse"]);
        let grit_ids =
            compute_patch_ids_from_text(HashAlgo::Sha1, log_p.as_bytes(), PatchIdMode::Stable);

        use std::io::Write;
        let mut child = Command::new("git")
            .current_dir(dir.path())
            .args(["patch-id", "--stable"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn git patch-id");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(log_p.as_bytes())
            .expect("write log");
        let git_out = child.wait_with_output().expect("wait");
        assert!(
            git_out.status.success(),
            "git patch-id --stable failed: {}",
            String::from_utf8_lossy(&git_out.stderr)
        );
        let git_lines: Vec<(ObjectId, ObjectId)> = String::from_utf8_lossy(&git_out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut parts = line.split_whitespace();
                let patch_hex = parts.next().expect("patch id");
                let commit_hex = parts.next().expect("commit id");
                let patch = ObjectId::from_hex(patch_hex).expect("patch hex");
                let commit = ObjectId::from_hex(commit_hex).expect("commit hex");
                (patch, commit)
            })
            .collect();

        assert_eq!(grit_ids.len(), git_lines.len(), "patch-id count mismatch");
        for ((g_patch, g_commit), (git_patch, git_commit)) in grit_ids.iter().zip(git_lines.iter())
        {
            assert_eq!(g_patch, git_patch, "patch-id mismatch");
            assert_eq!(g_commit, git_commit, "commit-id mismatch");
        }
    }

    fn git_patch_ids_from_log(dir: &std::path::Path, mode_flag: &str) -> Vec<(ObjectId, ObjectId)> {
        let log_p = git_in(dir, &["log", "-p", "--reverse"]);
        use std::io::Write;
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(["patch-id", mode_flag])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn git patch-id");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(log_p.as_bytes())
            .expect("write log");
        let out = child.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "git patch-id failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut parts = line.split_whitespace();
                let patch_hex = parts.next().expect("patch id");
                let commit_hex = parts.next().expect("commit id");
                (
                    ObjectId::from_hex(patch_hex).expect("patch hex"),
                    ObjectId::from_hex(commit_hex).expect("commit hex"),
                )
            })
            .collect()
    }

    #[test]
    fn t4204_patch_id_modes_match_git() {
        let dir = TempDir::new().expect("tempdir");
        git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
        std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
        git_in(dir.path(), &["add", "f.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("f.txt"), b"one\ntwo\n").unwrap();
        git_in(dir.path(), &["add", "f.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "second"]);
        std::fs::write(dir.path().join("g.bin"), &[0u8, 1, 2, 3]).unwrap();
        git_in(dir.path(), &["add", "g.bin"]);
        git_in(dir.path(), &["commit", "-q", "-m", "binary"]);
        std::fs::write(dir.path().join("h.txt"), b"a\n").unwrap();
        git_in(dir.path(), &["add", "h.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "add-h"]);
        git_in(dir.path(), &["mv", "h.txt", "i.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "rename"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dir.path().join("f.txt"))
                .expect("f.txt")
                .permissions();
            perms.set_mode(0o100755);
            std::fs::set_permissions(dir.path().join("f.txt"), perms).expect("chmod f.txt");
        }
        git_in(dir.path(), &["add", "f.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "mode-only"]);
        let log_p = git_in(dir.path(), &["log", "-p", "--reverse"]);

        for (mode, flag) in [
            (PatchIdMode::Stable, "--stable"),
            (PatchIdMode::Unstable, "--unstable"),
            (PatchIdMode::Verbatim, "--verbatim"),
        ] {
            let grit_ids = compute_patch_ids_from_text(HashAlgo::Sha1, log_p.as_bytes(), mode);
            let git_ids = git_patch_ids_from_log(dir.path(), flag);
            assert_eq!(grit_ids.len(), git_ids.len(), "patch-id count for {flag:?}");
            for ((g_patch, g_commit), (git_patch, git_commit)) in
                grit_ids.iter().zip(git_ids.iter())
            {
                assert_eq!(g_patch, git_patch, "patch-id {flag}");
                assert_eq!(g_commit, git_commit, "commit-id {flag}");
            }
        }

        let repo = crate::repo::Repository::open(
            &dir.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(dir.path()),
        )
        .unwrap();
        let git_stable = git_patch_ids_from_log(dir.path(), "--stable");
        let assert_odb_matches_git = |commit_hex: &str, label: &str| {
            let commit: ObjectId = commit_hex.trim().parse().expect("commit oid");
            let grit = compute_patch_id(HashAlgo::Sha1, &repo.odb(), &commit, PatchIdMode::Stable)
                .expect("patch")
                .expect("some");
            let git_patch = git_stable
                .iter()
                .find(|(_, c)| c == &commit)
                .expect(label)
                .0;
            assert_eq!(grit, git_patch, "{label}");
        };

        let second = git_in(
            dir.path(),
            &["rev-list", "--max-count=1", "--skip=4", "HEAD"],
        );
        assert_odb_matches_git(&second, "git patch-id for second commit");
        let binary = git_in(
            dir.path(),
            &["rev-list", "--max-count=1", "--skip=3", "HEAD"],
        );
        assert_odb_matches_git(&binary, "git patch-id for binary commit");
        let rename = git_in(
            dir.path(),
            &["rev-list", "--max-count=1", "--skip=1", "HEAD"],
        );
        assert_odb_matches_git(&rename, "git patch-id for rename commit");
        let mode_only = git_in(dir.path(), &["rev-parse", "HEAD"]);
        assert_odb_matches_git(&mode_only, "git patch-id for mode-only commit");

        let add_h = git_in(
            dir.path(),
            &["rev-list", "--max-count=1", "--skip=2", "HEAD"],
        );
        let add_h: ObjectId = add_h.trim().parse().unwrap();
        let filtered = compute_patch_id_for_paths(
            HashAlgo::Sha1,
            &repo.odb(),
            &add_h,
            &[String::from("h.txt")],
            PatchIdMode::Stable,
        )
        .expect("filtered h.txt")
        .expect("some h.txt");
        let git_h = git_stable
            .iter()
            .find(|(_, c)| c == &add_h)
            .expect("git patch-id for add-h")
            .0;
        assert_eq!(
            filtered, git_h,
            "ODB path-filter h.txt vs full commit patch-id"
        );
        assert!(
            compute_patch_id_for_paths(
                HashAlgo::Sha1,
                &repo.odb(),
                &add_h,
                &[String::from("f.txt")],
                PatchIdMode::Stable,
            )
            .expect("filtered f.txt")
            .is_none(),
            "path filter excluding all changes must be None"
        );

        git_in(dir.path(), &["checkout", "-q", "HEAD~4"]);
        git_in(dir.path(), &["checkout", "-q", "-b", "side"]);
        std::fs::write(dir.path().join("side.txt"), b"s\n").unwrap();
        git_in(dir.path(), &["add", "side.txt"]);
        git_in(dir.path(), &["commit", "-q", "-m", "side"]);
        git_in(dir.path(), &["checkout", "-q", "main"]);
        git_in(dir.path(), &["merge", "-q", "--no-ff", "side"]);
        let merge_tip: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .unwrap();
        assert!(
            compute_patch_id(HashAlgo::Sha1, &repo.odb(), &merge_tip, PatchIdMode::Stable)
                .expect("merge")
                .is_none()
        );
    }
}
