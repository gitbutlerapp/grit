//! Vimdiff merge tool layout generation compatible with Git's `mergetools/vimdiff` driver.
//!
//! This implements the same vimdiff layout behavior Git provides so `grit mergetool` and tests can
//! share one implementation. Layout strings use only ASCII (`LOCAL`, `BASE`, `REMOTE`, `MERGED`,
//! separators `+`, `/`, `,`, and parentheses).

/// Result of [`vimdiff_gen_cmd`]: the `-c "..."` vim argument body and the save target pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VimdiffGenCmd {
    /// Full `-c "set hidden diffopt-=hiddenoff | ... | tabfirst"` string (as passed to `vim -f`).
    pub final_cmd: String,
    /// Which file receives edits when the tool exits successfully (`LOCAL`, `BASE`, `REMOTE`, or `MERGED`).
    pub final_target: &'static str,
}

/// Computes `FINAL_CMD` and `FINAL_TARGET` from a layout string, matching `gen_cmd` in Git's vimdiff driver.
///
/// # Arguments
///
/// * `layout` — Layout definition (see `git help mergetool`, vimdiff backend).
#[must_use]
pub fn vimdiff_gen_cmd(layout: &str) -> VimdiffGenCmd {
    let final_target = if layout.contains("@LOCAL") {
        "LOCAL"
    } else if layout.contains("@BASE") {
        "BASE"
    } else if layout.contains("@REMOTE") {
        "REMOTE"
    } else {
        "MERGED"
    };

    let mut cmd = String::new();
    for (tab_idx, tab) in layout.split('+').enumerate() {
        if tab_idx == 0 {
            cmd.push_str("echo");
        } else {
            cmd.push_str(" | tabnew");
        }

        if !tab.contains(',') && !tab.contains('/') {
            cmd.push_str(" | silent execute 'bufdo diffthis'");
        }

        cmd = gen_cmd_aux(tab, cmd);
    }

    cmd.push_str(" | execute 'tabdo windo diffthis'");
    let final_cmd = format!("-c \"set hidden diffopt-=hiddenoff | {cmd} | tabfirst\"");

    VimdiffGenCmd {
        final_cmd,
        final_target,
    }
}

/// Resolves the layout string for a merge tool name, matching `merge_cmd` in Git's vimdiff script.
///
/// * `tool` — e.g. `vimdiff`, `gvimdiff2`, `nvimdiff1`.
/// * `mergetool_layout` — value of `mergetool.<tool>.layout` when set.
/// * `vimdiff_layout_fallback` — value of `mergetool.vimdiff.layout` when variant-specific layout is unset.
#[must_use]
pub fn vimdiff_resolve_layout<'a>(
    tool: &str,
    mergetool_layout: Option<&'a str>,
    vimdiff_layout_fallback: Option<&'a str>,
) -> &'a str {
    if let Some(l) = mergetool_layout.filter(|s| !s.is_empty()) {
        return l;
    }
    if let Some(l) = vimdiff_layout_fallback.filter(|s| !s.is_empty()) {
        return l;
    }

    if tool.ends_with("vimdiff1") {
        return "@LOCAL,REMOTE";
    }
    if tool.ends_with("vimdiff2") {
        return "LOCAL,MERGED,REMOTE";
    }
    if tool.ends_with("vimdiff3") {
        return "MERGED";
    }

    if tool.contains("vimdiff") {
        return "(LOCAL,BASE,REMOTE)/MERGED";
    }

    "(LOCAL,BASE,REMOTE)/MERGED"
}

/// Executable name for a vimdiff-family merge tool (`vim`, `gvim`, `nvim`).
#[must_use]
pub fn vimdiff_executable_for_tool(tool: &str) -> Option<&'static str> {
    if tool.starts_with("nvimdiff") {
        return Some("nvim");
    }
    if tool.starts_with("gvimdiff") {
        return Some("gvim");
    }
    if tool.starts_with("vimdiff") {
        return Some("vim");
    }
    None
}

/// When no base version exists, Git rewrites buffer indices in the vim command (`2b` → `quit`, etc.).
#[must_use]
pub fn vimdiff_cmd_without_base(final_cmd: &str) -> String {
    final_cmd
        .replace("2b", "quit")
        .replace("3b", "2b")
        .replace("4b", "3b")
}

fn substring_bytes(s: &str, start: usize, len: usize) -> &str {
    let b = s.as_bytes();
    if start >= b.len() || len == 0 {
        return "";
    }
    let end = (start + len).min(b.len());
    // Layout strings are ASCII-only in Git; slice must be char boundaries.
    s.get(start..end).unwrap_or("")
}

fn gen_cmd_aux(layout: &str, mut cmd: String) -> String {
    let b = layout.as_bytes();
    let mut start = 0usize;
    let mut end = b.len();

    let mut nested = 0i32;
    let mut nested_min = 100i32;
    for &ch in b {
        let c = ch as char;
        if c == ' ' {
            continue;
        }
        if c == '(' {
            nested += 1;
            continue;
        }
        if c == ')' {
            nested -= 1;
            continue;
        }
        nested_min = nested_min.min(nested);
    }

    while nested_min > 0 {
        start += 1;
        end -= 1;
        let mut start_minus_one = start.wrapping_sub(1);
        while start > 0 && substring_bytes(layout, start_minus_one, 1) != "(" {
            start += 1;
            start_minus_one = start.wrapping_sub(1);
        }
        while end > 0 && substring_bytes(layout, end, 1) != ")" {
            end -= 1;
        }
        nested_min -= 1;
    }

    let mut index_horizontal: Option<usize> = None;
    let mut index_vertical: Option<usize> = None;
    let mut nested = 0i32;
    let slice = substring_bytes(layout, start, end.saturating_sub(start));
    for (offset, &ch) in slice.as_bytes().iter().enumerate() {
        let c = ch as char;
        if c == ' ' {
            continue;
        }
        if c == '(' {
            nested += 1;
            continue;
        }
        if c == ')' {
            nested -= 1;
            continue;
        }
        if nested == 0 {
            let idx = start + offset;
            if c == '/' && index_horizontal.is_none() {
                index_horizontal = Some(idx);
            } else if c == ',' && index_vertical.is_none() {
                index_vertical = Some(idx);
            }
        }
    }

    if let Some(index) = index_horizontal {
        let (before, after) = ("leftabove split", "wincmd j");
        cmd.push_str(" | ");
        cmd.push_str(before);
        cmd = gen_cmd_aux(
            substring_bytes(layout, start, index.saturating_sub(start)),
            cmd,
        );
        cmd.push_str(" | ");
        cmd.push_str(after);
        cmd = gen_cmd_aux(
            substring_bytes(layout, index + 1, b.len().saturating_sub(index)),
            cmd,
        );
        return cmd;
    }

    if let Some(index) = index_vertical {
        let (before, after) = ("leftabove vertical split", "wincmd l");
        cmd.push_str(" | ");
        cmd.push_str(before);
        cmd = gen_cmd_aux(
            substring_bytes(layout, start, index.saturating_sub(start)),
            cmd,
        );
        cmd.push_str(" | ");
        cmd.push_str(after);
        cmd = gen_cmd_aux(
            substring_bytes(layout, index + 1, b.len().saturating_sub(index)),
            cmd,
        );
        return cmd;
    }

    let leaf = substring_bytes(layout, start, end.saturating_sub(start));
    let target: String = leaf
        .chars()
        .filter(|c| !matches!(c, ' ' | '@' | '(' | ')' | ';' | '|' | '-'))
        .collect();

    cmd.push_str(" | ");
    cmd.push_str(match target.as_str() {
        "LOCAL" => "1b",
        "BASE" => "2b",
        "REMOTE" => "3b",
        "MERGED" => "4b",
        _ => {
            return format!("{cmd} | ERROR: >{target}<");
        }
    });

    cmd
}

/// Inner script passed to `vim -f -c '<script>'` (without the outer `-c` wrapper).
#[must_use]
pub fn vimdiff_final_cmd_script(final_cmd: &str) -> String {
    final_cmd
        .strip_prefix("-c \"")
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(final_cmd)
        .to_string()
}

/// Builds argv for `vim -f -c '...' LOCAL BASE REMOTE MERGED` (base present), matching Git's `merge_cmd` eval.
#[must_use]
pub fn vimdiff_merge_argv_with_base(
    final_cmd: &str,
    local: &str,
    base: &str,
    remote: &str,
    merged: &str,
) -> Vec<String> {
    vec![
        "-f".to_string(),
        "-c".to_string(),
        vimdiff_final_cmd_script(final_cmd),
        local.to_string(),
        base.to_string(),
        remote.to_string(),
        merged.to_string(),
    ]
}

/// Builds argv when the common ancestor is missing: `LOCAL REMOTE MERGED` only, after [`vimdiff_cmd_without_base`].
#[must_use]
pub fn vimdiff_merge_argv_no_base(
    final_cmd: &str,
    local: &str,
    remote: &str,
    merged: &str,
) -> Vec<String> {
    vec![
        "-f".to_string(),
        "-c".to_string(),
        vimdiff_final_cmd_script(final_cmd),
        local.to_string(),
        remote.to_string(),
        merged.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t7609_vimdiff_gen_cmd_cases() {
        const CASES: &[&str] = &[
            "(LOCAL,BASE,REMOTE)/MERGED",
            "@LOCAL,REMOTE",
            "LOCAL,MERGED,REMOTE",
            "MERGED",
            "LOCAL/MERGED/REMOTE",
            "(LOCAL/REMOTE),MERGED",
            "MERGED,(LOCAL/REMOTE)",
            "(LOCAL,REMOTE)/MERGED",
            "MERGED/(LOCAL,REMOTE)",
            "(LOCAL/BASE/REMOTE),MERGED",
            "(LOCAL,BASE,REMOTE)/MERGED+BASE,LOCAL+BASE,REMOTE+(LOCAL/BASE/REMOTE),MERGED",
            "((LOCAL,REMOTE)/BASE),MERGED",
            "((LOCAL,REMOTE)/BASE),((LOCAL/REMOTE),MERGED)",
            "BASE,REMOTE+BASE,LOCAL",
            "  ((  (LOCAL , BASE , REMOTE) / MERGED))   +(BASE)   , LOCAL+ BASE , REMOTE+ (((LOCAL / BASE / REMOTE)) ,    MERGED   )  ",
            "LOCAL,BASE,REMOTE / MERGED + BASE,LOCAL + BASE,REMOTE + (LOCAL / BASE / REMOTE),MERGED",
            "(LOCAL,@BASE,REMOTE)/MERGED",
            "LOCAL,@REMOTE",
            "@REMOTE",
        ];

        const EXPECTED_CMD: &[&str] = &[
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 2b | wincmd l | 3b | wincmd j | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | 1b | wincmd l | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 4b | wincmd l | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | silent execute 'bufdo diffthis' | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | 1b | wincmd j | leftabove split | 4b | wincmd j | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | leftabove split | 1b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | 4b | wincmd l | leftabove split | 1b | wincmd j | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | 3b | wincmd j | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | 4b | wincmd j | leftabove vertical split | 1b | wincmd l | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | leftabove split | 1b | wincmd j | leftabove split | 2b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 2b | wincmd l | 3b | wincmd j | 4b | tabnew | leftabove vertical split | 2b | wincmd l | 1b | tabnew | leftabove vertical split | 2b | wincmd l | 3b | tabnew | leftabove vertical split | leftabove split | 1b | wincmd j | leftabove split | 2b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | leftabove split | leftabove vertical split | 1b | wincmd l | 3b | wincmd j | 2b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | leftabove split | leftabove vertical split | 1b | wincmd l | 3b | wincmd j | 2b | wincmd l | leftabove vertical split | leftabove split | 1b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | 2b | wincmd l | 3b | tabnew | leftabove vertical split | 2b | wincmd l | 1b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 2b | wincmd l | 3b | wincmd j | 4b | tabnew | leftabove vertical split | 2b | wincmd l | 1b | tabnew | leftabove vertical split | 2b | wincmd l | 3b | tabnew | leftabove vertical split | leftabove split | 1b | wincmd j | leftabove split | 2b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 2b | wincmd l | 3b | wincmd j | 4b | tabnew | leftabove vertical split | 2b | wincmd l | 1b | tabnew | leftabove vertical split | 2b | wincmd l | 3b | tabnew | leftabove vertical split | leftabove split | 1b | wincmd j | leftabove split | 2b | wincmd j | 3b | wincmd l | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | 2b | wincmd l | 3b | wincmd j | 4b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | leftabove vertical split | 1b | wincmd l | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
            "-c \"set hidden diffopt-=hiddenoff | echo | silent execute 'bufdo diffthis' | 3b | execute 'tabdo windo diffthis' | tabfirst\"",
        ];

        const EXPECTED_TARGET: &[&str] = &[
            "MERGED", "LOCAL", "MERGED", "MERGED", "MERGED", "MERGED", "MERGED", "MERGED",
            "MERGED", "MERGED", "MERGED", "MERGED", "MERGED", "MERGED", "MERGED", "MERGED", "BASE",
            "REMOTE", "REMOTE",
        ];

        assert_eq!(CASES.len(), EXPECTED_CMD.len());
        assert_eq!(CASES.len(), EXPECTED_TARGET.len());

        for (i, layout) in CASES.iter().enumerate() {
            let g = vimdiff_gen_cmd(layout);
            assert_eq!(
                g.final_cmd,
                EXPECTED_CMD[i],
                "case {} layout {:?}",
                i + 1,
                layout
            );
            assert_eq!(g.final_target, EXPECTED_TARGET[i], "target case {}", i + 1);
        }
    }

    #[test]
    fn t7609_merge_argv_paths_with_spaces() {
        let g = vimdiff_gen_cmd("(LOCAL,BASE,REMOTE)/MERGED");
        let adjusted = vimdiff_cmd_without_base(&g.final_cmd);
        let argv = vimdiff_merge_argv_no_base(&adjusted, "lo cal", "' '", "mer ged");
        assert_eq!(
            argv,
            vec![
                "-f".to_string(),
                "-c".to_string(),
                "set hidden diffopt-=hiddenoff | echo | leftabove split | leftabove vertical split | 1b | wincmd l | leftabove vertical split | quit | wincmd l | 2b | wincmd j | 3b | execute 'tabdo windo diffthis' | tabfirst".to_string(),
                "lo cal".to_string(),
                "' '".to_string(),
                "mer ged".to_string(),
            ],
            "merge_cmd without base: three path args, single -c string"
        );
    }
}
