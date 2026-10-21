//! Shared revision-selection flags for `grit log` and `grit revs`.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Args;
use grit_lib::config::environment::Environment;
use grit_lib::config::mailmap::load_mailmap_table;
use grit_lib::config::parse_bool;
use grit_lib::diff::{Pickaxe, PickaxeError};
use grit_lib::repo::Repository;
use grit_lib::revwalk::filter::{CommitFilter, CommitFilterBuilder, CommitFilterError};
use grit_lib::revwalk::{
    committer_cutoff_from_date, MergeFilter, RefSelection, RevWalkListOptions,
};

use crate::json_error::{invalid_commit_filter, invalid_pickaxe};

const CURSOR_PREFIX: &str = "lw1.";

/// Revision ranges, limits, dates, ref sets, and commit filters shared by history-list commands.
#[derive(Debug, Args, Clone)]
pub struct RevSelectArgs {
    /// Revision tips and ranges (`A..B`, `A...B`, `^tip`, plain tips).
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,

    /// Stop after this many commits.
    #[arg(short = 'n', long = "max-count", alias = "limit", value_name = "N")]
    pub max_count: Option<usize>,

    /// Skip this many commits after ordering.
    #[arg(long, default_value_t = 0)]
    pub skip: usize,

    /// Exclude commits older than this date (Git `--since`).
    #[arg(long = "since", value_name = "DATE")]
    pub since: Option<String>,

    /// Alias for `--since`.
    #[arg(long = "after", value_name = "DATE")]
    pub after: Option<String>,

    /// Exclude commits newer than this date (Git `--until`).
    #[arg(long = "until", value_name = "DATE")]
    pub until: Option<String>,

    /// Alias for `--until`, unless the value is an `lw1.` log cursor (deprecated).
    #[arg(long = "before", value_name = "DATE")]
    pub before: Option<String>,

    /// Follow only the first parent at merges.
    #[arg(long = "first-parent")]
    pub first_parent: bool,

    /// Include only merge commits.
    #[arg(long = "merges", conflicts_with = "no_merges")]
    pub merges: bool,

    /// Exclude merge commits.
    #[arg(long = "no-merges", conflicts_with = "merges")]
    pub no_merges: bool,

    /// Reverse commit order (oldest first).
    #[arg(long)]
    pub reverse: bool,

    /// Include every ref tip.
    #[arg(long)]
    pub all: bool,

    /// Include branch tips, or those matching an optional glob.
    #[arg(
        long = "branches",
        value_name = "GLOB",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub branches: Option<String>,

    /// Include tag tips, or those matching an optional glob.
    #[arg(
        long = "tags",
        value_name = "GLOB",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub tags: Option<String>,

    /// Include remote-tracking tips, or those matching an optional glob.
    #[arg(
        long = "remotes",
        value_name = "GLOB",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub remotes: Option<String>,

    /// Limit to commits whose author matches this pattern (repeatable; OR together).
    #[arg(long = "author", value_name = "PAT", action = clap::ArgAction::Append)]
    pub author: Vec<String>,

    /// Limit to commits whose committer matches this pattern (repeatable; OR together).
    #[arg(long = "committer", value_name = "PAT", action = clap::ArgAction::Append)]
    pub committer: Vec<String>,

    /// Limit to commits whose message matches this pattern (repeatable; OR unless `--all-match`).
    #[arg(long = "grep", value_name = "PAT", action = clap::ArgAction::Append)]
    pub grep: Vec<String>,

    /// Case-insensitive pattern matching for `--author`, `--committer`, and `--grep`.
    #[arg(short = 'i', long = "regexp-ignore-case")]
    pub regexp_ignore_case: bool,

    /// Require every `--grep` pattern to match.
    #[arg(long = "all-match")]
    pub all_match: bool,

    /// Invert `--grep` matching (commits that do not match the grep group).
    #[arg(long = "invert-grep")]
    pub invert_grep: bool,

    /// Treat `--author`, `--committer`, and `--grep` patterns as fixed strings.
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed_strings: bool,

    /// Do not apply `.mailmap` when matching `--author` or `--committer`.
    #[arg(long = "no-mailmap")]
    pub no_mailmap: bool,

    /// Keep commits where occurrence count of this string changed (`git log -S`).
    #[arg(short = 'S', value_name = "STRING", conflicts_with = "pickaxe_grep")]
    pub pickaxe_string: Option<String>,

    /// Keep commits whose added/removed lines match this pattern (`git log -G`).
    #[arg(short = 'G', value_name = "PAT", conflicts_with = "pickaxe_string")]
    pub pickaxe_grep: Option<String>,

    /// Treat `-S` as a BRE instead of a literal string (`--pickaxe-regex`).
    #[arg(long = "pickaxe-regex")]
    pub pickaxe_regex: bool,

    /// Limit to commits that touch these paths.
    #[arg(value_name = "PATH", last = true)]
    pub paths: Vec<String>,
}

impl RevSelectArgs {
    /// True when author/committer/grep filters are set (disables log cursor paging).
    #[must_use]
    pub fn has_commit_filter_flags(&self) -> bool {
        !self.author.is_empty()
            || !self.committer.is_empty()
            || !self.grep.is_empty()
            || self.pickaxe_string.is_some()
            || self.pickaxe_grep.is_some()
    }

    /// True when the caller supplied revision, date, or ref-set selectors (not command defaults).
    ///
    /// Log continuation with `--cursor` must not repeat these; the cursor encodes the query.
    #[must_use]
    pub fn has_log_walk_selectors(&self) -> bool {
        !self.revs.is_empty()
            || self.since.is_some()
            || self.after.is_some()
            || self.until.is_some()
            || self
                .before
                .as_ref()
                .is_some_and(|b| !b.starts_with(CURSOR_PREFIX))
            || self.all
            || self.branches.is_some()
            || self.tags.is_some()
            || self.remotes.is_some()
    }

    /// Convert CLI flags into [`RevWalkListOptions`].
    ///
    /// When `before` carries a deprecated `lw1.` log cursor, it is returned in
    /// `deprecated_cursor` instead of being parsed as a date.
    pub fn to_rev_walk_options(
        &self,
        repo: &Repository,
        env: &Environment,
        default_revs: &[&str],
        deprecated_cursor: &mut Option<String>,
        deprecated_cursor_warning: &mut bool,
    ) -> Result<RevWalkListOptions> {
        if self.merges && self.no_merges {
            bail!("`--merges` and `--no-merges` cannot be used together");
        }
        let now_sec = crate::context::wall_clock_now(env).unix_timestamp();
        let since_str = self.since.as_deref().or(self.after.as_deref());
        let since = match since_str {
            Some(s) => Some(parse_cutoff(s, now_sec).context("invalid --since/--after date")?),
            None => None,
        };
        let mut until = match self.until.as_deref() {
            Some(s) => Some(parse_cutoff(s, now_sec).context("invalid --until date")?),
            None => None,
        };
        if let Some(before) = self.before.as_deref() {
            if before.starts_with(CURSOR_PREFIX) {
                *deprecated_cursor = Some(before.to_owned());
                *deprecated_cursor_warning = true;
            } else {
                let cutoff = parse_cutoff(before, now_sec).context("invalid --before date")?;
                if until.is_some() {
                    bail!("`--until` and `--before` cannot both set a date cutoff");
                }
                until = Some(cutoff);
            }
        }

        let commit_filter = self
            .build_commit_filter(repo)
            .map_err(invalid_commit_filter)?;
        let pickaxe = self.build_pickaxe().map_err(invalid_pickaxe)?;
        let _ = default_revs;
        Ok(RevWalkListOptions {
            limit: self.max_count,
            skip: self.skip,
            since,
            until,
            merges: merge_filter(self.merges, self.no_merges),
            reverse: self.reverse,
            refs: ref_selections(self),
            first_parent: self.first_parent,
            paths: self.paths.clone(),
            commit_filter,
            pickaxe,
            graph: false,
        })
    }

    /// Revision tokens passed to the library walk (after applying command defaults).
    pub fn rev_tokens<'a>(&'a self, default_revs: &'a [&'a str]) -> Vec<&'a str> {
        if self.revs.is_empty() {
            default_revs.to_vec()
        } else {
            self.revs.iter().map(String::as_str).collect()
        }
    }

    fn build_pickaxe(&self) -> std::result::Result<Option<Arc<Pickaxe>>, PickaxeError> {
        if let Some(s) = &self.pickaxe_string {
            if s.is_empty() {
                return Err(PickaxeError::InvalidPattern {
                    pattern: String::new(),
                    detail: "-S requires a non-empty argument".into(),
                });
            }
            if self.pickaxe_regex && self.pickaxe_grep.is_some() {
                return Err(PickaxeError::InvalidPattern {
                    pattern: s.clone(),
                    detail: "--pickaxe-regex cannot be used with -G".into(),
                });
            }
            let px = Pickaxe::occurrences(s.as_str())
                .ignore_case(self.regexp_ignore_case)
                .pickaxe_regex(self.pickaxe_regex)
                .build()?;
            return Ok(Some(px.into_shared()));
        }
        if let Some(g) = &self.pickaxe_grep {
            if g.is_empty() {
                return Err(PickaxeError::InvalidPattern {
                    pattern: String::new(),
                    detail: "-G requires a non-empty argument".into(),
                });
            }
            if self.pickaxe_regex {
                return Err(PickaxeError::InvalidPattern {
                    pattern: g.clone(),
                    detail: "--pickaxe-regex cannot be used with -G".into(),
                });
            }
            let px = Pickaxe::line_grep(g, self.regexp_ignore_case)?;
            return Ok(Some(px.into_shared()));
        }
        Ok(None)
    }

    fn build_commit_filter(
        &self,
        repo: &Repository,
    ) -> std::result::Result<Option<Arc<CommitFilter>>, CommitFilterError> {
        if !self.has_commit_filter_flags() {
            return Ok(None);
        }
        let mut builder = CommitFilterBuilder::new()
            .ignore_case(self.regexp_ignore_case)
            .all_match(self.all_match)
            .invert_grep(self.invert_grep)
            .fixed_strings(self.fixed_strings);
        for pat in &self.author {
            builder = builder.author(pat.as_str());
        }
        for pat in &self.committer {
            builder = builder.committer(pat.as_str());
        }
        for pat in &self.grep {
            builder = builder.grep(pat.as_str());
        }
        if log_mailmap_enabled(repo, self.no_mailmap) {
            if let Ok(table) = load_mailmap_table(repo) {
                if !table.is_empty() {
                    builder = builder.mailmap(table);
                }
            }
        }
        builder.build().map(|f| Some(Arc::new(f)))
    }
}

/// Whether author/committer mailmap applies for log-style commands (`log.mailmap`, `--no-mailmap`).
pub(crate) fn log_mailmap_enabled(repo: &Repository, no_mailmap: bool) -> bool {
    if no_mailmap {
        return false;
    }
    let Ok(config) = repo.config() else {
        return true;
    };
    match config.get("log.mailmap") {
        None => true,
        Some(v) => parse_bool(&v).unwrap_or(true),
    }
}

fn parse_cutoff(date: &str, now_sec: i64) -> Result<i64> {
    committer_cutoff_from_date(date, now_sec).map_err(|e| anyhow::anyhow!("{e}"))
}

fn merge_filter(merges: bool, no_merges: bool) -> MergeFilter {
    if merges {
        MergeFilter::OnlyMerges
    } else if no_merges {
        MergeFilter::NoMerges
    } else {
        MergeFilter::Any
    }
}

fn ref_selections(args: &RevSelectArgs) -> Vec<RefSelection> {
    let mut out = Vec::new();
    if args.all {
        out.push(RefSelection::All);
    }
    if let Some(glob) = &args.branches {
        let pattern = (!glob.is_empty()).then(|| glob.clone());
        out.push(RefSelection::Branches(pattern));
    }
    if let Some(glob) = &args.tags {
        let pattern = (!glob.is_empty()).then(|| glob.clone());
        out.push(RefSelection::Tags(pattern));
    }
    if let Some(glob) = &args.remotes {
        let pattern = (!glob.is_empty()).then(|| glob.clone());
        out.push(RefSelection::Remotes(pattern));
    }
    out
}
