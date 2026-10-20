//! Shared revision-selection flags for `grit log` and `grit revs`.

use anyhow::{bail, Context, Result};
use clap::Args;
use grit_lib::config::environment::Environment;
use grit_lib::revwalk::{
    committer_cutoff_from_date, MergeFilter, RefSelection, RevWalkListOptions,
};

const CURSOR_PREFIX: &str = "lw1.";

/// Revision ranges, limits, dates, and ref sets shared by history-list commands.
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

    /// Limit to commits that touch these paths.
    #[arg(value_name = "PATH", last = true)]
    pub paths: Vec<String>,
}

impl RevSelectArgs {
    /// Convert CLI flags into [`RevWalkListOptions`].
    ///
    /// When `before` carries a deprecated `lw1.` log cursor, it is returned in
    /// `deprecated_cursor` instead of being parsed as a date.
    pub fn to_rev_walk_options(
        &self,
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
        let until = match self.until.as_deref() {
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
                return Ok(RevWalkListOptions {
                    limit: self.max_count,
                    skip: self.skip,
                    since,
                    until: Some(cutoff),
                    merges: merge_filter(self.merges, self.no_merges),
                    reverse: self.reverse,
                    refs: ref_selections(self),
                    first_parent: self.first_parent,
                    paths: self.paths.clone(),
                });
            }
        }

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
