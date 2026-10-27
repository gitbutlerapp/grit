//! Shell driver scripts materialized in the benchmark worktree on each prepare.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::history_rewrite_fixture::HistoryRewriteMeta;

/// Write `.grit-bench/*.sh` helpers used as the git-side rebase baselines.
pub fn write_benchmark_scripts(work: &Path, meta: &HistoryRewriteMeta) -> Result<()> {
    let dir = work.join(".grit-bench");
    fs::create_dir_all(&dir).context("mkdir .grit-bench")?;
    let msg = meta.bench_message.trim();
    fs::write(work.join(".grit-bench-reword-msg"), format!("{msg}\n"))
        .context("write reword msg")?;

    let reword_parent = format!("{}~1", meta.reword);
    let drop_parent = format!("{}~1", meta.drop);
    let drop_tip = "HEAD".to_string();
    let fixup_parent = format!("{}~1", meta.fixup);

    fs::write(
        dir.join("git-reword-rebase.sh"),
        format!(
            r#"#!/bin/sh
set -eu
export GIT_SEQUENCE_EDITOR="{seq}"
export GIT_EDITOR="{editor}"
git rebase -i {reword_parent}
"#,
            seq = work.join(".grit-bench-seq-reword.sh").display(),
            editor = work.join(".grit-bench-git-editor.sh").display(),
            reword_parent = reword_parent,
        ),
    )?;
    fs::write(
        work.join(".grit-bench-seq-reword.sh"),
        // BSD sed (macOS) needs a suffix argument after `-i`; write through a temp file instead.
        "#!/bin/sh\nset -eu\nsed '1s/^pick/reword/' \"$1\" > \"$1.grit-bench\"\nmv \"$1.grit-bench\" \"$1\"\n",
    )?;
    fs::write(
        work.join(".grit-bench-git-editor.sh"),
        format!(
            "#!/bin/sh\ncp {} \"$1\"\n",
            work.join(".grit-bench-reword-msg").display()
        ),
    )?;

    fs::write(
        dir.join("git-drop-rebase.sh"),
        format!(
            r#"#!/bin/sh
set -eu
git rebase --onto {drop_parent} {drop} {drop_tip}
"#,
            drop_parent = drop_parent,
            drop = meta.drop,
            drop_tip = drop_tip,
        ),
    )?;

    fs::write(
        dir.join("git-fixup-rebase.sh"),
        format!(
            r#"#!/bin/sh
set -eu
git commit --fixup {fixup} -m "bench fixup" >/dev/null
export GIT_SEQUENCE_EDITOR=true
git rebase -i --autosquash {fixup_parent}
"#,
            fixup = meta.fixup,
            fixup_parent = fixup_parent,
        ),
    )?;

    fs::write(
        dir.join("git-squash-rebase.sh"),
        format!(
            r#"#!/bin/sh
set -eu
git reset --soft {parent}
git commit -m "$(cat .grit-bench-reword-msg)"
"#,
            parent = meta.squash_rebase_parent,
        ),
    )?;

    for path in [
        dir.join("git-reword-rebase.sh"),
        dir.join("git-drop-rebase.sh"),
        dir.join("git-fixup-rebase.sh"),
        dir.join("git-squash-rebase.sh"),
        work.join(".grit-bench-seq-reword.sh"),
        work.join(".grit-bench-git-editor.sh"),
    ] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}
