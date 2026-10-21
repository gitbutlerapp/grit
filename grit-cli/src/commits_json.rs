//! Shared commit JSON loading for `log`, `show`, `revs`, and `shortlog`.

use anyhow::Result;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::log::{
    collect_decorations_for_commits, load_commits_with_decorations, DecorationMap, LoadedCommit,
};
use grit_lib::repo::Repository;

use crate::context::{self, subject_line, CommitSummary};
use crate::output::{CommitJson, CommitRefJson};

/// Decoration map plus helpers to build [`CommitJson`] rows without re-walking refs per commit.
pub struct CommitJsonLoader {
    decorations: DecorationMap,
}

impl CommitJsonLoader {
    /// Collect ref decorations for the commits that will appear in this JSON page.
    pub fn new(repo: &Repository, commits: &[ObjectId]) -> Result<Self> {
        Ok(Self {
            decorations: collect_decorations_for_commits(repo, false, commits)?,
        })
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
        let loaded = load_commits_with_decorations(repo, oids, &self.decorations)?;
        let mut json = Vec::with_capacity(loaded.len());
        let mut summaries = Vec::with_capacity(loaded.len());
        for row in &loaded {
            let (author, timestamp) = context::author_and_time(&row.author);
            summaries.push(CommitSummary {
                oid: row.oid,
                subject: subject_line(&row.message),
                author,
                author_ident: row.author.clone(),
                timestamp,
            });
            json.push(CommitJson::from_loaded(row, now));
        }
        Ok((json, summaries))
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
        let (_, author_ts) = crate::context::author_and_time(&commit.author);
        Self {
            oid: commit.oid.to_hex(),
            parents: commit.parents.iter().map(ObjectId::to_hex).collect(),
            author: crate::output::Person::from_identity_line(&commit.author),
            committer: crate::output::Person::from_identity_line(&commit.committer),
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
            relative_date: Some(crate::context::relative_date_from(author_ts, now)),
            side: None,
            equivalent: None,
        }
    }
}
