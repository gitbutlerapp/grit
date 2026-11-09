//! `grit blame` — show who last modified each line of a file.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use std::fmt;

/// Invalid `grit blame` arguments (maps to exit code 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlameUsageError;

use grit_lib::blame::{
    BlameFinalSource, BlameOptions, BlameResult, CopyDetection, IgnoreRevs, IgnoreRevsSources,
};
use grit_lib::diff::WhitespaceMode;
use grit_lib::revwalk::{Peel, ResolveOptions};

impl fmt::Display for BlameUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blame copy detection: at most three -C flags are supported")
    }
}

impl std::error::Error for BlameUsageError {}

use crate::context::{self, relative_date_from};
use crate::output::Person;
use crate::render::blame::BlameDisplayLine;

pub use crate::render::blame::{BlameCommitJson, BlameHunkJson, BlameLineJson, BlameOutcome};

fn blame_line_marker(boundary: bool, unblamable: bool, ignored: bool) -> char {
    if boundary {
        '^'
    } else if unblamable {
        '*'
    } else if ignored {
        '?'
    } else {
        ' '
    }
}

/// Strip trailing line terminators from blamed line text for display and JSON.
fn normalize_blame_line_content(content: &str) -> String {
    content.trim_end_matches(['\n', '\r']).to_owned()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    rev_pos: Option<String>,
    file: Option<String>,
    rev: Option<String>,
    line_range: Option<String>,
    first_parent: bool,
    ignore_rev: Vec<String>,
    ignore_revs_file: Vec<String>,
    group: bool,
    ignore_all_space: bool,
    copy_detection: u8,
    move_detection: bool,
    contents: Option<String>,
    now: time::OffsetDateTime,
) -> Result<BlameOutcome> {
    let (rev_pos, file) = match (rev_pos, file) {
        (None, Some(file)) => (None, file),
        (Some(rev), Some(file)) => (Some(rev), file),
        (Some(only), None) => (None, only),
        (None, None) => bail!("missing file path"),
    };
    if rev.is_some() && rev_pos.is_some() {
        bail!("cannot use both `--rev` and a positional revision before `--`");
    }
    let explicit_rev = rev.or(rev_pos);
    let rev_spec = explicit_rev.as_deref().unwrap_or("HEAD");

    let repo = context::discover()?;
    let start = repo
        .revwalk()
        .resolve(rev_spec, &ResolveOptions::default().with_peel(Peel::Commit))
        .with_context(|| format!("could not resolve revision '{rev_spec}'"))?;

    let final_source = match contents.as_deref() {
        Some("-") => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
                .context("could not read --contents from stdin")?;
            BlameFinalSource::Contents(buf)
        }
        Some(path) => BlameFinalSource::Contents(
            std::fs::read(path).with_context(|| format!("could not read --contents '{path}'"))?,
        ),
        None if explicit_rev.is_none() && repo.work_tree().is_some() => BlameFinalSource::Worktree,
        None => BlameFinalSource::Revision,
    };
    let mut options = BlameOptions {
        start: Some(start),
        final_source,
        now: Some(now),
        ..BlameOptions::default()
    };

    let blame_path = if repo.work_tree().is_some() {
        repo.resolve_worktree_pathspec(&file, repo.pathspec_argv_prefix(&file))
            .with_context(|| format!("could not resolve path '{file}'"))?
    } else {
        file.clone()
    };

    options.line_range = line_range
        .as_deref()
        .map(|spec| {
            repo.diff()
                .resolve_blame_line_range_with_options(&blame_path, spec, &options)
        })
        .transpose()
        .with_context(|| "could not parse line range")?;

    let ignore = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: ignore_revs_file,
            revs: ignore_rev,
            use_config: true,
        },
    )?;

    let copy_detection = CopyDetection::from_count(copy_detection as usize)
        .map_err(|_| anyhow::Error::from(BlameUsageError))?;

    let mut whitespace = WhitespaceMode::default();
    if ignore_all_space {
        whitespace = WhitespaceMode::ignore_all_space();
    }

    options.first_parent_only = first_parent;
    options.ignore = ignore;
    options.whitespace = whitespace;
    options.copy_detection = copy_detection;
    options.move_detection = move_detection || copy_detection.count() > 0;
    let result = repo.diff().blame(&blame_path, &options)?;

    outcome_from_result(
        &repo,
        blame_path,
        start.to_hex(),
        &result,
        group,
        now.unix_timestamp(),
    )
}

fn outcome_from_result(
    repo: &grit_lib::repo::Repository,
    file: String,
    rev: String,
    result: &BlameResult,
    group: bool,
    now: i64,
) -> Result<BlameOutcome> {
    let mut abbrev_batch = crate::log_display::display_abbrev_batch(repo)?;
    let mut commit_short = std::collections::HashMap::new();
    for oid in result.commits.keys() {
        commit_short.insert(*oid, abbrev_batch.abbrev_known(repo, *oid)?);
    }
    let mut commits_json = BTreeMap::new();
    for (oid, info) in &result.commits {
        commits_json.insert(
            oid.to_hex(),
            BlameCommitJson {
                author: Person::from_identity_line(&info.author),
                summary: info.summary.clone(),
            },
        );
    }

    let mut lines_json = Vec::with_capacity(result.lines.len());
    let mut display_lines = Vec::with_capacity(result.lines.len());
    for line in &result.lines {
        let commit_hex = line.commit.to_hex();
        let meta = result.commits.get(&line.commit);
        let (author_short, date) = meta
            .map(|m| {
                let (author, _) = context::author_and_time(&m.author);
                let date = relative_date_from(m.author_time, now);
                (author, date)
            })
            .unwrap_or_else(|| ("unknown".to_owned(), "?".to_owned()));

        let content = normalize_blame_line_content(&line.content);
        let marker = blame_line_marker(line.boundary, line.unblamable, line.ignored);
        lines_json.push(BlameLineJson {
            commit: commit_hex.clone(),
            original_line: line.original_line,
            final_line: line.final_line,
            content: content.clone(),
            original_path: line.original_path.clone(),
            encoding_lossy: line.encoding_lossy,
            ignored: line.ignored.then_some(true),
            unblamable: line.unblamable.then_some(true),
            not_committed: line.not_committed.then_some(true),
            boundary: line.boundary.then_some(true),
        });
        let commit_short = commit_short
            .get(&line.commit)
            .cloned()
            .unwrap_or_else(|| line.commit.to_short_hex());
        display_lines.push(BlameDisplayLine {
            commit_short,
            original_line: line.original_line,
            final_line: line.final_line,
            content,
            author_short,
            date,
            marker,
        });
    }

    let (lines, hunks) = if group {
        let hunks_json = result
            .hunks()
            .into_iter()
            .map(|hunk| {
                let hunk_lines = result.hunk_lines(&hunk);
                let line_encoding_lossy = hunk_lines
                    .iter()
                    .any(|line| line.encoding_lossy)
                    .then(|| hunk_lines.iter().map(|line| line.encoding_lossy).collect());
                BlameHunkJson {
                    commit: hunk.commit.to_hex(),
                    original_start: hunk.original_start,
                    final_start: hunk.final_start,
                    count: hunk.count,
                    ignored: hunk.ignored.then_some(true),
                    unblamable: hunk.unblamable.then_some(true),
                    not_committed: hunk.not_committed.then_some(true),
                    lines: hunk_lines
                        .iter()
                        .map(|line| normalize_blame_line_content(&line.content))
                        .collect(),
                    line_encoding_lossy,
                    original_path: hunk.original_path,
                }
            })
            .collect();
        (None, Some(hunks_json))
    } else {
        (Some(lines_json), None)
    };

    Ok(BlameOutcome {
        file,
        rev,
        lines,
        hunks,
        commits: commits_json,
        display_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_blame_line_content_strips_trailing_crlf() {
        assert_eq!(normalize_blame_line_content("x\r\n"), "x");
        assert_eq!(normalize_blame_line_content("y\n"), "y");
        assert_eq!(normalize_blame_line_content("z"), "z");
    }
}

/// Dispatch parsed CLI args for `Blame`.
pub fn dispatch(
    args: crate::args::BlameArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let BlameArgs {
        rev_pos,
        file,
        rev,
        line_range,
        first_parent,
        ignore_rev,
        ignore_revs_file,
        group,
        ignore_all_space,
        copy_detection,
        move_detection,
        contents,
    } = args;
    emit_success(
        commands::blame::run(
            rev_pos,
            file,
            rev,
            line_range,
            first_parent,
            ignore_rev,
            ignore_revs_file,
            group,
            ignore_all_space,
            copy_detection,
            move_detection,
            contents,
            ctx.now,
        )?,
        opts,
        ctx,
    )
}
