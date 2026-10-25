//! Custom and built-in low-level merge drivers (`merge.<name>.driver`, `merge` attributes).
//!
//! Matches Git's `merge-ll.c` / `ll_ext_merge`: external drivers receive temp files for
//! base/ours/theirs, expand `%O/%A/%B/%L/%P/%S/%X/%Y`, and write the result into `%A`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::command_runner::{CommandEnvironment, CommandStdin, CommandStdio};
use crate::config::ConfigSet;
use crate::crlf::{FileAttrs, MergeAttr};
use crate::error::{Error, Result};
use crate::merge_file::{is_binary, merge, ConflictStyle, MergeFavor, MergeInput};
use crate::repo::Repository;
use crate::transport::sq_quote_shell_arg;

/// Resolved merge driver for a path (built-in or external command line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeDriverKind {
    /// Built-in three-way text merge (`merge=text` or default).
    Text,
    /// Built-in binary merge (`merge=binary` / `-merge`).
    Binary,
    /// Built-in union merge (`merge=union`).
    Union,
    /// User-defined `merge.<name>.driver` command.
    External {
        /// Config key suffix (`merge.<name>.driver`).
        name: String,
        /// Raw command template from config.
        cmdline: String,
    },
}

/// Outcome of running a low-level merge driver on one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeDriverOutcome {
    /// Clean merge: merged blob bytes for a stage-0 index entry.
    Clean(Vec<u8>),
    /// Driver reported conflict without a marker blob (external / binary drivers).
    Conflict,
    /// Text merge left conflict markers in `content`.
    ConflictWithMarkers(Vec<u8>),
}

/// Labels passed to `%S`, `%X`, and `%Y` in external driver commands.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MergeDriverLabels<'a> {
    pub base: &'a str,
    pub ours: &'a str,
    pub theirs: &'a str,
}

/// Effective `merge` driver name from attributes and `merge.default`.
#[must_use]
pub(crate) fn effective_merge_driver_name(attrs: &FileAttrs, config: &ConfigSet) -> Option<String> {
    match &attrs.merge {
        MergeAttr::Unset => None,
        MergeAttr::Driver(name) => Some(name.clone()),
        MergeAttr::Unspecified => config.get("merge.default"),
    }
}

/// Resolve a driver name to built-in behaviour or an external command.
#[must_use]
pub(crate) fn resolve_merge_driver(config: &ConfigSet, driver_name: &str) -> MergeDriverKind {
    if let Some(cmdline) = config.get(&format!("merge.{driver_name}.driver")) {
        return MergeDriverKind::External {
            name: driver_name.to_owned(),
            cmdline,
        };
    }
    match driver_name {
        "binary" => MergeDriverKind::Binary,
        "union" => MergeDriverKind::Union,
        "text" => MergeDriverKind::Text,
        _ => MergeDriverKind::Text,
    }
}

/// Expand merge-driver placeholders in `cmdline` (Git `ll_ext_merge` / `strbuf_expand_step`).
#[must_use]
pub(crate) fn expand_merge_driver_command(
    cmdline: &str,
    path: &str,
    marker_size: usize,
    labels: MergeDriverLabels<'_>,
    temp_o: &Path,
    temp_a: &Path,
    temp_b: &Path,
) -> String {
    let base_name = temp_o.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let ours_name = temp_a.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let theirs_name = temp_b.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let mut out = String::with_capacity(cmdline.len() + path.len() + 64);
    let mut chars = cmdline.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('O') => {
                chars.next();
                out.push_str(base_name);
            }
            Some('A') => {
                chars.next();
                out.push_str(ours_name);
            }
            Some('B') => {
                chars.next();
                out.push_str(theirs_name);
            }
            Some('L') => {
                chars.next();
                out.push_str(&marker_size.to_string());
            }
            Some('P') => {
                chars.next();
                out.push_str(&sq_quote_shell_arg(path));
            }
            Some('S') => {
                chars.next();
                out.push_str(&sq_quote_shell_arg(labels.base));
            }
            Some('X') => {
                chars.next();
                out.push_str(&sq_quote_shell_arg(labels.ours));
            }
            Some('Y') => {
                chars.next();
                out.push_str(&sq_quote_shell_arg(labels.theirs));
            }
            _ => out.push('%'),
        }
    }
    out
}

fn write_temp_blob(dir: &Path, prefix: &str, data: &[u8]) -> Result<PathBuf> {
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(dir)
        .map_err(Error::Io)?;
    file.write_all(data).map_err(Error::Io)?;
    file.flush().map_err(Error::Io)?;
    let (_file, path) = file.keep().map_err(|err| Error::Io(err.error))?;
    Ok(path)
}

fn command_cwd(repo: &Repository) -> PathBuf {
    repo.work_tree()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.git_dir().to_path_buf())
}

fn command_env(repo: &Repository) -> CommandEnvironment {
    CommandEnvironment::from_repository_environment(repo.environment(), &[])
}

/// Run an external merge driver; read merged content from the ours temp path (`%A`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_external_merge_driver(
    repo: &Repository,
    cmdline: &str,
    path: &str,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    marker_size: usize,
    labels: MergeDriverLabels<'_>,
) -> Result<MergeDriverOutcome> {
    let temp_dir = command_cwd(repo);
    let temp_o = write_temp_blob(&temp_dir, ".merge_file_", base)?;
    let temp_a = write_temp_blob(&temp_dir, ".merge_file_", ours)?;
    let temp_b = write_temp_blob(&temp_dir, ".merge_file_", theirs)?;

    let script = expand_merge_driver_command(
        cmdline,
        path,
        marker_size,
        labels,
        &temp_o,
        &temp_a,
        &temp_b,
    );
    let runner = repo.command_runner();
    let cwd = command_cwd(repo);
    let mut spec = crate::command_runner::CommandSpec::sh_dash_c_filter(script);
    spec.cwd = Some(cwd);
    spec.env = command_env(repo);
    spec.stdin = CommandStdin::Null;
    spec.stdout = CommandStdio::Null;
    spec.stderr = CommandStdio::Inherit;

    let status = match runner.spawn(&spec) {
        Ok(child) => child.wait_with_output().map_err(Error::Io)?,
        Err(source) => {
            let _ = fs::remove_file(&temp_o);
            let _ = fs::remove_file(&temp_a);
            let _ = fs::remove_file(&temp_b);
            return Err(Error::Io(source));
        }
    };

    let merged = fs::read(&temp_a).map_err(Error::Io)?;
    let _ = fs::remove_file(&temp_o);
    let _ = fs::remove_file(&temp_a);
    let _ = fs::remove_file(&temp_b);

    if status.status.success {
        Ok(MergeDriverOutcome::Clean(merged))
    } else if status.status.code.is_some_and(|c| c <= 128) {
        Ok(MergeDriverOutcome::Conflict)
    } else {
        Err(Error::Io(std::io::Error::other(format!(
            "merge driver terminated with exit {:?}",
            status.status.code
        ))))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_builtin_text_merge(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    favor: MergeFavor,
    style: ConflictStyle,
    marker_size: usize,
    diff_algorithm: Option<&str>,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    label_ours: &str,
    label_base: &str,
    label_theirs: &str,
) -> Result<(MergeDriverOutcome, Vec<u8>)> {
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return Ok(match favor {
            MergeFavor::Ours => (MergeDriverOutcome::Clean(ours.to_vec()), ours.to_vec()),
            MergeFavor::Theirs => (MergeDriverOutcome::Clean(theirs.to_vec()), theirs.to_vec()),
            _ => (MergeDriverOutcome::Conflict, ours.to_vec()),
        });
    }
    let input = MergeInput {
        base,
        ours,
        theirs,
        label_ours,
        label_base,
        label_theirs,
        favor,
        style,
        marker_size,
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
    };
    let result = merge(&input)?;
    let outcome = if result.conflicts > 0 {
        MergeDriverOutcome::ConflictWithMarkers(result.content.clone())
    } else {
        MergeDriverOutcome::Clean(result.content.clone())
    };
    Ok((outcome, result.content))
}

/// Try a configured merge driver when attributes name one (or `merge.default` applies).
///
/// Returns `None` when the path should use the legacy in-tree merge path (no driver name).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_merge_driver(
    repo: &Repository,
    attrs: &FileAttrs,
    path: &str,
    base: Option<&[u8]>,
    ours: &[u8],
    theirs: &[u8],
    favor: MergeFavor,
    style: ConflictStyle,
    diff_algorithm: Option<&str>,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    labels: MergeDriverLabels<'_>,
) -> Result<Option<(MergeDriverOutcome, Vec<u8>)>> {
    let config = repo.config()?;
    let Some(driver_name) = effective_merge_driver_name(attrs, config.as_ref()) else {
        return Ok(None);
    };
    if driver_name == "union" && favor == MergeFavor::None {
        return Ok(None);
    }
    let kind = resolve_merge_driver(config.as_ref(), &driver_name);
    let marker_size = attrs.effective_conflict_marker_size();
    let base_bytes = base.unwrap_or(b"");
    match kind {
        MergeDriverKind::Text => Ok(None),
        MergeDriverKind::Union if favor == MergeFavor::None => Ok(None),
        MergeDriverKind::Binary => Ok(Some((
            match favor {
                MergeFavor::Ours => MergeDriverOutcome::Clean(ours.to_vec()),
                MergeFavor::Theirs => MergeDriverOutcome::Clean(theirs.to_vec()),
                _ => MergeDriverOutcome::Conflict,
            },
            match favor {
                MergeFavor::Theirs => theirs.to_vec(),
                _ => ours.to_vec(),
            },
        ))),
        MergeDriverKind::Union => run_builtin_text_merge(
            base_bytes,
            ours,
            theirs,
            MergeFavor::Union,
            style,
            marker_size,
            diff_algorithm,
            ignore_all_space,
            ignore_space_change,
            ignore_space_at_eol,
            ignore_cr_at_eol,
            labels.ours,
            labels.base,
            labels.theirs,
        )
        .map(Some),
        MergeDriverKind::External { cmdline, .. } => {
            let outcome = run_external_merge_driver(
                repo,
                &cmdline,
                path,
                base_bytes,
                ours,
                theirs,
                marker_size,
                labels,
            )?;
            let content = match &outcome {
                MergeDriverOutcome::Clean(c) => c.clone(),
                MergeDriverOutcome::Conflict | MergeDriverOutcome::ConflictWithMarkers(_) => {
                    ours.to_vec()
                }
            };
            Ok(Some((outcome, content)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_merge_driver_placeholders() {
        let o = Path::new("/tmp/.merge_file_aaa");
        let a = Path::new("/tmp/.merge_file_bbb");
        let b = Path::new("/tmp/.merge_file_ccc");
        let labels = MergeDriverLabels {
            base: "base",
            ours: "HEAD",
            theirs: "side",
        };
        let cmd = "echo %O %A %B %L %P %S %X %Y %%";
        let expanded = expand_merge_driver_command(cmd, "conf.cfg", 7, labels, o, a, b);
        assert!(expanded.contains(".merge_file_aaa"));
        assert!(expanded.contains(".merge_file_bbb"));
        assert!(expanded.contains(".merge_file_ccc"));
        assert!(expanded.contains(" 7 "));
        assert!(expanded.contains(&sq_quote_shell_arg("conf.cfg")));
        assert!(expanded.contains(&sq_quote_shell_arg("HEAD")));
        assert!(expanded.ends_with(" %"));
    }
}
