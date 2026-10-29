//! Shared commit JSON loading for `log`, `show`, `revs`, and `shortlog`.

use anyhow::Result;
use grit_lib::objects::{CommitData, ObjectId, OidHashMap};
use grit_lib::porcelain::log::{
    collect_decorations, collect_decorations_for_commits, load_commits_with_decorations_reusing,
    CommitReuse, DecorationMap, LoadedCommit,
};
use grit_lib::repo::Repository;

use crate::context::{self, subject_line, CommitSummary};
use crate::output::{CommitJson, CommitRefJson};

/// Decoration map plus helpers to build [`CommitJson`] rows without re-walking refs per commit.
pub struct CommitJsonLoader {
    /// Decorations for every commit, or `None` to collect them per call for just its rows.
    decorations: Option<DecorationMap>,
}

impl CommitJsonLoader {
    /// Collect ref decorations once for the repository, for commands that load many
    /// batches of rows.
    pub fn new(repo: &Repository) -> Result<Self> {
        Ok(Self {
            decorations: Some(collect_decorations(repo, false)?),
        })
    }

    /// Collect decorations per call for only the commits being loaded, for commands that
    /// load their rows in one batch.
    pub fn for_listed_commits() -> Self {
        Self { decorations: None }
    }

    /// Leave every row's `refs` empty, for output that never renders decorations.
    pub fn without_refs() -> Self {
        Self {
            decorations: Some(DecorationMap::default()),
        }
    }

    /// Build human [`CommitSummary`] rows without loading ref decorations (no extra config I/O).
    /// Commits already parsed (by the walk) are taken out of `parsed` instead of read again.
    pub fn load_summaries_only(
        repo: &Repository,
        oids: &[ObjectId],
        parsed: &mut impl CommitReuse,
    ) -> Result<Vec<CommitSummary>> {
        let mut summaries = Vec::with_capacity(oids.len());
        for oid in oids {
            let data = match parsed.take_commit(oid) {
                Some(data) => data?,
                None => repo.odb().read_commit(oid)?,
            };
            let resolved = grit_lib::change_id::resolved_change_id(oid, &data);
            let (author, timestamp) = context::author_and_time(&data.author);
            summaries.push(CommitSummary {
                oid: *oid,
                subject: subject_line(&data.message),
                author,
                author_ident: data.author,
                timestamp,
                change_id: resolved.id.as_str().to_owned(),
                change_id_stored: resolved.stored,
            });
        }
        Ok(summaries)
    }

    /// Build JSON rows for `oids` in order (one commit read per oid).
    pub fn load(&self, repo: &Repository, oids: &[ObjectId], now: i64) -> Result<Vec<CommitJson>> {
        Ok(self.load_with_summaries(repo, oids, now)?.0)
    }

    /// Build JSON rows and human [`CommitSummary`] rows with a single commit read per oid.
    pub fn load_with_summaries(
        &self,
        repo: &Repository,
        oids: &[ObjectId],
        now: i64,
    ) -> Result<(Vec<CommitJson>, Vec<CommitSummary>)> {
        self.load_with_summaries_reusing(repo, oids, now, OidHashMap::<CommitData>::default())
    }

    /// Like [`Self::load_with_summaries`], taking commits the walk already read out of `parsed`.
    /// An owned `parsed` is dropped before the rows are built.
    pub fn load_with_summaries_reusing(
        &self,
        repo: &Repository,
        oids: &[ObjectId],
        now: i64,
        parsed: impl CommitReuse,
    ) -> Result<(Vec<CommitJson>, Vec<CommitSummary>)> {
        let loaded = self.load_commits(repo, oids, parsed)?;
        let mut json = Vec::with_capacity(loaded.len());
        let mut summaries = Vec::with_capacity(loaded.len());
        for row in &loaded {
            summaries.push(CommitSummary {
                oid: row.oid,
                subject: subject_line(&row.message),
                author: context::author_display_name(&row.author.name, &row.author.email),
                author_ident: row.author.header_line(),
                timestamp: row.author.time.unix_timestamp(),
                change_id: row.change_id.as_str().to_owned(),
                change_id_stored: row.change_id_stored,
            });
            json.push(CommitJson::from_loaded(row, now));
        }
        Ok((json, summaries))
    }

    /// Like [`Self::load_with_summaries_reusing`], for output that only needs the JSON rows.
    pub fn load_reusing(
        &self,
        repo: &Repository,
        oids: &[ObjectId],
        now: i64,
        parsed: impl CommitReuse,
    ) -> Result<Vec<CommitJson>> {
        Ok(self
            .load_commits(repo, oids, parsed)?
            .iter()
            .map(|row| CommitJson::from_loaded(row, now))
            .collect())
    }

    /// Load `oids` with their decorations, taking walked commits out of `parsed`, which is
    /// dropped before the caller builds its rows.
    fn load_commits(
        &self,
        repo: &Repository,
        oids: &[ObjectId],
        mut parsed: impl CommitReuse,
    ) -> Result<Vec<LoadedCommit>> {
        let listed;
        let decorations = match &self.decorations {
            Some(all) => all,
            None => {
                listed = collect_decorations_for_commits(repo, false, oids)?;
                &listed
            }
        };
        Ok(load_commits_with_decorations_reusing(
            repo,
            oids,
            decorations,
            &mut parsed,
        )?)
    }

    /// Build a single [`CommitJson`] row.
    pub fn load_one(&self, repo: &Repository, oid: &ObjectId, now: i64) -> Result<CommitJson> {
        self.load(repo, std::slice::from_ref(oid), now)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("internal error: missing commit row"))
    }
}

/// When the walk used `limit + 1`, keep the first `limit` oids and report truncation.
pub fn truncate_oid_list(oids: Vec<ObjectId>, limit: Option<usize>) -> (Vec<ObjectId>, bool) {
    let Some(n) = limit else {
        return (oids, false);
    };
    if oids.len() > n {
        (oids.into_iter().take(n).collect(), true)
    } else {
        (oids, false)
    }
}

/// Rev-walk limit that fetches one extra row to detect truncation cheaply.
pub fn fetch_limit_plus_one(limit: Option<usize>) -> Option<usize> {
    limit.map(|n| n.saturating_add(1))
}

impl CommitJson {
    /// Full commit JSON from a library [`LoadedCommit`].
    pub fn from_loaded(commit: &LoadedCommit, now: i64) -> Self {
        Self {
            oid: commit.oid.to_hex(),
            parents: commit.parents.iter().map(ObjectId::to_hex).collect(),
            author: crate::output::Person::from_signature(&commit.author),
            committer: crate::output::Person::from_signature(&commit.committer),
            subject: subject_line(&commit.message),
            message: commit.message.trim_end().to_owned(),
            refs: commit
                .refs
                .iter()
                .map(|r| CommitRefJson {
                    name: r.name.clone(),
                    kind: r.kind.into(),
                })
                .collect(),
            relative_date: Some(crate::context::relative_date_from(
                commit.author.time.unix_timestamp(),
                now,
            )),
            change_id: commit.change_id.as_str().to_owned(),
            change_id_stored: commit.change_id_stored,
        }
    }
}
