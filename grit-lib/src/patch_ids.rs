//! Patch-ID computation for commit equivalence detection.
//!
//! A patch-ID is a SHA-1 digest of the normalised diff a commit introduces.
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

use crate::diff::{diff_trees, zero_oid, DiffEntry};
use crate::error::Result;
use crate::hash::ObjectHasher;
use crate::merge_file;
use crate::objects::HashAlgo;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;

fn patch_id_hasher() -> ObjectHasher {
    HashAlgo::Sha1.hasher()
}

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
/// - `input` — raw bytes of the unified diff stream.
/// - `mode`  — which patch-ID algorithm to use (see [`PatchIdMode`]).
///
/// # Returns
///
/// A `Vec` of `(patch_id, commit_id)` pairs in the order they were encountered
/// in the stream.
pub fn compute_patch_ids_from_text(input: &[u8], mode: PatchIdMode) -> Vec<(ObjectId, ObjectId)> {
    let stable = mode != PatchIdMode::Unstable;
    let verbatim = mode == PatchIdMode::Verbatim;

    let mut results: Vec<(ObjectId, ObjectId)> = Vec::new();

    // Current accumulated state for the patch being processed.
    let mut ctx = patch_id_hasher();
    let mut result = [0u8; 20];
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
            if let Some(oid) = try_parse_oid_prefix(candidate) {
                // Finalise the patch we've been accumulating.
                text_flush_one_hunk(&mut result, &mut ctx);
                if patchlen > 0 {
                    if let Some(coid) = current_commit.take() {
                        if let Ok(pid) = ObjectId::from_bytes(&result) {
                            results.push((pid, coid));
                        }
                    }
                }
                // Reset for the new patch.
                result = [0u8; 20];
                ctx = patch_id_hasher();
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
                    text_flush_one_hunk(&mut result, &mut ctx);
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
                text_flush_one_hunk(&mut result, &mut ctx);
                if patchlen > 0 {
                    if let Some(coid) = current_commit.take() {
                        if let Ok(pid) = ObjectId::from_bytes(&result) {
                            results.push((pid, coid));
                        }
                    }
                }
                result = [0u8; 20];
                ctx = patch_id_hasher();
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
                text_flush_one_hunk(&mut result, &mut ctx);
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
    text_flush_one_hunk(&mut result, &mut ctx);
    if patchlen > 0 {
        if let Some(coid) = current_commit {
            if let Ok(pid) = ObjectId::from_bytes(&result) {
                results.push((pid, coid));
            }
        }
    }

    results
}

/// Finalise `ctx`, accumulate its digest into `result` with byte-wise
/// carry-addition (mirrors git's `flush_one_hunk`), and reset `ctx`.
fn text_flush_one_hunk(result: &mut [u8; 20], ctx: &mut ObjectHasher) {
    let old = std::mem::replace(ctx, patch_id_hasher());
    let finished = old.finalize();
    let hash = finished.as_bytes();
    let mut carry: u16 = 0;
    for i in 0..20 {
        carry = carry + result[i] as u16 + hash[i] as u16;
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

/// Parse a 40-hex OID at the start of `s`.
///
/// Returns `None` if `s` does not start with exactly 40 lowercase hex digits
/// optionally followed by ASCII whitespace.
fn try_parse_oid_prefix(s: &str) -> Option<ObjectId> {
    let s = s.trim_end_matches('\n').trim_end_matches('\r');
    if s.len() < 40 {
        return None;
    }
    let hex = &s[..40];
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The character after the OID (if any) must be whitespace or end of string.
    if s.len() > 40 && !s.as_bytes()[40].is_ascii_whitespace() {
        return None;
    }
    let mut bytes = [0u8; 20];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    ObjectId::from_bytes(&bytes).ok()
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
/// - `odb` — object database used to read commit, tree, and blob objects.
/// - `commit_oid` — OID of the commit to compute the patch-ID for.
///
/// # Errors
///
/// Returns errors from object-database reads or object-parse failures.
pub fn compute_patch_id(odb: &Odb, commit_oid: &ObjectId) -> Result<Option<ObjectId>> {
    compute_patch_id_filtered(odb, commit_oid, &[])
}

/// Compute the patch-ID for a single commit, limited to matching pathspecs.
///
/// This follows [`compute_patch_id`] semantics, but ignores file diffs whose
/// old and new paths do not match `paths`.
///
/// # Parameters
///
/// - `odb` — object database used to read commit, tree, and blob objects.
/// - `commit_oid` — OID of the commit to compute the patch-ID for.
/// - `paths` — pathspec strings used to limit the files included in the diff.
///
/// # Errors
///
/// Returns errors from object-database reads or object-parse failures.
pub fn compute_patch_id_for_paths(
    odb: &Odb,
    commit_oid: &ObjectId,
    paths: &[String],
) -> Result<Option<ObjectId>> {
    compute_patch_id_filtered(odb, commit_oid, paths)
}

fn compute_patch_id_filtered(
    odb: &Odb,
    commit_oid: &ObjectId,
    paths: &[String],
) -> Result<Option<ObjectId>> {
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

    // Compute tree-to-tree diff.
    let mut diffs = diff_trees(odb, parent_tree_oid.as_ref(), Some(&commit.tree), "")?;
    if !paths.is_empty() {
        diffs.retain(|entry| diff_entry_matches_paths(entry, paths));
    }

    // Sort by primary path (lexicographic), matching diffcore_std ordering.
    diffs.sort_by(|a, b| a.path().cmp(b.path()));

    let mut result = [0u8; 20];

    for entry in &diffs {
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

        let mut ctx = patch_id_hasher();
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

        text_flush_one_hunk(&mut result, &mut ctx);
    }

    ObjectId::from_bytes(&result).map(Some)
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

fn patch_id_add_string(ctx: &mut ObjectHasher, s: &[u8]) {
    ctx.update(s);
}

fn patch_id_add_mode(ctx: &mut ObjectHasher, mode: u32) {
    let text = format!("{mode:06o}");
    ctx.update(text.as_bytes());
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
        let grit_ids = compute_patch_ids_from_text(log_p.as_bytes(), PatchIdMode::Stable);

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
}
