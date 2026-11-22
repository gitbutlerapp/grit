//! Shared `.gitattributes` rules for per-path diff funcname (hunk header) lookup.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::attributes::{
    collect_attrs_for_path, load_gitattributes_for_check_attr_source, load_gitattributes_for_diff,
    AttrValue, DiffAttributes, ParsedGitAttributes,
};
use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::userdiff::{matcher_for_driver_arc, FuncnameMatcher};

/// Attribute rules loaded once per diff, reused for every file path.
#[derive(Clone)]
pub(crate) struct PatchFuncnameAttrs {
    diff_stack: Option<Arc<DiffAttributes>>,
    commit_tree: Option<(ObjectId, ParsedGitAttributes)>,
    driver_matchers: Arc<Mutex<BTreeMap<String, Arc<FuncnameMatcher>>>>,
}

impl PatchFuncnameAttrs {
    /// Load the same attribute stack as `git diff` (work tree, `attr.tree`, `GIT_ATTR_SOURCE`).
    ///
    /// Nested work-tree `.gitattributes` files are read only for the directories of the paths
    /// that are looked up, so the cost follows the size of the diff, not of the work tree.
    ///
    /// # Errors
    ///
    /// Propagates attribute load failures.
    pub(crate) fn load_for_diff(repo: &Repository) -> Result<Self> {
        Ok(Self {
            diff_stack: Some(Arc::new(load_gitattributes_for_diff(repo)?)),
            commit_tree: None,
            driver_matchers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Attributes from one commit tree (`git show` / `git log -p`).
    ///
    /// # Errors
    ///
    /// Propagates attribute load failures.
    pub(crate) fn load_for_commit_tree(repo: &Repository, tree: &ObjectId) -> Result<Self> {
        Ok(Self {
            diff_stack: None,
            commit_tree: Some((*tree, load_gitattributes_for_check_attr_source(repo, tree)?)),
            driver_matchers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Shared matcher for one path (cheap `Arc` clone after the driver is warm).
    ///
    /// # Errors
    ///
    /// Propagates attribute and pattern errors.
    pub(crate) fn matcher_arc_for_path(
        &self,
        _repo: &Repository,
        config: &ConfigSet,
        rel_path: &str,
    ) -> Result<Option<Arc<FuncnameMatcher>>> {
        let map = if let Some((_, parsed)) = &self.commit_tree {
            collect_attrs_for_path(&parsed.rules, &parsed.macros, rel_path, false)
        } else if let Some(attrs) = &self.diff_stack {
            attrs.collect_attrs_for_path(rel_path, false)
        } else {
            return Ok(None);
        };
        let Some(AttrValue::Value(driver)) = map.get("diff") else {
            return Ok(None);
        };
        let mut cache = self
            .driver_matchers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(matcher) = cache.get(driver) {
            return Ok(Some(Arc::clone(matcher)));
        }
        let Some(matcher) = matcher_for_driver_arc(config, driver.as_str())? else {
            return Ok(None);
        };
        Ok(Some(cache.entry(driver.clone()).or_insert(matcher).clone()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::diff::{
        write_patch_resolved, DiffOptions, DiffTarget, FileDiff, PatchResolvedSide, PatchSide,
        PatchTextOptions, RenameDetection,
    };
    use crate::environment::RepositoryOptions;
    use crate::ident::Signature;
    use crate::objects::ObjectId;
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{CancelToken, NullProgress};
    use crate::repo::{InitOptions, Repository};
    use crate::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

    const BEFORE: &str = "SECTION one\na\nb\nc\nd\ne\nf\nold\n";
    const AFTER: &str = "SECTION one\na\nb\nc\nd\ne\nf\nnew\n";

    fn open(root: &Path) -> Repository {
        Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open repo")
    }

    fn stage_all(repo: &Repository) {
        stage_for_commit(repo, &mut NullProgress, &CancelToken::never()).expect("stage");
    }

    fn commit_all(repo: &Repository, message: &str) -> ObjectId {
        stage_all(repo);
        let mut req = CommitRequest::new(message);
        req.author = Some(Signature::new("T", "t@e.com", repo.identity_now()).expect("author"));
        req.committer =
            Some(Signature::new("T", "t@e.com", repo.identity_now()).expect("committer"));
        create_commit(repo, &req, &mut NullProgress, &CancelToken::never())
            .expect("commit")
            .oid
    }

    fn options() -> DiffOptions {
        let mut opts = DiffOptions::default();
        opts.word_level_inline = false;
        opts
    }

    fn contexts(files: &[FileDiff]) -> Vec<String> {
        files
            .iter()
            .flat_map(|f| &f.hunks)
            .filter_map(|h| h.context.clone())
            .collect()
    }

    /// The full stack reads every directory of the work tree, so a diff that loads it costs
    /// time in proportion to the tree rather than to the change.
    fn assert_no_full_walk(repo: &Repository, step: &str) {
        assert!(
            !repo.caches().has_loaded_full_worktree_attr_stack(),
            "{step}: diff loaded the full work-tree .gitattributes walk"
        );
    }

    /// Every diff path `grit diff` uses resolves nested diff drivers without walking the
    /// whole work tree.
    #[test]
    fn diff_drivers_never_walk_the_whole_work_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let (setup, _) = Repository::init(root, &InitOptions::default()).expect("init");
        let config_path = setup.git_dir().join("config");
        let mut config = std::fs::read_to_string(&config_path).expect("read config");
        config.push_str("[diff \"section\"]\n\txfuncname = ^SECTION .*$\n");
        std::fs::write(&config_path, config).expect("write config");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir sub");
        std::fs::write(root.join("sub/.gitattributes"), "*.c diff=section\n").expect("attrs");
        std::fs::write(root.join("sub/a.c"), BEFORE).expect("write");
        for i in 0..20 {
            let dir = root.join(format!("pad/d{i:02}"));
            std::fs::create_dir_all(&dir).expect("mkdir pad");
            std::fs::write(dir.join("f.txt"), "x\n").expect("write pad");
        }
        std::fs::write(root.join("pad/d03/.gitattributes"), "*.c diff=cpp\n").expect("attrs");
        let base = commit_all(&setup, "base");
        std::fs::write(root.join("sub/a.c"), AFTER).expect("modify");
        let all = Pathspec::entire_tree(PathspecGlobalFlags::default());
        let section = vec!["SECTION one".to_owned()];

        let repo = open(root);
        let files = repo
            .diff()
            .files(&DiffTarget::WorktreeVsIndex, &all, &options())
            .expect("worktree files");
        assert_eq!(contexts(&files), section);
        assert_no_full_walk(&repo, "worktree files");

        let repo = open(root);
        let mut uncommitted = repo.diff().uncommitted().expect("uncommitted");
        let entries = uncommitted
            .entries(&DiffTarget::WorktreeVsIndex, &all, &options())
            .expect("worktree entries");
        let changes = uncommitted
            .load_worktree_changes(entries.entries)
            .expect("worktree changes");
        let files = repo
            .diff()
            .file_diffs_from_changes(&changes, options())
            .expect("file diffs");
        assert_eq!(contexts(&files), section);
        assert_no_full_walk(&repo, "worktree changes");

        let repo = open(root);
        let mut uncommitted = repo.diff().uncommitted().expect("uncommitted");
        let head_tree = repo.odb().read_commit(&base).expect("base commit").tree;
        let entries = uncommitted
            .worktree_entries(Some(&head_tree), RenameDetection::Off)
            .expect("entries");
        let changes = uncommitted
            .load_worktree_changes(entries.clone())
            .expect("changes");
        let resolved: Vec<PatchResolvedSide> = entries
            .into_iter()
            .zip(changes)
            .map(|(entry, fc)| {
                PatchResolvedSide::new(
                    entry,
                    PatchSide::Bytes(fc.old_bytes.data),
                    PatchSide::Worktree,
                )
            })
            .collect();
        let patch_opts = PatchTextOptions::from_repo(&repo, options()).expect("patch options");
        let mut patch = Vec::new();
        write_patch_resolved(&repo, &resolved, &patch_opts, &mut patch).expect("patch");
        assert!(
            String::from_utf8_lossy(&patch).contains("@@ SECTION one"),
            "{}",
            String::from_utf8_lossy(&patch)
        );
        assert_no_full_walk(&repo, "patch text");

        stage_all(&open(root));
        let repo = open(root);
        let files = repo
            .diff()
            .files(&DiffTarget::IndexVsHead, &all, &options())
            .expect("staged files");
        assert_eq!(contexts(&files), section);
        assert_no_full_walk(&repo, "staged files");

        let repo = open(root);
        let mut uncommitted = repo.diff().uncommitted().expect("uncommitted");
        let entries = uncommitted
            .entries(&DiffTarget::IndexVsHead, &all, &options())
            .expect("staged entries");
        let changes = repo
            .diff()
            .object_changes(&entries.entries)
            .expect("staged changes");
        let files = repo
            .diff()
            .file_diffs_from_changes(&changes, options())
            .expect("file diffs");
        assert_eq!(contexts(&files), section);
        assert_no_full_walk(&repo, "staged changes");

        let tip = commit_all(&open(root), "change");
        let trees = DiffTarget::Trees {
            old: Some(base),
            new: tip,
        };
        let repo = open(root);
        let files = repo
            .diff()
            .files(&trees, &all, &options())
            .expect("tree files");
        assert_eq!(contexts(&files), section);
        let entries = repo
            .diff()
            .entries(&trees, &all, &options())
            .expect("tree entries");
        let mut streamed = Vec::new();
        repo.diff()
            .for_each_file_diff(&entries.entries, &options(), |f| {
                streamed.push(f);
                Ok(())
            })
            .expect("streamed tree diff");
        assert_eq!(contexts(&streamed), section);
        assert_no_full_walk(&repo, "tree diff");
    }
}
