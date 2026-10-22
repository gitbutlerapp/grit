//! Apply parsed [`FilePatch`] hunks to in-memory blob bytes (no I/O).

use super::{BinaryPatchPayload, FilePatch, Hunk, HunkLine};
use crate::error::{ApplyError, Error};

/// Options for [`apply_patch_to_blob`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobApplyOptions {
    /// When true, allow `Binary files … differ` markers with no payload (result unchanged).
    pub allow_binary_marker_only: bool,
}

/// Apply one file patch to `base` blob bytes and return the post-image.
///
/// Binary patches use [`super::materialize_binary_patch_forward`]. Text patches apply
/// unified-diff hunks in reverse line order so hunk headers stay anchored to the preimage.
pub fn apply_patch_to_blob(
    base: &[u8],
    patch: &FilePatch,
    opts: &BlobApplyOptions,
) -> std::result::Result<Vec<u8>, ApplyError> {
    if patch.is_deleted {
        return Ok(Vec::new());
    }
    if let Some(payload) = patch.binary_patch.as_ref() {
        return materialize_binary(base, payload);
    }
    if patch.is_binary {
        if opts.allow_binary_marker_only {
            return Ok(base.to_vec());
        }
        let path = patch.effective_path().unwrap_or("<unknown>").to_string();
        return Err(ApplyError::BinaryDataMissing { path });
    }
    if patch.hunks.is_empty() {
        return Ok(base.to_vec());
    }
    let path = patch.effective_path().unwrap_or("<unknown>").to_string();
    apply_hunks_to_blob(base, &patch.hunks, &path)
}

fn materialize_binary(
    base: &[u8],
    payload: &BinaryPatchPayload,
) -> std::result::Result<Vec<u8>, ApplyError> {
    super::materialize_binary_patch_forward(payload, base).map_err(|e| match e {
        Error::Apply(a) => a,
        other => ApplyError::Failed {
            path: String::new(),
            message: other.to_string(),
        },
    })
}

fn apply_hunks_to_blob(
    base: &[u8],
    hunks: &[Hunk],
    path: &str,
) -> std::result::Result<Vec<u8>, ApplyError> {
    let mut lines = split_blob_lines(base);
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by(|a, b| {
        b.old_start
            .cmp(&a.old_start)
            .then(b.old_count.cmp(&a.old_count))
    });

    for hunk in ordered {
        apply_one_hunk(&mut lines, hunk, path)?;
    }
    Ok(join_blob_lines(&lines))
}

fn apply_one_hunk(
    lines: &mut Vec<Vec<u8>>,
    hunk: &Hunk,
    path: &str,
) -> std::result::Result<(), ApplyError> {
    let (old_side, new_side, ends_without_newline) = hunk_old_new(hunk);
    if old_side.is_empty() {
        let insert_at = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start.saturating_sub(1).min(lines.len())
        };
        lines.splice(insert_at..insert_at, new_side);
        if ends_without_newline {
            trim_trailing_newline_on_last_line(lines);
        }
        return Ok(());
    }

    let pos =
        find_hunk_match(lines, &old_side, hunk.old_start.saturating_sub(1)).ok_or_else(|| {
            ApplyError::PatchFailed {
                path: path.to_string(),
                line: hunk.first_body_line,
            }
        })?;
    lines.splice(pos..pos.saturating_add(old_side.len()), new_side);
    if ends_without_newline {
        trim_trailing_newline_on_last_line(lines);
    }
    Ok(())
}

fn hunk_old_new(hunk: &Hunk) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, bool) {
    let mut old_side = Vec::new();
    let mut new_side = Vec::new();
    let mut ends_without_newline = false;
    for line in &hunk.lines {
        match line {
            HunkLine::Context(s) => {
                let bytes = line_bytes(s, true);
                old_side.push(bytes.clone());
                new_side.push(bytes);
            }
            HunkLine::Remove(s) => old_side.push(line_bytes(s, true)),
            HunkLine::Add(s) => new_side.push(line_bytes(s, true)),
            HunkLine::NoNewline => ends_without_newline = true,
        }
    }
    (old_side, new_side, ends_without_newline)
}

fn line_bytes(text: &str, with_newline: bool) -> Vec<u8> {
    if with_newline {
        let mut v = text.as_bytes().to_vec();
        if v.last() != Some(&b'\n') {
            v.push(b'\n');
        }
        v
    } else {
        text.as_bytes().to_vec()
    }
}

fn trim_trailing_newline_on_last_line(lines: &mut [Vec<u8>]) {
    if let Some(last) = lines.last_mut() {
        if last.last() == Some(&b'\n') {
            last.pop();
        }
    }
}

fn find_hunk_match(lines: &[Vec<u8>], old_side: &[Vec<u8>], hint: usize) -> Option<usize> {
    if old_side.is_empty() {
        return Some(hint.min(lines.len()));
    }
    let search_from = hint.min(lines.len().saturating_sub(old_side.len()));
    if lines
        .get(search_from..search_from.saturating_add(old_side.len()))
        .is_some_and(|window| window == old_side)
    {
        return Some(search_from);
    }
    for pos in 0..=lines.len().saturating_sub(old_side.len()) {
        if lines[pos..pos + old_side.len()] == *old_side {
            return Some(pos);
        }
    }
    None
}

fn split_blob_lines(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            out.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(data[start..].to_vec());
    }
    out
}

fn join_blob_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.iter().flat_map(|l| l.iter().copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::parse_patch;

    #[test]
    fn blob_apply_single_line_change() {
        let patch_text = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-one
+two
";
        let patches = parse_patch(patch_text, 1, "p", false, None).expect("parse");
        let got = apply_patch_to_blob(b"one\n", &patches[0], &BlobApplyOptions::default())
            .expect("apply");
        assert_eq!(got, b"two\n");
    }
}
