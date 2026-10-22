//! Apply parsed unified-diff hunks to a single blob (no worktree or index I/O).

use super::{Hunk, HunkLine};
use crate::error::ApplyError;

/// Options controlling blob-level hunk application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// When true, interpret the patch as reversing a forward diff (`+`/`-` swapped).
    pub reverse: bool,
}

/// One logical line in a blob, including a trailing `\n` in `bytes` when present in the file.
#[derive(Debug, Clone)]
struct ImageLine {
    bytes: Vec<u8>,
    patched: bool,
}

/// Line-oriented view of blob bytes (Git `apply.c` `struct image`).
#[derive(Debug, Clone)]
struct Image {
    lines: Vec<ImageLine>,
}

impl Image {
    fn from_blob(data: &[u8]) -> Self {
        if data.is_empty() {
            return Self { lines: Vec::new() };
        }
        let mut lines = Vec::new();
        let mut start = 0usize;
        for (i, &b) in data.iter().enumerate() {
            if b == b'\n' {
                lines.push(ImageLine {
                    bytes: data[start..=i].to_vec(),
                    patched: false,
                });
                start = i + 1;
            }
        }
        if start < data.len() {
            lines.push(ImageLine {
                bytes: data[start..].to_vec(),
                patched: false,
            });
        }
        Self { lines }
    }

    fn to_blob(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for line in &self.lines {
            out.extend_from_slice(&line.bytes);
        }
        out
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// Pre/post images for one hunk plus Git-style leading/trailing context counts.
#[derive(Debug)]
struct HunkImages {
    preimage: Image,
    postimage: Image,
    leading: usize,
    trailing: usize,
}

/// Apply `hunks` to `original` and return the post-image bytes.
///
/// Matches `git apply` default behavior: exact line match at the hunk's old position,
/// then search outward; context lines may be peeled when no match is found. No fuzz or
/// whitespace fixing.
pub fn apply_hunks_to_blob(
    original: &[u8],
    hunks: &[Hunk],
    opts: &ApplyOptions,
) -> Result<Vec<u8>, ApplyError> {
    let mut img = Image::from_blob(original);
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let images = build_hunk_images(hunk, opts.reverse)?;
        let applied = apply_one_hunk(&mut img, &images, hunk, hunk_index)?;
        if !applied {
            return Err(ApplyError::ContextMismatch {
                hunk_index,
                expected_line: hunk.old_start,
            });
        }
    }
    Ok(img.to_blob())
}

fn build_hunk_images(hunk: &Hunk, reverse: bool) -> Result<HunkImages, ApplyError> {
    let mut preimage_buf = Vec::new();
    let mut postimage_buf = Vec::new();
    let mut leading = 0usize;
    let mut trailing = 0usize;
    let mut saw_change = false;

    let mut i = 0usize;
    while i < hunk.lines.len() {
        let (kind, text) = match &hunk.lines[i] {
            HunkLine::Context(s) => (' ', s.as_str()),
            HunkLine::Add(s) => ('+', s.as_str()),
            HunkLine::Remove(s) => ('-', s.as_str()),
            HunkLine::NoNewline => {
                i += 1;
                continue;
            }
        };
        let mut kind = kind;
        if reverse {
            kind = match kind {
                '+' => '-',
                '-' => '+',
                other => other,
            };
        }

        let ends_with_newline = !matches!(hunk.lines.get(i + 1), Some(HunkLine::NoNewline));
        let line_bytes = line_bytes_from_hunk(text, ends_with_newline);

        match kind {
            ' ' => {
                if !saw_change {
                    leading += 1;
                }
                trailing += 1;
                preimage_buf.extend_from_slice(&line_bytes);
                postimage_buf.extend_from_slice(&line_bytes);
            }
            '-' => {
                saw_change = true;
                trailing = 0;
                preimage_buf.extend_from_slice(&line_bytes);
            }
            '+' => {
                saw_change = true;
                trailing = 0;
                postimage_buf.extend_from_slice(&line_bytes);
            }
            _ => {}
        }
        i += 1;
    }

    if !saw_change {
        trailing = leading;
    }

    Ok(HunkImages {
        preimage: Image::from_blob(&preimage_buf),
        postimage: Image::from_blob(&postimage_buf),
        leading,
        trailing,
    })
}

fn line_bytes_from_hunk(text: &str, ends_with_newline: bool) -> Vec<u8> {
    let mut v = text.as_bytes().to_vec();
    if ends_with_newline {
        v.push(b'\n');
    }
    v
}

fn apply_one_hunk(
    img: &mut Image,
    images: &HunkImages,
    hunk: &Hunk,
    hunk_index: usize,
) -> Result<bool, ApplyError> {
    let mut preimage = images.preimage.clone();
    let mut postimage = images.postimage.clone();
    let mut leading = images.leading;
    let mut trailing = images.trailing;

    let mut match_beginning = hunk.old_start <= 1;
    let mut match_end = trailing == 0;

    let mut pos = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start.saturating_sub(1)
    };

    loop {
        if let Some(applied_pos) =
            find_match(img, &preimage, pos, match_beginning, match_end, hunk_index)?
        {
            update_image(img, applied_pos, &preimage, &postimage);
            return Ok(true);
        }

        if leading == 0 && trailing == 0 {
            break;
        }
        if match_beginning || match_end {
            match_beginning = false;
            match_end = false;
            continue;
        }
        if leading >= trailing {
            remove_first_line(&mut preimage);
            remove_first_line(&mut postimage);
            pos = pos.saturating_sub(1);
            leading -= 1;
        }
        if trailing > leading {
            remove_last_line(&mut preimage);
            remove_last_line(&mut postimage);
            trailing -= 1;
        }
    }
    Ok(false)
}

fn find_match(
    img: &Image,
    preimage: &Image,
    start_line: usize,
    match_beginning: bool,
    match_end: bool,
    hunk_index: usize,
) -> Result<Option<usize>, ApplyError> {
    let pre_lines = preimage.lines.len();
    if pre_lines == 0 {
        return Ok(Some(start_line.min(img.line_count())));
    }

    let mut line = start_line;
    if match_beginning {
        line = 0;
    } else if match_end {
        line = img.line_count().saturating_sub(pre_lines);
    }
    if line > img.line_count() {
        line = img.line_count();
    }

    let mut backwards_lno = line;
    let mut forwards_lno = line;
    let mut current_lno = line;

    let max_tries = img.line_count().saturating_add(pre_lines).saturating_add(1);
    for i in 0..max_tries {
        if match_fragment(
            img,
            preimage,
            current_lno,
            match_beginning,
            match_end,
            hunk_index,
        )? {
            return Ok(Some(current_lno));
        }

        let can_back = backwards_lno > 0;
        let can_fwd =
            forwards_lno < img.line_count() && forwards_lno + pre_lines <= img.line_count();
        if !can_back && !can_fwd {
            break;
        }

        if i & 1 == 1 {
            if can_back {
                backwards_lno -= 1;
                current_lno = backwards_lno;
            }
        } else if can_fwd {
            forwards_lno += 1;
            current_lno = forwards_lno;
        }
    }
    Ok(None)
}

fn match_fragment(
    img: &Image,
    preimage: &Image,
    at_line: usize,
    match_beginning: bool,
    match_end: bool,
    hunk_index: usize,
) -> Result<bool, ApplyError> {
    let pre_count = preimage.lines.len();
    if pre_count == 0 {
        return Ok(true);
    }
    if match_beginning && at_line != 0 {
        return Ok(false);
    }
    if match_end && at_line + pre_count != img.line_count() {
        return Ok(false);
    }
    if at_line + pre_count > img.line_count() {
        return Err(ApplyError::UnexpectedEof { hunk_index });
    }

    for (i, pre) in preimage.lines.iter().enumerate() {
        let img_line = &img.lines[at_line + i];
        if img_line.patched {
            return Err(ApplyError::HunksOverlap { hunk_index });
        }
        if img_line.bytes != pre.bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

fn update_image(img: &mut Image, at: usize, preimage: &Image, postimage: &Image) {
    let pre_limit = preimage.lines.len().min(img.lines.len().saturating_sub(at));
    img.lines
        .splice(at..at + pre_limit, postimage.lines.iter().cloned());
    for line in &mut img.lines[at..at + postimage.lines.len()] {
        line.patched = true;
    }
}

fn remove_first_line(image: &mut Image) {
    if !image.lines.is_empty() {
        image.lines.remove(0);
    }
}

fn remove_last_line(image: &mut Image) {
    if !image.lines.is_empty() {
        image.lines.pop();
    }
}
