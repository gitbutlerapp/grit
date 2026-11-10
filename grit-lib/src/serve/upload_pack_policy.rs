//! Upload-pack configuration: capabilities, want validation, filters.

use std::collections::{HashSet, VecDeque};

use super::{Result, ServeError};
use crate::config::ConfigSet;
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;
use crate::upload_filter::{validate_upload_filter_request, UploadFilterError};

/// Policy derived from repository config for one upload-pack session.
pub(crate) struct UploadPackPolicy {
    pub allow_filter: bool,
    pub allow_tip_sha1_in_want: bool,
    pub allow_reachable_sha1_in_want: bool,
    /// `uploadpack.allowAnySHA1InWant`: any object present in the repository may
    /// be wanted, reachable or not. A large-object promisor that holds blobs no
    /// ref points at needs this to answer lazy fetches.
    pub allow_any_sha1_in_want: bool,
    pub ref_in_want: bool,
    pub allow_sideband_all: bool,
    /// `transfer.advertiseObjectInfo`: advertise and serve the v2 `object-info`
    /// command. Off by default, as in Git.
    pub advertise_object_info: bool,
    /// `uploadpack.lazyFetch` (grit only): before answering a client that accepted
    /// no promisor remote, fetch the promised objects the repository lacks from its
    /// own promisor remotes. Off by default, like Git's upload-pack.
    pub lazy_fetch: bool,
}

impl UploadPackPolicy {
    pub fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.get_bool(key).ok().flatten().unwrap_or(false);
        Self {
            allow_filter: flag("uploadpack.allowfilter"),
            allow_tip_sha1_in_want: flag("uploadpack.allowtipsha1inwant"),
            allow_reachable_sha1_in_want: flag("uploadpack.allowreachablesha1inwant"),
            allow_any_sha1_in_want: flag("uploadpack.allowanysha1inwant"),
            ref_in_want: flag("uploadpack.allowrefinwant"),
            allow_sideband_all: flag("uploadpack.allowsidebandall"),
            advertise_object_info: flag("transfer.advertiseobjectinfo"),
            lazy_fetch: flag("uploadpack.lazyfetch"),
        }
    }

    pub fn v0_capability_tokens(&self) -> Vec<&'static str> {
        let mut caps = vec!["shallow", "deepen-since", "deepen-not", "deepen-relative"];
        if self.allow_filter {
            caps.push("filter");
        }
        // As in Git, allowing any object implies both narrower capabilities.
        if self.allow_tip_sha1_in_want || self.allow_any_sha1_in_want {
            caps.push("allow-tip-sha1-in-want");
        }
        if self.allow_reachable_sha1_in_want || self.allow_any_sha1_in_want {
            caps.push("allow-reachable-sha1-in-want");
        }
        caps
    }

    /// Features for the v2 `fetch=` capability line.
    ///
    /// Only v2 features belong here. Request arguments every v2 server accepts
    /// (`thin-pack`, `ofs-delta`, `deepen-not`, ...) are not advertised.
    pub fn v2_fetch_features(&self) -> Vec<&'static str> {
        let mut f = vec!["shallow", "wait-for-done"];
        if self.allow_filter {
            f.push("filter");
        }
        if self.ref_in_want {
            f.push("ref-in-want");
        }
        if self.allow_sideband_all {
            f.push("sideband-all");
        }
        f
    }

    pub fn parse_filter(&self, config: &ConfigSet, spec: &str) -> Result<ObjectFilter> {
        if !self.allow_filter {
            return Err(ServeError::Protocol {
                detail: "filter not supported (uploadpack.allowFilter is false)".into(),
            }
            .into());
        }
        validate_upload_filter_request(config, spec).map_err(ServeError::UploadFilter)?;
        Ok(ObjectFilter::parse(spec)
            .map_err(|e| ServeError::UploadFilter(UploadFilterError::InvalidFilterSpec(e)))?)
    }
}

/// Resolve `want-ref` names to OIDs and record them for the `wanted-refs` section.
pub(crate) fn resolve_want_ref(
    repo: &Repository,
    hidden: &[String],
    name: &str,
) -> Result<ObjectId> {
    if matches!(name, "HEAD" | "refs/HEAD") {
        let head = super::head_info(repo);
        return Ok(head.oid.ok_or_else(|| ServeError::Protocol {
            detail: "unknown ref HEAD".into(),
        })?);
    }
    let full = if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/{name}")
    };
    if crate::hide_refs::ref_is_hidden(name, &full, hidden) {
        return Err(ServeError::Protocol {
            detail: format!("unknown ref {name}"),
        }
        .into());
    }
    Ok(crate::refs::resolve_ref_in(repo, &full)
        .or_else(|_| crate::refs::resolve_ref_in(repo, name))
        .map_err(|_| ServeError::Protocol {
            detail: format!("unknown ref {name}"),
        })?)
}

/// Validate `want` OIDs for a filtered fetch (object must exist in the ODB).
pub(crate) fn validate_wants_filtered(repo: &Repository, wants: &[ObjectId]) -> Result<()> {
    for w in wants {
        if !repo.odb().contains(w)? {
            return Err(ServeError::NotOurRef(*w).into());
        }
    }
    Ok(())
}

/// Validate `want` OIDs against advertisement, config rules, and session mode.
///
/// Stateful sessions only accept wants that appeared in the ref advertisement
/// (plus `HEAD`). Stateless RPC (smart HTTP POST) matches Git: objects reachable
/// from current ref tips are allowed without extra config, but wants that are
/// already advertised ref tips skip reachability work.
pub(crate) fn validate_wants(
    repo: &Repository,
    advertised_tips: &HashSet<ObjectId>,
    head: Option<ObjectId>,
    wants: &[ObjectId],
    policy: &UploadPackPolicy,
    stateless_rpc: bool,
) -> Result<()> {
    if policy.allow_any_sha1_in_want {
        return validate_wants_filtered(repo, wants);
    }
    if policy.allow_reachable_sha1_in_want {
        let reachable = reachable_from_repository_tips(repo)?;
        for w in wants {
            if !reachable.contains(w) {
                return Err(ServeError::NotOurRef(*w).into());
            }
        }
        return Ok(());
    }

    let advertised = advertised_object_ids(advertised_tips, head);
    let extra: Vec<ObjectId> = wants
        .iter()
        .copied()
        .filter(|w| !advertised.contains(w))
        .collect();
    if extra.is_empty() {
        return Ok(());
    }
    let unique_extra = dedupe_object_ids(&extra);

    if stateless_rpc {
        validate_stateless_extra_wants(repo, &unique_extra)?;
        return Ok(());
    }

    if policy.allow_tip_sha1_in_want {
        let tips = repository_tip_commits(repo)?;
        for w in &unique_extra {
            if !tips.contains(w) {
                return Err(ServeError::NotOurRef(*w).into());
            }
        }
        return Ok(());
    }

    Err(ServeError::NotOurRef(extra[0]).into())
}

fn dedupe_object_ids(ids: &[ObjectId]) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    ids.iter().filter(|id| seen.insert(**id)).copied().collect()
}

fn advertised_object_ids(tips: &HashSet<ObjectId>, head: Option<ObjectId>) -> HashSet<ObjectId> {
    let mut allowed = tips.clone();
    if let Some(h) = head {
        allowed.insert(h);
    }
    allowed
}

/// Extra wants in stateless RPC: reachable from ref tips (Git smart HTTP race case).
fn validate_stateless_extra_wants(repo: &Repository, wants: &[ObjectId]) -> Result<()> {
    let tips = repository_tip_commits(repo)?;
    let ref_objects = repository_ref_object_ids(repo)?;
    let mut targets: HashSet<ObjectId> = HashSet::new();
    let mut checks: Vec<(ObjectId, ObjectId)> = Vec::new();
    for w in wants {
        if ref_objects.contains(w) || tips.contains(w) {
            continue;
        }
        if !repo.odb().contains(w)? {
            return Err(ServeError::NotOurRef(*w).into());
        }
        let target = want_reachability_target(repo, *w)?;
        targets.insert(target);
        checks.push((*w, target));
    }
    if targets.is_empty() {
        return Ok(());
    }
    let found = reachable_targets_from_tip_closure(repo, &tips, &targets)?;
    for (want, target) in checks {
        if !found.contains(&target) {
            return Err(ServeError::NotOurRef(want).into());
        }
    }
    Ok(())
}

/// For annotated tag wants, reachability follows the fully peeled object id.
fn want_reachability_target(repo: &Repository, want: ObjectId) -> Result<ObjectId> {
    let mut current = want;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return Ok(want);
        }
        let Ok(obj) = repo.odb().read(&current) else {
            return Ok(want);
        };
        if obj.kind != ObjectKind::Tag {
            return Ok(current);
        }
        let Ok(tag) = crate::objects::parse_tag(&obj.data) else {
            return Ok(want);
        };
        current = tag.object;
    }
}

/// Direct ref targets (unpeeled), including annotated tag objects on refs.
fn repository_ref_object_ids(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut ids = HashSet::new();
    for (_, oid) in crate::refs::list_refs_in(repo, "refs/")? {
        ids.insert(oid);
    }
    if let Ok(head) = crate::refs::resolve_ref_in(repo, "HEAD") {
        ids.insert(head);
    }
    Ok(ids)
}

/// Which of `targets` appear in the object closure reachable from `tips`.
fn reachable_targets_from_tip_closure(
    repo: &Repository,
    tips: &HashSet<ObjectId>,
    targets: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut found = HashSet::new();
    if targets.is_empty() {
        return Ok(found);
    }
    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = queue.pop_front() {
        if targets.contains(&oid) {
            found.insert(oid);
            if found.len() == targets.len() {
                return Ok(found);
            }
        }
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb().read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = crate::objects::parse_commit(&obj.data) {
                    queue.extend(c.parents);
                    queue.push_back(c.tree);
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = crate::objects::parse_tree(&obj.data) {
                    for entry in entries {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = crate::objects::parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(found)
}

/// Peeled commit tips for every ref in the repository (including hidden refs).
fn repository_tip_commits(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut tips = HashSet::new();
    for (_, oid) in crate::refs::list_refs_in(repo, "refs/")? {
        if let Ok(p) = repo.odb().peel_tags(oid).map(|(p, _)| p) {
            tips.insert(p);
        }
    }
    if let Ok(head) = crate::refs::resolve_ref_in(repo, "HEAD") {
        if let Ok(p) = repo.odb().peel_tags(head).map(|(p, _)| p) {
            tips.insert(p);
        }
    }
    Ok(tips)
}

fn reachable_from_repository_tips(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let tips = repository_tip_commits(repo)?;
    let mut seen = HashSet::new();
    let mut q: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = q.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb().read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = crate::objects::parse_commit(&obj.data) {
                    q.extend(c.parents);
                    q.push_back(c.tree);
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = crate::objects::parse_tree(&obj.data) {
                    for entry in entries {
                        q.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = crate::objects::parse_tag(&obj.data) {
                    q.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::config::environment::RepositoryOptions;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::refs::store::{
        Expected, MemoryRefStore, RawRef, RefName, RefStore, RefTransaction, RefUpdate,
    };
    use crate::repo::{InitOptions, Repository};

    use super::{
        dedupe_object_ids, repository_ref_object_ids, resolve_want_ref, validate_wants,
        UploadPackPolicy,
    };

    fn memory_serve_repo() -> (tempfile::TempDir, Repository, Arc<MemoryRefStore>, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("master"),
        )
        .expect("init");
        let store = Arc::new(MemoryRefStore::new());
        let repo = Repository::open(
            dir.path(),
            &(RepositoryOptions::empty()
                .with_ref_store(store.clone())
                .with_initial_branch("master")),
        )
        .expect("open");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let commit = repo
            .odb()
            .write(
                ObjectKind::Commit,
                format!(
                    "tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\ninit\n"
                )
                .as_bytes(),
            )
            .expect("commit");
        store
            .prepare(
                RefTransaction::new()
                    .update(RefUpdate {
                        name: RefName::from_trusted("refs/heads/master"),
                        new_value: Some(RawRef::Direct(commit)),
                        expected: Expected::Missing,
                        reflog: None,
                        flags: Default::default(),
                    })
                    .expect("txn"),
            )
            .expect("prepare")
            .commit()
            .expect("commit");
        (dir, repo, store, commit)
    }

    #[test]
    fn resolve_want_ref_head_uses_injected_store() {
        let (_dir, repo, _store, commit) = memory_serve_repo();
        let oid = resolve_want_ref(&repo, &[], "HEAD").expect("HEAD");
        assert_eq!(oid, commit);
    }

    #[test]
    fn resolve_want_ref_memory_only_branch() {
        let (_dir, repo, store, commit) = memory_serve_repo();
        store
            .prepare(
                RefTransaction::new()
                    .update(RefUpdate {
                        name: RefName::from_trusted("refs/heads/only-memory"),
                        new_value: Some(RawRef::Direct(commit)),
                        expected: Expected::Missing,
                        reflog: None,
                        flags: Default::default(),
                    })
                    .expect("txn"),
            )
            .expect("prepare")
            .commit()
            .expect("commit");
        let oid = resolve_want_ref(&repo, &[], "heads/only-memory").expect("ref");
        assert_eq!(oid, commit);
    }

    #[test]
    fn repository_ref_object_ids_includes_injected_head() {
        let (_dir, repo, _store, commit) = memory_serve_repo();
        let ids = repository_ref_object_ids(&repo).expect("ids");
        assert!(ids.contains(&commit));
    }

    fn seed_linear_commits(repo: &Repository, n: usize) -> Vec<ObjectId> {
        let empty_tree =
            ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
        let _ = repo
            .odb()
            .write_loose_materialize(ObjectKind::Tree, &[] as &[u8]);
        let mut parent: Option<ObjectId> = None;
        let mut chain = Vec::new();
        for i in 1..=n {
            let mut body = format!("tree {}\n", empty_tree.to_hex());
            if let Some(p) = parent {
                body.push_str(&format!("parent {}\n", p.to_hex()));
            }
            body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
            body.push_str(&format!("c{i}\n"));
            let oid = repo
                .odb()
                .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
                .expect("commit");
            chain.push(oid);
            parent = Some(oid);
        }
        if let Some(tip) = chain.last().copied() {
            std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))
                .expect("ref");
        }
        chain
    }

    fn fork_side_commit(repo: &Repository) -> ObjectId {
        let empty_tree =
            ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
        let root_body = format!(
            "tree {}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nroot\n",
            empty_tree.to_hex()
        );
        let root = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, root_body.as_bytes())
            .expect("root");
        let side_body = format!(
            "tree {}\nparent {}\nauthor T <t@e.com> 101 +0000\ncommitter T <t@e.com> 101 +0000\n\nside\n",
            empty_tree.to_hex(),
            root.to_hex()
        );
        repo.odb()
            .write_loose_materialize(ObjectKind::Commit, side_body.as_bytes())
            .expect("side")
    }

    #[test]
    fn dedupe_object_ids_preserves_first_occurrence_order() {
        let a = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let ids = vec![a, b, a, a, b];
        assert_eq!(dedupe_object_ids(&ids), vec![a, b]);
    }

    /// Issue #2587: duplicate stateless wants must not repeat reachability work.
    #[test]
    fn validate_wants_stateless_many_duplicates_of_reachable_ancestor() {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main"),
        )
        .expect("init");
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
        let chain = seed_linear_commits(&repo, 500);
        let tip = chain[chain.len() - 1];
        let root = chain[0];
        let advertised: HashSet<ObjectId> = [tip].into_iter().collect();
        let config = repo.config().snapshot().expect("config");
        let policy = UploadPackPolicy::from_config(config.as_ref());
        let dup_wants: Vec<ObjectId> = (0..5_000).map(|_| root).collect();

        let t0 = Instant::now();
        validate_wants(&repo, &advertised, Some(tip), &[root], &policy, true).expect("single want");
        let single = t0.elapsed();

        let t1 = Instant::now();
        validate_wants(&repo, &advertised, Some(tip), &dup_wants, &policy, true)
            .expect("duplicate wants");
        let many = t1.elapsed();
        assert!(
            many < single * 20 + Duration::from_millis(200),
            "5000 duplicates took {many:?}; one want took {single:?}"
        );
    }

    #[test]
    fn validate_wants_stateless_nested_annotated_tag_refs_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path().join("wt");
        grit_test_support::git(dir.path(), &["init", "-q", &wt.to_string_lossy()]);
        grit_test_support::git(&wt, &["commit", "-q", "--allow-empty", "-m", "c"]);
        grit_test_support::git(&wt, &["tag", "-a", "inner", "-m", "inner"]);
        let inner = grit_test_support::git(&wt, &["rev-parse", "refs/tags/inner"])
            .trim()
            .to_owned();
        grit_test_support::git(&wt, &["tag", "-a", "outer", "-m", "outer", &inner]);
        let outer = ObjectId::from_hex(
            grit_test_support::git(&wt, &["rev-parse", "refs/tags/outer"]).trim(),
        )
        .expect("outer");
        std::fs::remove_file(wt.join(".git/refs/tags/inner")).expect("drop inner");
        std::fs::remove_file(wt.join(".git/refs/tags/outer")).expect("drop outer");

        let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).expect("open");
        let tip = ObjectId::from_hex(grit_test_support::git(&wt, &["rev-parse", "HEAD"]).trim())
            .expect("tip");
        let advertised: HashSet<ObjectId> = [tip].into_iter().collect();
        let config = repo.config().snapshot().expect("config");
        let policy = UploadPackPolicy::from_config(config.as_ref());
        validate_wants(&repo, &advertised, Some(tip), &[outer], &policy, true)
            .expect("nested annotated tag want must validate");
    }

    #[test]
    fn validate_wants_stateless_many_duplicates_unreachable_fail_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main"),
        )
        .expect("init");
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
        let chain = seed_linear_commits(&repo, 5);
        let tip = chain[chain.len() - 1];
        let unreachable = fork_side_commit(&repo);
        let advertised: HashSet<ObjectId> = [tip].into_iter().collect();
        let config = repo.config().snapshot().expect("config");
        let policy = UploadPackPolicy::from_config(config.as_ref());
        let dup_wants: Vec<ObjectId> = (0..2_000).map(|_| unreachable).collect();

        let t0 = Instant::now();
        let err = validate_wants(&repo, &advertised, Some(tip), &dup_wants, &policy, true)
            .expect_err("unreachable");
        let elapsed = t0.elapsed();
        assert!(err.to_string().contains("not our ref"), "unexpected: {err}");
        assert!(
            err.to_string().contains(&unreachable.to_hex()),
            "unexpected: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "unreachable duplicates should fail quickly, took {elapsed:?}"
        );
    }
}
