//! Annotated and lightweight tag create, list, delete, and verify for one repository.

use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::tag::{
    create_tag, delete_tag, list_tags, list_tags_with_detail, tag_ref_metadata, verify_tag_by_name,
    CreateTagOutcome, CreateTagRequest, TagDetail, TagKind, TagList, VerifiedTag,
};
use crate::refs::listing::RefQuery;
use crate::repo::Repository;
use crate::tree_sha256::TagVerifyOptions;

/// Listing mode for [`Tags::list`], collapsing [`list_tags`] and [`list_tags_with_detail`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TagListOptions {
    query: RefQuery,
    detail: TagDetail,
}

impl Default for TagListOptions {
    fn default() -> Self {
        Self {
            query: RefQuery::default(),
            detail: TagDetail::Annotation,
        }
    }
}

impl TagListOptions {
    /// Annotated summaries for every matching tag (same as legacy `list_tags`).
    #[must_use]
    pub fn full_annotations(query: RefQuery) -> Self {
        Self {
            query,
            detail: TagDetail::Annotation,
        }
    }

    /// Attach a ref query filter or sort.
    #[must_use]
    pub fn with_query(mut self, query: RefQuery) -> Self {
        self.query = query;
        self
    }

    /// Control how much is loaded per matching tag ref.
    #[must_use]
    pub fn with_detail(mut self, detail: TagDetail) -> Self {
        self.detail = detail;
        self
    }

    /// Ref query (patterns, sort, limit, namespaces) passed to listing.
    #[must_use]
    pub fn query(&self) -> &RefQuery {
        &self.query
    }

    /// How much tag metadata to load for each matching ref.
    #[must_use]
    pub fn detail(&self) -> TagDetail {
        self.detail
    }
}

/// Tag ref create, list, delete, and verify for one repository.
#[derive(Debug, Copy, Clone)]
pub struct Tags<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Tags<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Create a lightweight or annotated tag ref.
    ///
    /// # Errors
    ///
    /// Returns [`crate::porcelain::tag::TagError`] when the name is invalid, the tag exists
    /// without `force`, or the message is empty after cleanup.
    pub fn create(&self, req: &CreateTagRequest) -> Result<CreateTagOutcome> {
        create_tag(self.repo, req)
    }

    /// List tags matching `opts.query` at `opts.detail`.
    pub fn list(&self, opts: &TagListOptions) -> Result<TagList> {
        match opts.detail {
            TagDetail::Annotation => {
                let summaries = list_tags(self.repo, &opts.query)?;
                Ok(TagList::Summaries(summaries))
            }
            _ => list_tags_with_detail(self.repo, &opts.query, opts.detail),
        }
    }

    /// Delete `refs/tags/<name>`; returns the object the tag pointed at.
    pub fn delete(&self, name: &str, dry_run: bool) -> Result<ObjectId> {
        delete_tag(self.repo, name, dry_run)
    }

    /// Verify an annotated tag by short name.
    pub fn verify(&self, name: &str, options: &TagVerifyOptions) -> Result<VerifiedTag> {
        verify_tag_by_name(self.repo, name, options)
    }

    /// Classify a tag ref target and peel to a commit when possible.
    #[must_use]
    pub fn ref_metadata(&self, ref_oid: &ObjectId) -> (TagKind, Option<ObjectId>) {
        tag_ref_metadata(self.repo, ref_oid)
    }

    /// Creator date for a tag or tagged commit (see [`crate::porcelain::tag::creator_date`]).
    #[must_use]
    pub fn creator_date(&self, oid: &ObjectId) -> i64 {
        crate::porcelain::tag::creator_date(self.repo, oid)
    }
}
