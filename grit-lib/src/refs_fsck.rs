//! Reference database consistency checks for `git refs verify` and `git fsck --references`.
//!
//! Aligns with Git's `refs_fsck` / `files_fsck_*` and `packed_fsck` behavior and message text.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::ConfigSet;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::repo::Repository;

/// Severity of a refs-fsck diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefsFsckSeverity {
    Error,
    Warning,
}

/// One diagnostic (use [`format_refs_fsck_line`] for Git-compatible output).
#[derive(Debug, Clone)]
pub struct RefsFsckIssue {
    pub severity: RefsFsckSeverity,
    pub path: String,
    pub msg_id: &'static str,
    pub detail: String,
}

/// `error: path: msgId: detail` / `warning: ...`
#[must_use]
pub fn format_refs_fsck_line(issue: &RefsFsckIssue) -> String {
    let level = match issue.severity {
        RefsFsckSeverity::Error => "error",
        RefsFsckSeverity::Warning => "warning",
    };
    format!(
        "{}: {}: {}: {}",
        level, issue.path, issue.msg_id, issue.detail
    )
}

fn canonical_git_dir(git_dir: &Path) -> PathBuf {
    let commondir_file = git_dir.join("commondir");
    let Some(raw) = fs::read_to_string(commondir_file).ok() else {
        return git_dir.to_path_buf();
    };
    let rel = raw.trim();
    if rel.is_empty() {
        return git_dir.to_path_buf();
    }
    let path = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        git_dir.join(rel)
    };
    path.canonicalize().unwrap_or(path)
}

fn is_pseudo_ref(name: &str) -> bool {
    matches!(name, "FETCH_HEAD" | "MERGE_HEAD" | "ORIG_HEAD")
}

fn is_root_ref_syntax(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'-' || b == b'_')
}

/// Matches Git's `is_root_ref` closely enough for fsck root ref enumeration.
fn is_root_ref(name: &str) -> bool {
    if !is_root_ref_syntax(name) || is_pseudo_ref(name) {
        return false;
    }
    if name.ends_with("_HEAD") {
        return true;
    }
    matches!(
        name,
        "HEAD"
            | "AUTO_MERGE"
            | "BISECT_EXPECTED_REV"
            | "NOTES_MERGE_PARTIAL"
            | "NOTES_MERGE_REF"
            | "MERGE_AUTOSTASH"
    )
}

fn stripped_for_head_check(display_path: &str) -> &str {
    display_path
        .strip_prefix("worktrees/")
        .and_then(|s| s.find('/').map(|i| &s[i + 1..]))
        .unwrap_or(display_path)
}

fn ref_path_for_name_check(display_path: &str) -> &str {
    if let Some(rest) = display_path.strip_prefix("worktrees/") {
        if let Some(idx) = rest.find("/refs/") {
            return &rest[idx + 1..];
        }
        if rest.ends_with("/HEAD") || rest == "HEAD" {
            return "HEAD";
        }
    }
    display_path
}

fn fsck_refs_msg_severity(
    config: &ConfigSet,
    camel_id: &str,
    default_warn: bool,
    strict: bool,
) -> Option<RefsFsckSeverity> {
    let key = format!("fsck.{camel_id}");
    let v = config.get(&key).map(|s| s.to_ascii_lowercase());
    if matches!(v.as_deref(), Some("ignore")) {
        return None;
    }
    let level = match v.as_deref() {
        Some("warn") => RefsFsckSeverity::Warning,
        Some("error") => RefsFsckSeverity::Error,
        _ => {
            if default_warn {
                if strict {
                    RefsFsckSeverity::Error
                } else {
                    RefsFsckSeverity::Warning
                }
            } else {
                RefsFsckSeverity::Error
            }
        }
    };
    Some(level)
}

fn push_issue(
    issues: &mut Vec<RefsFsckIssue>,
    config: &ConfigSet,
    strict: bool,
    camel_id: &'static str,
    default_warn: bool,
    path: String,
    detail: String,
) {
    let Some(sev) = fsck_refs_msg_severity(config, camel_id, default_warn, strict) else {
        return;
    };
    issues.push(RefsFsckIssue {
        severity: sev,
        path,
        msg_id: camel_id,
        detail,
    });
}

/// Run ref database checks (files backend + packed-refs). `strict` is `git refs verify --strict`.
pub fn refs_fsck(
    repo: &Repository,
    odb: &Odb,
    config: &ConfigSet,
    strict: bool,
) -> io::Result<Vec<RefsFsckIssue>> {
    let mut issues = Vec::new();
    let common = canonical_git_dir(&repo.git_dir);

    let mut stores: Vec<(PathBuf, Option<String>)> = vec![(common.clone(), None)];
    let worktrees_dir = common.join("worktrees");
    if let Ok(rd) = fs::read_dir(&worktrees_dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let id = e.file_name().to_string_lossy().to_string();
                stores.push((e.path(), Some(id)));
            }
        }
    }

    for (git_dir, wt_id) in stores {
        fsck_worktree(
            &git_dir,
            wt_id.as_deref(),
            &common,
            odb,
            config,
            strict,
            &mut issues,
        )?;
    }

    // Preserve discovery order (matches Git). Do not sort: message order matters for the same
    // path (e.g. `symlinkRef` before `badReferentName`), and `packed-refs line N` sorts
    // incorrectly as strings (`line 10` before `line 2`). Aggregate tests use `sort` on output.
    Ok(issues)
}

fn fsck_worktree(
    git_dir: &Path,
    worktree_id: Option<&str>,
    common_dir: &Path,
    odb: &Odb,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) -> io::Result<()> {
    let refs_dir = git_dir.join("refs");
    if refs_dir.is_dir() {
        walk_refs_files(common_dir, &refs_dir, odb, config, strict, issues)?;
    }

    if worktree_id.is_none() {
        fsck_packed_refs(common_dir, config, strict, issues)?;
    }

    fsck_root_refs(
        git_dir,
        common_dir,
        path_prefix_for_root(worktree_id),
        odb,
        config,
        strict,
        issues,
    )?;
    Ok(())
}

fn path_prefix_for_root(worktree_id: Option<&str>) -> Option<String> {
    worktree_id.map(|id| format!("worktrees/{id}/"))
}

fn display_rel_path(common_dir: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(common_dir).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Resolve `base.join(rel)` with `..` collapsed (Git-style symlink target handling when the
/// destination path does not exist and `canonicalize` fails).
fn normalize_joined_path(base: &Path, rel: &Path) -> PathBuf {
    let combined = base.join(rel);
    let mut out = PathBuf::new();
    for comp in combined.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    out
}

fn walk_refs_files(
    common_dir: &Path,
    dir: &Path,
    odb: &Odb,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == "." || fname == ".." {
            continue;
        }
        if path.is_dir() {
            walk_refs_files(common_dir, &path, odb, config, strict, issues)?;
            continue;
        }
        if !path.is_file() && !path.is_symlink() {
            continue;
        }
        if !fname.starts_with('.') && fname.ends_with(".lock") {
            continue;
        }
        let display = display_rel_path(common_dir, &path);
        verify_loose_ref(common_dir, &display, &path, odb, config, strict, issues)?;
    }
    Ok(())
}

fn fsck_root_refs(
    git_dir: &Path,
    common_dir: &Path,
    path_prefix: Option<String>,
    odb: &Odb,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) -> io::Result<()> {
    let Ok(rd) = fs::read_dir(git_dir) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if !name.starts_with('.') && name.ends_with(".lock") {
            continue;
        }
        if !is_root_ref(&name) {
            continue;
        }
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() && !meta.is_symlink() {
            continue;
        }
        let display = match &path_prefix {
            Some(p) => format!("{p}{name}"),
            None => name,
        };
        verify_loose_ref(common_dir, &display, &path, odb, config, strict, issues)?;
    }
    Ok(())
}

fn verify_loose_ref(
    common_dir: &Path,
    display_path: &str,
    path: &Path,
    odb: &Odb,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) -> io::Result<()> {
    check_ref_file_name(display_path, config, strict, issues);

    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() && !meta.is_symlink() {
        push_issue(
            issues,
            config,
            strict,
            "badRefFiletype",
            false,
            display_path.to_owned(),
            "unexpected file type".to_owned(),
        );
        return Ok(());
    }

    if meta.is_symlink() {
        push_issue(
            issues,
            config,
            strict,
            "symlinkRef",
            true,
            display_path.to_owned(),
            "use deprecated symbolic link for symref".to_owned(),
        );
        let target = fs::read_link(path)?;
        let parent = path.parent().unwrap_or(Path::new(""));
        let joined = normalize_joined_path(parent, Path::new(&target));
        let resolved = fs::canonicalize(&joined).unwrap_or(joined);
        let abs_common = fs::canonicalize(common_dir).unwrap_or(common_dir.to_path_buf());
        let g = abs_common.to_string_lossy();
        let r = resolved.to_string_lossy().to_string();
        let referent = if r.starts_with(g.as_ref()) {
            let rest = &r[g.len()..];
            rest.trim_start_matches(['/', '\\']).replace('\\', "/")
        } else {
            r.replace('\\', "/")
        };
        refs_fsck_symref(display_path, &referent, config, strict, issues);
        return Ok(());
    }

    let raw = fs::read_to_string(path)?;
    let buf = raw.strip_suffix('\r').unwrap_or(&raw);

    if let Some(after) = buf.strip_prefix("ref:") {
        let mut s = after;
        while s
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            s = &s[1..];
        }
        fsck_symref_contents(display_path, s, config, strict, issues);
        return Ok(());
    }

    let bytes = buf.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
        i += 1;
    }
    // A valid ref points at a full hex OID (40 chars for SHA-1, 64 for SHA-256).
    if !ObjectId::is_hex_len(i) {
        push_issue(
            issues,
            config,
            strict,
            "badRefContent",
            false,
            display_path.to_owned(),
            buf.trim_end_matches(['\n', '\r']).to_owned(),
        );
        return Ok(());
    }
    let oid: ObjectId = match buf[..i].parse() {
        Ok(o) => o,
        Err(_) => {
            push_issue(
                issues,
                config,
                strict,
                "badRefContent",
                false,
                display_path.to_owned(),
                buf.trim_end_matches(['\n', '\r']).to_owned(),
            );
            return Ok(());
        }
    };
    let trailing = &buf[i..];
    if !trailing.is_empty()
        && !trailing
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_whitespace())
    {
        push_issue(
            issues,
            config,
            strict,
            "badRefContent",
            false,
            display_path.to_owned(),
            buf.trim_end_matches(['\n', '\r']).to_owned(),
        );
        return Ok(());
    }

    if trailing.is_empty() {
        push_issue(
            issues,
            config,
            strict,
            "refMissingNewline",
            true,
            display_path.to_owned(),
            "misses LF at the end".to_owned(),
        );
    } else if trailing != "\n" {
        // Git: warn when `*trailing != '\n' || *(trailing + 1)` — only a lone `\n` after the oid
        // is valid; anything else (including `\n\n\n`) is reported with the full tail string.
        push_issue(
            issues,
            config,
            strict,
            "trailingRefContent",
            true,
            display_path.to_owned(),
            format!("has trailing garbage: '{trailing}'"),
        );
    }

    if oid.is_zero() {
        push_issue(
            issues,
            config,
            strict,
            "badRefOid",
            false,
            display_path.to_owned(),
            format!("points to invalid object ID '{}'", oid.to_hex()),
        );
    } else if !odb.exists(&oid) {
        push_issue(
            issues,
            config,
            strict,
            "missingObject",
            false,
            display_path.to_owned(),
            format!("points to missing object {}", oid.to_hex()),
        );
    }

    Ok(())
}

fn check_ref_file_name(
    display_path: &str,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) {
    let check_path = ref_path_for_name_check(display_path);
    if is_root_ref(check_path) || check_path == "HEAD" {
        return;
    }
    if check_refname_format(
        check_path,
        &RefNameOptions {
            allow_onelevel: false,
            refspec_pattern: false,
            normalize: false,
        },
    )
    .is_err()
    {
        push_issue(
            issues,
            config,
            strict,
            "badRefName",
            false,
            display_path.to_owned(),
            "invalid refname format".to_owned(),
        );
    }
}

fn fsck_symref_contents(
    display_path: &str,
    referent_raw: &str,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) {
    // Match `files_fsck_symref_target` + `strbuf_rtrim` (trim trailing ASCII whitespace).
    let orig_len = referent_raw.len();
    let orig_last_byte = referent_raw.as_bytes().last().copied();
    let trimmed = referent_raw.trim_end_matches(|c: char| c.is_ascii_whitespace());
    let after_len = trimmed.len();

    if after_len == orig_len || (after_len < orig_len && orig_last_byte != Some(b'\n')) {
        push_issue(
            issues,
            config,
            strict,
            "refMissingNewline",
            true,
            display_path.to_owned(),
            "misses LF at the end".to_owned(),
        );
    }
    if after_len != orig_len && after_len != orig_len.saturating_sub(1) {
        push_issue(
            issues,
            config,
            strict,
            "trailingRefContent",
            true,
            display_path.to_owned(),
            "has trailing whitespaces or newlines".to_owned(),
        );
    }

    refs_fsck_symref(display_path, trimmed, config, strict, issues);
}

fn refs_fsck_symref(
    display_path: &str,
    target: &str,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) {
    let stripped = stripped_for_head_check(display_path);
    if stripped == "HEAD" && !target.starts_with("refs/heads/") {
        push_issue(
            issues,
            config,
            strict,
            "badHeadTarget",
            false,
            display_path.to_owned(),
            format!("HEAD points to non-branch '{target}'"),
        );
    }

    if is_root_ref(target) {
        return;
    }

    if check_refname_format(
        target,
        &RefNameOptions {
            allow_onelevel: false,
            refspec_pattern: false,
            normalize: false,
        },
    )
    .is_err()
    {
        push_issue(
            issues,
            config,
            strict,
            "badReferentName",
            false,
            display_path.to_owned(),
            format!("points to invalid refname '{target}'"),
        );
        return;
    }

    if !target.starts_with("refs/") && !target.starts_with("worktrees/") {
        push_issue(
            issues,
            config,
            strict,
            "symrefTargetIsNotARef",
            true,
            display_path.to_owned(),
            format!("points to non-ref target '{target}'"),
        );
    }
}

fn cmp_packed_refname(r1: &str, r2: &str) -> Ordering {
    let b1 = r1.as_bytes();
    let b2 = r2.as_bytes();
    let mut i = 0;
    loop {
        let c1 = b1.get(i).copied();
        let c2 = b2.get(i).copied();
        match (c1, c2) {
            (None, None) => return Ordering::Equal,
            (Some(b'\n'), None) => return Ordering::Less,
            (None, Some(b'\n')) => return Ordering::Greater,
            (Some(b'\n'), Some(b'\n')) => return Ordering::Equal,
            (Some(b'\n'), _) => return Ordering::Less,
            (_, Some(b'\n')) => return Ordering::Greater,
            (Some(a), Some(b)) if a != b => return a.cmp(&b),
            (Some(_), Some(_)) => i += 1,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

fn fsck_packed_refs(
    common_dir: &Path,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<RefsFsckIssue>,
) -> io::Result<()> {
    let path = crate::refs::store::paths::packed_refs_path(common_dir);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_symlink() {
        push_issue(
            issues,
            config,
            strict,
            "badRefFiletype",
            false,
            "packed-refs".to_owned(),
            "not a regular file but a symlink".to_owned(),
        );
        return Ok(());
    }
    if !meta.is_file() {
        push_issue(
            issues,
            config,
            strict,
            "badRefFiletype",
            false,
            "packed-refs".to_owned(),
            "not a regular file".to_owned(),
        );
        return Ok(());
    }
    let data = fs::read(&path)?;
    if data.is_empty() {
        push_issue(
            issues,
            config,
            strict,
            "emptyPackedRefsFile",
            true,
            "packed-refs".to_owned(),
            "file is empty".to_owned(),
        );
        return Ok(());
    }

    let text = String::from_utf8_lossy(&data).into_owned();
    let mut sorted = false;
    let mut main_ref_order: Vec<(usize, String)> = Vec::new();

    for (line_no, raw_line) in text.lines().enumerate() {
        let line_no = line_no + 1;
        let line = raw_line.trim_end_matches('\r');

        if line_no == 1 && line.starts_with('#') {
            if line.starts_with("# pack-refs with: ") {
                let traits = line
                    .strip_prefix("# pack-refs with: ")
                    .unwrap_or("")
                    .split_whitespace();
                sorted = traits.clone().any(|t| t == "sorted");
            } else if line.contains("pack-refs") {
                push_issue(
                    issues,
                    config,
                    strict,
                    "badPackedRefHeader",
                    false,
                    "packed-refs.header".to_owned(),
                    format!("'{line}' does not start with '# pack-refs with: '"),
                );
            }
            continue;
        }

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(inner) = line.strip_prefix('^') {
            let mut j = 0usize;
            while j < inner.len() && inner.as_bytes()[j].is_ascii_hexdigit() {
                j += 1;
            }
            if !ObjectId::is_hex_len(j) {
                push_issue(
                    issues,
                    config,
                    strict,
                    "badPackedRefEntry",
                    false,
                    format!("packed-refs line {line_no}"),
                    format!("'{inner}' has invalid peeled oid"),
                );
            } else if j < inner.len() {
                push_issue(
                    issues,
                    config,
                    strict,
                    "badPackedRefEntry",
                    false,
                    format!("packed-refs line {line_no}"),
                    format!("has trailing garbage after peeled oid '{}'", &inner[j..]),
                );
            }
            continue;
        }

        let mut j = 0usize;
        while j < line.len() && line.as_bytes()[j].is_ascii_hexdigit() {
            j += 1;
        }
        let oid_hex = &line[..j];
        let rest = &line[j..];

        if !ObjectId::is_hex_len(oid_hex.len()) {
            let display_line = format!("{oid_hex}{rest}");
            push_issue(
                issues,
                config,
                strict,
                "badPackedRefEntry",
                false,
                format!("packed-refs line {line_no}"),
                format!("'{display_line}' has invalid oid"),
            );
            continue;
        }

        if rest.is_empty()
            || !rest
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            push_issue(
                issues,
                config,
                strict,
                "badPackedRefEntry",
                false,
                format!("packed-refs line {line_no}"),
                format!(
                    "has no space after oid '{oid_hex}' but with '{}'",
                    rest.trim_end_matches('\r')
                ),
            );
            continue;
        }

        // Skip the single separator whitespace after the oid (Git: `p++` after `isspace`).
        let rest = rest.trim_end_matches('\r');
        let refname = match rest.chars().next() {
            Some(c) if c.is_whitespace() => &rest[c.len_utf8()..],
            _ => rest,
        };

        if check_refname_format(
            refname,
            &RefNameOptions {
                allow_onelevel: false,
                refspec_pattern: false,
                normalize: false,
            },
        )
        .is_err()
        {
            push_issue(
                issues,
                config,
                strict,
                "badRefName",
                false,
                format!("packed-refs line {line_no}"),
                format!("has bad refname '{refname}'"),
            );
        }

        main_ref_order.push((line_no, refname.to_owned()));
    }

    if sorted && main_ref_order.len() >= 2 {
        let mut former: Option<&str> = None;
        for (line_no, refname) in &main_ref_order {
            if let Some(prev) = former {
                if cmp_packed_refname(refname, prev) != Ordering::Greater {
                    push_issue(
                        issues,
                        config,
                        strict,
                        "packedRefUnsorted",
                        false,
                        format!("packed-refs line {line_no}"),
                        format!("refname '{refname}' is less than previous refname '{prev}'"),
                    );
                    break;
                }
            }
            former = Some(refname.as_str());
        }
    }

    Ok(())
}
