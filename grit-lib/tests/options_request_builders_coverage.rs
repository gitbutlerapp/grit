//! Coverage for the constructors and `with_*` setters of the `#[non_exhaustive]` options and
//! request structs: rev walks, ref updates and queries, tag creation, reflog expiry and
//! history rewrites.

use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::ident::Signature;
use grit_lib::merge::merge_file::MergeFavor;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::tag::CreateTagRequest;
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefSort};
use grit_lib::refs::store::{Expected, RawRef, RefName, RefUpdate, RefUpdateFlags, ReflogUpdate};
use grit_lib::repo_handles::ExpireOptions;
use grit_lib::revwalk::history::{
    ApplyOptions, EmptyPolicy, PreRebase, RebaseOntoOptions, RefScope, RewriteOptions,
};
use grit_lib::revwalk::{CommitExtent, MergeFilter, RefSelection, RevWalkListOptions};

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("20-byte oid")
}

fn signature() -> Signature {
    "Builder <builder@example.com> 1700000000 +0000"
        .parse()
        .expect("signature")
}

#[test]
fn rev_walk_list_options_setters_set_each_field() {
    let defaults = RevWalkListOptions::default();
    assert_eq!(defaults.limit, None);
    assert!(defaults.refs.is_empty());

    let opts = RevWalkListOptions::default()
        .with_limit(Some(5))
        .with_skip(2)
        .with_since(Some(10))
        .with_until(Some(20))
        .with_merges(MergeFilter::default())
        .with_reverse(true)
        .with_refs(vec![RefSelection::All])
        .with_first_parent(true)
        .with_pathspec(defaults.pathspec.clone())
        .with_follow(true)
        .with_full_history(true)
        .with_commit_filter(None)
        .with_pickaxe(None)
        .with_graph(true)
        .with_retain_commit_data(true)
        .with_retained_commit_extent(CommitExtent::ThroughSubject)
        .with_left_right(true)
        .with_cherry_mark(true);
    assert_eq!(opts.limit, Some(5));
    assert_eq!(opts.skip, 2);
    assert_eq!(opts.since, Some(10));
    assert_eq!(opts.until, Some(20));
    assert!(opts.reverse);
    assert_eq!(opts.refs.len(), 1);
    assert!(opts.first_parent);
    assert!(opts.follow);
    assert!(opts.full_history);
    assert!(opts.commit_filter.is_none());
    assert!(opts.pickaxe.is_none());
    assert!(opts.graph);
    assert!(opts.retain_commit_data);
    assert!(matches!(
        opts.retained_commit_extent,
        CommitExtent::ThroughSubject
    ));
    assert!(opts.left_right);
    assert!(opts.cherry_mark);
}

#[test]
fn ref_update_new_defaults_and_setters() {
    let name = RefName::from_trusted("refs/heads/topic");
    let update = RefUpdate::new(name.clone(), Some(RawRef::Direct(oid(1))));
    assert_eq!(update.name, name);
    assert_eq!(update.new_value, Some(RawRef::Direct(oid(1))));
    assert_eq!(update.expected, Expected::Any);
    assert_eq!(update.reflog, None);
    assert_eq!(update.flags, RefUpdateFlags::default());

    let reflog = ReflogUpdate::from_signature(&signature(), "create");
    let flags = RefUpdateFlags::default()
        .with_no_deref(true)
        .with_log_only(true)
        .with_skip_oid_verification(true);
    assert!(flags.no_deref && flags.log_only && flags.skip_oid_verification);

    let other = RefName::from_trusted("refs/heads/other");
    let update = update
        .with_name(other.clone())
        .with_new_value(None)
        .with_expected(Expected::Oid(oid(2)))
        .with_reflog(Some(reflog.clone()))
        .with_flags(flags);
    assert_eq!(update.name, other);
    assert_eq!(update.new_value, None);
    assert_eq!(update.expected, Expected::Oid(oid(2)));
    assert_eq!(update.reflog, Some(reflog));
    assert_eq!(update.flags, flags);
}

#[test]
fn ref_query_setters_set_each_field() {
    let query = RefQuery::default()
        .with_namespaces(vec![RefNamespace::Tags])
        .with_patterns(vec!["v1*".to_owned()])
        .with_sort(RefSort::default())
        .with_limit(Some(3))
        .with_contains(vec![oid(1)])
        .with_no_contains(vec![oid(2)])
        .with_merged(vec![oid(3)])
        .with_no_merged(vec![oid(4)])
        .with_points_at(vec![oid(5)]);
    assert_eq!(query.namespaces, vec![RefNamespace::Tags]);
    assert_eq!(query.patterns, vec!["v1*".to_owned()]);
    assert_eq!(query.sort, RefSort::default());
    assert_eq!(query.limit, Some(3));
    assert_eq!(query.contains, vec![oid(1)]);
    assert_eq!(query.no_contains, vec![oid(2)]);
    assert_eq!(query.merged, vec![oid(3)]);
    assert_eq!(query.no_merged, vec![oid(4)]);
    assert_eq!(query.points_at, vec![oid(5)]);
}

#[test]
fn create_tag_request_new_defaults_and_setters() {
    let request = CreateTagRequest::new("v1", oid(1));
    assert_eq!(request.name, "v1");
    assert_eq!(request.target, oid(1));
    assert!(request.message.is_none() && request.tagger.is_none());
    assert!(request.sign.is_none() && request.signing_key.is_none());
    assert!(request.tree_sha256.is_none());
    assert!(!request.force && !request.dry_run);

    let request = request
        .with_name("v2".to_owned())
        .with_target(oid(2))
        .with_message(Some("release\n".to_owned()))
        .with_tagger(Some("T <t@example.com> 1 +0000".to_owned()))
        .with_sign(Some(false))
        .with_signing_key(Some("KEY".to_owned()))
        .with_tree_sha256(None)
        .with_force(true)
        .with_dry_run(true);
    assert_eq!(request.name, "v2");
    assert_eq!(request.target, oid(2));
    assert_eq!(request.message.as_deref(), Some("release\n"));
    assert_eq!(request.tagger.as_deref(), Some("T <t@example.com> 1 +0000"));
    assert_eq!(request.sign, Some(false));
    assert_eq!(request.signing_key.as_deref(), Some("KEY"));
    assert!(request.force && request.dry_run);
}

#[test]
fn expire_options_new_defaults_and_setters() {
    let opts = ExpireOptions::new(1_700_000_000);
    assert_eq!(opts.now, 1_700_000_000);
    assert!(opts.expire.is_none() && opts.expire_unreachable.is_none());
    assert!(!opts.all && !opts.dry_run);

    let opts = opts
        .with_now(5)
        .with_expire(Some("now".to_owned()))
        .with_expire_unreachable(Some("never".to_owned()))
        .with_all(true)
        .with_dry_run(true);
    assert_eq!(opts.now, 5);
    assert_eq!(opts.expire.as_deref(), Some("now"));
    assert_eq!(opts.expire_unreachable.as_deref(), Some("never"));
    assert!(opts.all && opts.dry_run);
}

#[test]
fn rewrite_apply_and_rebase_options_constructors_and_setters() {
    let rewrite = RewriteOptions::new(signature());
    assert_eq!(rewrite.scope, RefScope::Branches);
    assert_eq!(rewrite.empty, EmptyPolicy::Drop);
    assert_eq!(rewrite.committer, signature());
    assert!(!rewrite.preserve_committer);
    assert!(rewrite.sign);

    let rewrite = rewrite
        .with_scope(RefScope::Head)
        .with_empty(EmptyPolicy::Keep)
        .with_committer(signature())
        .with_preserve_committer(true)
        .with_sign(false);
    assert_eq!(rewrite.scope, RefScope::Head);
    assert_eq!(rewrite.empty, EmptyPolicy::Keep);
    assert!(rewrite.preserve_committer);
    assert!(!rewrite.sign);

    let apply = ApplyOptions::new(signature(), "history test");
    assert_eq!(apply.identity, signature());
    assert_eq!(apply.reflog_message, "history test");
    assert!(apply.record_operation.is_none());
    assert_eq!(apply.hooks, PostOperationHooks::Run);
    let apply = apply
        .with_identity(signature())
        .with_reflog_message("other".to_owned())
        .with_record_operation(Some("reword".to_owned()))
        .with_hooks(PostOperationHooks::Skip);
    assert_eq!(apply.reflog_message, "other");
    assert_eq!(apply.record_operation.as_deref(), Some("reword"));
    assert_eq!(apply.hooks, PostOperationHooks::Skip);

    let rebase = RebaseOntoOptions::new(rewrite.clone())
        .with_rewrite(rewrite.clone())
        .with_skip_applied(false)
        .with_flatten_leaf_merges(true)
        .with_favor(MergeFavor::Ours)
        .with_drop(vec![oid(9)])
        .with_pre_rebase(PreRebase::Skip);
    assert_eq!(rebase.rewrite, rewrite);
    assert!(!rebase.skip_applied);
    assert!(rebase.flatten_leaf_merges);
    assert_eq!(rebase.favor, MergeFavor::Ours);
    assert_eq!(rebase.drop, vec![oid(9)]);
    assert_eq!(rebase.pre_rebase, PreRebase::Skip);
}
