//! Shared `--hunks` / `--patch-file` parsing for index patch selection commands.

use anyhow::{bail, Result};
use grit_lib::apply::{parse_patch, FilePatch};
use std::io::Read;
use std::path::Path;

/// Read patch bytes from a path or stdin (`-`).
pub fn read_patch_bytes(patch_file: &str) -> Result<Vec<u8>> {
    if patch_file == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| anyhow::anyhow!("could not read patch from stdin: {e}"))?;
        return Ok(buf);
    }
    std::fs::read(Path::new(patch_file))
        .map_err(|e| anyhow::anyhow!("could not read patch file '{patch_file}': {e}"))
}

/// Parse a unified diff file into [`FilePatch`] rows (same rules as `grit add --patch-file`).
pub fn parse_patch_file(patch_file: &str) -> Result<Vec<FilePatch>> {
    let label = if patch_file == "-" {
        "stdin".to_string()
    } else {
        patch_file.to_string()
    };
    let patch_bytes = read_patch_bytes(patch_file)?;
    let patch_text = std::str::from_utf8(&patch_bytes)
        .map_err(|_| anyhow::anyhow!("patch file must be valid UTF-8"))?;
    parse_patch(patch_text, 1, &label, false, None).map_err(crate::json_error::map_lib_error)
}

/// Ensure at most one of patch-file, hunks, or lines is set.
pub fn ensure_single_patch_mode(patch_file: bool, hunks: bool, lines: bool) -> Result<()> {
    let count = usize::from(patch_file) + usize::from(hunks) + usize::from(lines);
    if count > 1 {
        bail!("use only one of --patch-file, --hunks, or --lines");
    }
    Ok(())
}
