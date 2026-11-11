//! Coverage for the constructors and `with_*` setters of the `#[non_exhaustive]` options and
//! request structs: rev walks, ref updates and queries, tag creation, reflog expiry, history
//! rewrites, push, clone, pull, serve, switch, restore, merge, status and ownership checks.

use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::commits::HookPolicy;
use grit_lib::config::environment::Environment;
use grit_lib::diff::merge_file::ConflictStyle;
use grit_lib::diff::merge_trees::DirectoryRenameMode;
use grit_lib::ident::Signature;
use grit_lib::maintenance::fsck::ObjectFsckPolicy;
use grit_lib::merge::merge_file::MergeFavor;
use grit_lib::merge::{FastForwardPolicy, MergeCommitMode, MergeOptions};
use grit_lib::objects::ObjectId;
use grit_lib::ownership_check::OwnershipContext;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::pull::{PullMode, PullOptions};
use grit_lib::porcelain::restore::{ConflictSide, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
use grit_lib::porcelain::tag::CreateTagRequest;
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefSort};
use grit_lib::refs::store::{Expected, RawRef, RefName, RefUpdate, RefUpdateFlags, ReflogUpdate};
use grit_lib::remote::clone::{CloneIntoOptions, CloneOptions, CloneTags};
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::serve::{ProtocolVersion, ReceivePolicy, ServeOptions};
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::repo_handles::ExpireOptions;
use grit_lib::revwalk::history::{
    ApplyOptions, EmptyPolicy, PreRebase, RebaseOntoOptions, RefScope, RewriteOptions,
};
use grit_lib::revwalk::{CommitExtent, MergeFilter, RefSelection, RevWalkListOptions};
use grit_lib::status::{IgnoredMode, RenameDetection, StatusOptions, UntrackedMode};
use std::path::PathBuf;

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
    let name = RefName::new("refs/heads/topic").expect("valid ref name");
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

    let other = RefName::new("refs/heads/other").expect("valid ref name");
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

#[test]
fn push_ref_spec_and_push_options_setters_set_each_field() {
    let spec = PushRefSpec::default();
    assert!(spec.src.is_none() && spec.dst.is_empty());
    assert!(!spec.force && !spec.delete && !spec.expect_absent);

    let spec = spec
        .with_src(Some(oid(1)))
        .with_src_ref(Some("refs/heads/main".to_owned()))
        .with_dst("refs/heads/topic".to_owned())
        .with_force(true)
        .with_delete(true)
        .with_expected_old(Some(oid(2)))
        .with_expect_absent(true)
        .with_hook_src_ref(Some("HEAD".to_owned()));
    assert_eq!(spec.src, Some(oid(1)));
    assert_eq!(spec.src_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(spec.dst, "refs/heads/topic");
    assert!(spec.force && spec.delete && spec.expect_absent);
    assert_eq!(spec.expected_old, Some(oid(2)));
    assert_eq!(spec.hook_src_ref.as_deref(), Some("HEAD"));

    let opts = PushOptions::default();
    assert!(!opts.atomic && !opts.dry_run && !opts.no_verify);
    assert!(opts.push_options.is_empty() && opts.tracking_remote.is_none());

    let opts = opts
        .with_atomic(true)
        .with_dry_run(true)
        .with_tracking_remote(Some("origin".to_owned()))
        .with_push_options(vec!["ci.skip".to_owned()])
        .with_diagnostics(None)
        .with_network_trace(true)
        .with_remote_ref_tips(Some(vec![oid(3)]))
        .with_resumable_threshold(Some(1024))
        .with_no_verify(true);
    assert!(opts.atomic && opts.dry_run && opts.network_trace && opts.no_verify);
    assert_eq!(opts.tracking_remote.as_deref(), Some("origin"));
    assert_eq!(opts.push_options, vec!["ci.skip".to_owned()]);
    assert!(opts.diagnostics.is_none());
    assert_eq!(opts.remote_ref_tips, Some(vec![oid(3)]));
    assert_eq!(opts.resumable_threshold, Some(1024));
}

#[test]
fn clone_options_new_defaults_and_setters() {
    let opts = CloneOptions::new("https://example.com/r.git", PathBuf::from("r"));
    assert_eq!(opts.url, "https://example.com/r.git");
    assert_eq!(opts.dest, PathBuf::from("r"));
    assert_eq!(opts.initial_branch, "main");
    assert!(opts.checkout && !opts.bare && !opts.auto_filter);
    assert!(opts.depth.is_none() && opts.config.is_empty());

    let opts = opts
        .with_url("/srv/r.git".to_owned())
        .with_dest(PathBuf::from("dest"))
        .with_remote_name("upstream".to_owned())
        .with_environment(Environment::empty())
        .with_initial_branch("trunk".to_owned())
        .with_thread_policy(ThreadPolicy::default())
        .with_depth(Some(1))
        .with_filter(Some(ObjectFilterSpec::BlobNone))
        .with_auto_filter(true)
        .with_config(vec![("core.x".to_owned(), "1".to_owned())])
        .with_branch(Some("dev".to_owned()))
        .with_single_branch(Some(true))
        .with_tags(Some(CloneTags::None))
        .with_promisor_fetcher(None)
        .with_bare(true)
        .with_checkout(false)
        .with_bundle_cache(Some(PathBuf::from("cache")));
    assert_eq!(opts.url, "/srv/r.git");
    assert_eq!(opts.dest, PathBuf::from("dest"));
    assert_eq!(opts.remote_name, "upstream");
    assert_eq!(opts.initial_branch, "trunk");
    assert_eq!(opts.depth, Some(1));
    assert_eq!(opts.filter, Some(ObjectFilterSpec::BlobNone));
    assert!(opts.auto_filter && opts.bare && !opts.checkout);
    assert_eq!(opts.config.len(), 1);
    assert_eq!(opts.branch.as_deref(), Some("dev"));
    assert_eq!(opts.single_branch, Some(true));
    assert_eq!(opts.tags, Some(CloneTags::None));
    assert!(opts.promisor_fetcher.is_none());
    assert_eq!(opts.bundle_cache, Some(PathBuf::from("cache")));
}

#[test]
fn clone_into_options_new_defaults_and_setters() {
    let opts = CloneIntoOptions::new("/srv/r.git");
    assert_eq!(opts.url, "/srv/r.git");
    assert_eq!(opts.initial_branch, "main");
    assert!(opts.checkout && !opts.bare);

    let opts = opts
        .with_url("/srv/other.git".to_owned())
        .with_remote_name("upstream".to_owned())
        .with_environment(Environment::empty())
        .with_initial_branch("trunk".to_owned())
        .with_thread_policy(ThreadPolicy::default())
        .with_depth(Some(2))
        .with_filter(Some(ObjectFilterSpec::BlobLimit(10)))
        .with_branch(Some("dev".to_owned()))
        .with_single_branch(Some(false))
        .with_tags(Some(CloneTags::All))
        .with_promisor_fetcher(None)
        .with_bare(true)
        .with_checkout(false)
        .with_bundle_cache(Some(PathBuf::from("cache")));
    assert_eq!(opts.url, "/srv/other.git");
    assert_eq!(opts.remote_name, "upstream");
    assert_eq!(opts.initial_branch, "trunk");
    assert_eq!(opts.depth, Some(2));
    assert_eq!(opts.filter, Some(ObjectFilterSpec::BlobLimit(10)));
    assert_eq!(opts.branch.as_deref(), Some("dev"));
    assert_eq!(opts.single_branch, Some(false));
    assert_eq!(opts.tags, Some(CloneTags::All));
    assert!(opts.bare && !opts.checkout);
    assert_eq!(opts.bundle_cache, Some(PathBuf::from("cache")));
}

#[test]
fn pull_options_setters_set_each_field() {
    let opts = PullOptions::default();
    assert!(opts.remote.is_none() && opts.mode.is_none() && opts.upstream.is_none());
    assert!(!opts.autostash && !opts.check && !opts.stop_on_conflict);

    let opts = opts
        .with_remote(Some("origin".to_owned()))
        .with_branch(Some("main".to_owned()))
        .with_explicit_fetch(true)
        .with_autostash(true)
        .with_check(true)
        .with_mode(Some(PullMode::Rebase))
        .with_fetch(FetchOptions::new())
        .with_upstream(None)
        .with_stop_on_conflict(true);
    assert_eq!(opts.remote.as_deref(), Some("origin"));
    assert_eq!(opts.branch.as_deref(), Some("main"));
    assert!(opts.explicit_fetch && opts.autostash && opts.check && opts.stop_on_conflict);
    assert_eq!(opts.mode, Some(PullMode::Rebase));
    assert!(opts.upstream.is_none());
}

#[test]
fn serve_options_and_receive_policy_setters_set_each_field() {
    let opts = ServeOptions::default();
    assert_eq!(opts.protocol, ProtocolVersion::default());
    assert!(!opts.stateless_rpc && !opts.advertise_refs && opts.agent.is_empty());

    let opts = opts
        .with_protocol(ProtocolVersion::V2)
        .with_stateless_rpc(true)
        .with_advertise_refs(true)
        .with_agent("test/1".to_owned())
        .with_hidden_refs(vec!["refs/hidden".to_owned()])
        .with_server_options(vec!["opt".to_owned()])
        .with_bundle_list(None)
        .with_push_bundle(Some("bundles".to_owned()))
        .with_accepted_promisor_remotes(vec!["lop".to_owned()])
        .with_lazy_fetch(Some(true));
    assert_eq!(opts.protocol, ProtocolVersion::V2);
    assert!(opts.stateless_rpc && opts.advertise_refs);
    assert_eq!(opts.agent, "test/1");
    assert_eq!(opts.hidden_refs, vec!["refs/hidden".to_owned()]);
    assert_eq!(opts.server_options, vec!["opt".to_owned()]);
    assert!(opts.bundle_list.is_none());
    assert_eq!(opts.push_bundle.as_deref(), Some("bundles"));
    assert_eq!(opts.accepted_promisor_remotes, vec!["lop".to_owned()]);
    assert_eq!(opts.lazy_fetch, Some(true));

    let defaults = ReceivePolicy::default();
    assert!(defaults.deny_delete_current && !defaults.deny_deletes);
    let policy = ReceivePolicy::default()
        .with_deny_non_fast_forwards(true)
        .with_deny_deletes(true)
        .with_deny_current_branch(defaults.deny_current_branch)
        .with_deny_delete_current(false)
        .with_fsck_objects(Some(ObjectFsckPolicy::default()))
        .with_shallow_update(true);
    assert!(policy.deny_non_fast_forwards && policy.deny_deletes && policy.shallow_update);
    assert!(!policy.deny_delete_current);
    assert_eq!(policy.deny_current_branch, defaults.deny_current_branch);
    assert!(policy.fsck_objects.is_some());
}

#[test]
fn switch_and_restore_options_constructors_and_setters() {
    let detached = SwitchMode::Detached {
        commit: oid(1),
        reflog_to_label: "HEAD~1".to_owned(),
    };
    let opts = SwitchOptions::new(detached.clone());
    assert!(!opts.dry_run);
    assert_eq!(opts.hooks, PostOperationHooks::Run);
    let opts = opts
        .with_mode(detached)
        .with_dry_run(true)
        .with_hooks(PostOperationHooks::Skip);
    assert!(opts.dry_run);
    assert_eq!(opts.hooks, PostOperationHooks::Skip);
    assert!(matches!(opts.mode, SwitchMode::Detached { .. }));

    let opts = RestoreOptions::new(vec!["f.txt".to_owned()]);
    assert_eq!(opts.pathspecs, vec!["f.txt".to_owned()]);
    assert!(opts.pathspec_sources.is_empty());
    assert_eq!(opts.source, RestoreSource::Index);
    assert!(!opts.staged && opts.worktree);
    assert!(opts.stage.is_none() && opts.remerge.is_none());
    let opts = opts
        .with_pathspecs(vec!["g.txt".to_owned()])
        .with_pathspec_sources(vec!["./g.txt".to_owned()])
        .with_source(RestoreSource::Tree(oid(2)))
        .with_staged(true)
        .with_worktree(false)
        .with_stage(Some(ConflictSide::Theirs))
        .with_remerge(Some(ConflictStyle::Diff3));
    assert_eq!(opts.pathspecs, vec!["g.txt".to_owned()]);
    assert_eq!(opts.pathspec_sources, vec!["./g.txt".to_owned()]);
    assert_eq!(opts.source, RestoreSource::Tree(oid(2)));
    assert!(opts.staged && !opts.worktree);
    assert_eq!(opts.stage, Some(ConflictSide::Theirs));
    assert_eq!(opts.remerge, Some(ConflictStyle::Diff3));
}

#[test]
fn merge_status_and_ownership_setters_set_each_field() {
    let opts = MergeOptions::default()
        .with_fast_forward(FastForwardPolicy::Only)
        .with_commit(MergeCommitMode::NoCommit)
        .with_message(Some("merge\n".to_owned()))
        .with_favor(MergeFavor::Theirs)
        .with_conflict_style(Some(ConflictStyle::ZealousDiff3))
        .with_allow_unrelated(true)
        .with_hooks(PostOperationHooks::Skip)
        .with_verify(HookPolicy::SkipVerify)
        .with_directory_renames(Some(DirectoryRenameMode::Off));
    assert_eq!(opts.fast_forward, FastForwardPolicy::Only);
    assert_eq!(opts.commit, MergeCommitMode::NoCommit);
    assert_eq!(opts.message.as_deref(), Some("merge\n"));
    assert_eq!(opts.favor, MergeFavor::Theirs);
    assert_eq!(opts.conflict_style, Some(ConflictStyle::ZealousDiff3));
    assert!(opts.allow_unrelated);
    assert_eq!(opts.hooks, PostOperationHooks::Skip);
    assert_eq!(opts.verify, HookPolicy::SkipVerify);
    assert_eq!(opts.directory_renames, Some(DirectoryRenameMode::Off));

    let defaults = StatusOptions::default();
    assert!(defaults.write_index && defaults.ahead_behind && !defaults.count_stash);
    let opts = StatusOptions::default()
        .with_untracked(UntrackedMode::All)
        .with_ignored(IgnoredMode::Traditional)
        .with_renames(Some(
            RenameDetection::default()
                .with_threshold(60)
                .with_copies(true),
        ))
        .with_pathspec(defaults.pathspec.clone())
        .with_ahead_behind(false)
        .with_stat_parallel_threads(Some(1))
        .with_fsmonitor_query_boundary(None)
        .with_now_unix_seconds(Some(1_700_000_000))
        .with_count_stash(true)
        .with_write_index(false);
    assert_eq!(opts.untracked, UntrackedMode::All);
    assert_eq!(opts.ignored, IgnoredMode::Traditional);
    assert_eq!(opts.renames.map(|r| r.threshold), Some(60));
    assert!(!opts.ahead_behind && opts.count_stash && !opts.write_index);
    assert_eq!(opts.stat_parallel_threads, Some(1));
    assert!(opts.fsmonitor_query_boundary.is_none());
    assert_eq!(opts.now_unix_seconds, Some(1_700_000_000));

    let ctx = OwnershipContext::default();
    assert_eq!(ctx.sudo_uid, None);
    assert_eq!(ctx.with_sudo_uid(Some(501)).sudo_uid, Some(501));
}

#[test]
fn process_and_hook_input_setters_set_each_field() {
    use grit_lib::command_runner::hooks::{CommitHookEnv, PrePushRef};
    use grit_lib::command_runner::CommandEnvironment;
    use std::ffi::OsString;
    use std::path::Path;

    let env = CommandEnvironment::default()
        .with_inherit_process(true)
        .with_set(vec![(OsString::from("A"), OsString::from("1"))])
        .with_remove(vec![OsString::from("B")]);
    assert!(env.inherit_process);
    assert_eq!(env.set, vec![(OsString::from("A"), OsString::from("1"))]);
    assert_eq!(env.remove, vec![OsString::from("B")]);

    let extra = [("K", "v")];
    let hook = CommitHookEnv::default()
        .with_index_file(Some(Path::new("index")))
        .with_git_editor(Some(":"))
        .with_git_prefix(Some("sub/"))
        .with_extra_env(&extra);
    assert_eq!(hook.index_file, Some(Path::new("index")));
    assert_eq!(hook.git_editor, Some(":"));
    assert_eq!(hook.git_prefix, Some("sub/"));
    assert_eq!(hook.extra_env, &extra);

    let push = PrePushRef::new(
        Some("refs/heads/main".to_owned()),
        Some(oid(1)),
        "refs/heads/main",
        None,
    );
    assert_eq!(push.local_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(push.local_oid, Some(oid(1)));
    assert_eq!(push.remote_ref, "refs/heads/main");
    assert_eq!(push.remote_oid, None);
    let push = push
        .with_local_ref(None)
        .with_local_oid(None)
        .with_remote_ref("refs/heads/gone".to_owned())
        .with_remote_oid(Some(oid(2)));
    assert_eq!(push.local_ref, None);
    assert_eq!(push.local_oid, None);
    assert_eq!(push.remote_ref, "refs/heads/gone");
    assert_eq!(push.remote_oid, Some(oid(2)));
}

#[test]
fn rendering_input_setters_set_each_field() {
    use grit_lib::config::git_date::{DateMode, DateModeType};
    use grit_lib::revwalk::GraphCharset;

    let mode = DateMode::from_type(DateModeType::Short);
    assert_eq!(mode.ty, DateModeType::Short);
    assert!(!mode.local && mode.strftime_fmt.is_none());
    let mode = mode
        .with_ty(DateModeType::Strftime)
        .with_local(true)
        .with_strftime_fmt(Some("%Y".to_owned()));
    assert_eq!(mode.ty, DateModeType::Strftime);
    assert!(mode.local);
    assert_eq!(mode.strftime_fmt.as_deref(), Some("%Y"));

    let charset = GraphCharset::ASCII
        .with_commit('o')
        .with_pipe(':')
        .with_merge_right('>')
        .with_merge_left('<')
        .with_merge_middle('+');
    assert_eq!(
        (
            charset.commit,
            charset.pipe,
            charset.merge_right,
            charset.merge_left,
            charset.merge_middle
        ),
        ('o', ':', '>', '<', '+')
    );
    assert_ne!(GraphCharset::ASCII, GraphCharset::UNICODE);

    let renames = RenameDetection::default();
    assert_eq!(renames.threshold, 50);
    assert!(!renames.copies);
    let renames = renames.with_threshold(70).with_copies(true);
    assert_eq!(renames.threshold, 70);
    assert!(renames.copies);
}

#[test]
fn walk_and_query_input_setters_set_each_field() {
    use grit_lib::blame::IgnoreRevsSources;
    use grit_lib::refs::listing::{RefQueryOptions, SortKey};
    use grit_lib::refs::reflog::ReflogSelector;
    use grit_lib::revwalk::merge_base::ReachableWalkCommitterCutoffs;
    use grit_lib::worktree::pathspec::PathspecFile;

    let cutoffs = ReachableWalkCommitterCutoffs::default()
        .with_since(Some(10))
        .with_until(Some(20));
    assert_eq!((cutoffs.since, cutoffs.until), (Some(10), Some(20)));

    let file = PathspecFile::new(b"a\0b\0".to_vec(), true)
        .with_data(b"c\n".to_vec())
        .with_nul_terminated(false);
    assert_eq!(file.data, b"c\n".to_vec());
    assert!(!file.nul_terminated);

    let sources = IgnoreRevsSources::default()
        .with_files(vec![".git-blame-ignore-revs".to_owned()])
        .with_revs(vec!["HEAD~1".to_owned()])
        .with_use_config(true);
    assert_eq!(sources.files, vec![".git-blame-ignore-revs".to_owned()]);
    assert_eq!(sources.revs, vec!["HEAD~1".to_owned()]);
    assert!(sources.use_config);

    let sort = RefSort::default();
    assert_eq!(sort.key, SortKey::Name);
    assert!(!sort.reverse);
    let sort = sort.with_key(SortKey::Version).with_reverse(true);
    assert_eq!(sort.key, SortKey::Version);
    assert!(sort.reverse);

    let opts = RefQueryOptions::default()
        .with_versionsort_suffixes(vec!["-rc".to_owned()])
        .with_ignore_case(true);
    assert_eq!(opts.versionsort_suffixes, vec!["-rc".to_owned()]);
    assert!(opts.ignore_case);

    let main = RefName::new("refs/heads/main").expect("ref name");
    let topic = RefName::new("refs/heads/topic").expect("ref name");
    let selector = ReflogSelector::new(main.clone(), 2);
    assert_eq!(selector.ref_name, main);
    assert_eq!(selector.index, 2);
    let selector = selector.with_ref_name(topic.clone()).with_index(0);
    assert_eq!(selector.ref_name, topic);
    assert_eq!(selector.index, 0);
}

#[test]
fn ref_transaction_input_setters_set_each_field() {
    use grit_lib::refs::gc::RefTransactionItem;
    use grit_lib::refs::reftable::{RefValue, ReftableTransactionUpdate};
    use grit_lib::refs::store::RefNameRef;
    use grit_lib::refs::BranchCommitRefUpdate;

    let item = RefTransactionItem::new("refs/heads/main", Some(oid(1)));
    assert_eq!(item.name, "refs/heads/main");
    assert_eq!(item.new_oid, Some(oid(1)));
    assert_eq!(item.expected_old, None);
    let item = item
        .with_name("refs/heads/topic".to_owned())
        .with_new_oid(None)
        .with_expected_old(Some(oid(2)));
    assert_eq!(item.name, "refs/heads/topic");
    assert_eq!(item.new_oid, None);
    assert_eq!(item.expected_old, Some(oid(2)));

    let update = ReftableTransactionUpdate::new("refs/heads/main");
    assert_eq!(update.refname, "refs/heads/main");
    assert!(update.value.is_none() && update.log.is_none() && update.expected_old.is_none());
    let update = update
        .with_refname("refs/heads/topic".to_owned())
        .with_value(Some(RefValue::Val1(oid(3))))
        .with_log(None)
        .with_expected_old(Some(oid(4)));
    assert_eq!(update.refname, "refs/heads/topic");
    assert!(matches!(update.value, Some(RefValue::Val1(o)) if o == oid(3)));
    assert!(update.log.is_none());
    assert_eq!(update.expected_old, Some(oid(4)));

    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
    let reflog = ReflogUpdate::new("A U Thor <a@example.com>", "commit: x", now);
    assert_eq!(reflog.identity, "A U Thor <a@example.com>");
    assert_eq!(reflog.message, "commit: x");
    assert_eq!(reflog.time, now);
    assert!(reflog.old_oid.is_none() && reflog.new_oid.is_none());
    let later = now + time::Duration::seconds(1);
    let reflog = reflog
        .with_identity("B <b@example.com>".to_owned())
        .with_message("reset".to_owned())
        .with_time(later)
        .with_old_oid(Some(oid(5)))
        .with_new_oid(Some(oid(6)));
    assert_eq!(reflog.identity, "B <b@example.com>");
    assert_eq!(reflog.message, "reset");
    assert_eq!(reflog.time, later);
    assert_eq!(
        (reflog.old_oid, reflog.new_oid),
        (Some(oid(5)), Some(oid(6)))
    );

    let main = RefNameRef::new("refs/heads/main").expect("ref name");
    let topic = RefNameRef::new("refs/heads/topic").expect("ref name");
    let branch = BranchCommitRefUpdate::new(main, None, oid(7), "Id <i@example.com>", "commit");
    assert!(branch.branch_ref == main);
    assert_eq!(branch.expected_old, None);
    assert_eq!(branch.new_oid, oid(7));
    assert_eq!(
        (branch.identity, branch.reflog_message),
        ("Id <i@example.com>", "commit")
    );
    let branch = branch
        .with_branch_ref(topic)
        .with_expected_old(Some(oid(7)))
        .with_new_oid(oid(8))
        .with_identity("Other <o@example.com>")
        .with_reflog_message("amend");
    assert!(branch.branch_ref == topic);
    assert_eq!(branch.expected_old, Some(oid(7)));
    assert_eq!(branch.new_oid, oid(8));
    assert_eq!(
        (branch.identity, branch.reflog_message),
        ("Other <o@example.com>", "amend")
    );
}

#[test]
fn branch_merge_and_history_input_setters_set_each_field() {
    use grit_lib::merge::merge_trees::{TheirsConflictLabel, TreeMergeConflictPresentation};
    use grit_lib::merge::{MergeSource, MergeSourceKind};
    use grit_lib::porcelain::branch::StartPoint;
    use grit_lib::porcelain::hunk_select::LineRange;
    use grit_lib::revwalk::history::{CommitEdit, NewCommit};

    let start = StartPoint::new(oid(1), "main");
    assert_eq!((start.oid, start.label.as_str()), (oid(1), "main"));
    let start = start.with_oid(oid(2)).with_label("topic".to_owned());
    assert_eq!((start.oid, start.label.as_str()), (oid(2), "topic"));

    let source = MergeSource::new(oid(3), MergeSourceKind::Commit);
    assert_eq!((source.oid, source.kind), (oid(3), MergeSourceKind::Commit));
    let source = source
        .with_oid(oid(4))
        .with_kind(MergeSourceKind::LocalBranch);
    assert_eq!(
        (source.oid, source.kind),
        (oid(4), MergeSourceKind::LocalBranch)
    );

    let range = LineRange::new("a.txt", 1, 3);
    assert_eq!(
        (range.path.as_str(), range.start, range.end),
        ("a.txt", 1, 3)
    );
    let range = range
        .with_path("b.txt".to_owned())
        .with_start(4)
        .with_end(6);
    assert_eq!(
        (range.path.as_str(), range.start, range.end),
        ("b.txt", 4, 6)
    );

    let defaults = TreeMergeConflictPresentation::default();
    assert_eq!(defaults.label_ours, "HEAD");
    assert_eq!(defaults.style, ConflictStyle::Merge);
    let presentation = defaults
        .with_label_ours("ours")
        .with_label_theirs(TheirsConflictLabel::Fixed("theirs"))
        .with_label_base("base")
        .with_style(ConflictStyle::Diff3)
        .with_checkout_merge(true)
        .with_df_directory_ours_commit(Some(oid(5)))
        .with_df_directory_theirs_commit(Some(oid(6)));
    assert_eq!(presentation.label_ours, "ours");
    assert!(matches!(
        presentation.label_theirs,
        TheirsConflictLabel::Fixed("theirs")
    ));
    assert_eq!(presentation.label_base, "base");
    assert_eq!(presentation.style, ConflictStyle::Diff3);
    assert!(presentation.checkout_merge);
    assert_eq!(presentation.df_directory_ours_commit, Some(oid(5)));
    assert_eq!(presentation.df_directory_theirs_commit, Some(oid(6)));

    let author = "A <a@example.com> 1700000000 +0000";
    let insert = NewCommit::new(oid(7), author, "inserted\n");
    assert_eq!(insert.tree, oid(7));
    assert_eq!(insert.author, author);
    assert_eq!(insert.message, "inserted\n");
    let insert = insert
        .with_tree(oid(8))
        .with_author("B <b@example.com> 1700000000 +0000".to_owned())
        .with_message("other\n".to_owned());
    assert_eq!(insert.tree, oid(8));
    assert_eq!(insert.author, "B <b@example.com> 1700000000 +0000");
    assert_eq!(insert.message, "other\n");

    match CommitEdit::rewrite(
        Some(oid(9)),
        Some("m\n".to_owned()),
        None,
        Some(vec![oid(1)]),
    ) {
        CommitEdit::Rewrite {
            tree,
            message,
            author,
            parents,
            ..
        } => {
            assert_eq!(tree, Some(oid(9)));
            assert_eq!(message.as_deref(), Some("m\n"));
            assert_eq!(author, None);
            assert_eq!(parents, Some(vec![oid(1)]));
        }
        other => panic!("expected Rewrite, got {other:?}"),
    }
    match CommitEdit::split_before(insert.clone(), Some("rest\n".to_owned())) {
        CommitEdit::SplitBefore {
            insert: got,
            rest_message,
            ..
        } => {
            assert_eq!(got, insert);
            assert_eq!(rest_message.as_deref(), Some("rest\n"));
        }
        other => panic!("expected SplitBefore, got {other:?}"),
    }
    assert!(matches!(
        CommitEdit::reuse(oid(2), oid(3)),
        CommitEdit::Reuse { existing, parent, .. } if existing == oid(2) && parent == oid(3)
    ));
}

#[test]
fn transport_and_credential_input_setters_set_each_field() {
    use grit_lib::remote::credentials::Credential;
    use grit_lib::remote::protocol::ProtocolPolicyInputs;
    use grit_lib::remote::transfer::CloneReflog;
    use grit_lib::remote::transport::GitDaemonTransport;
    use std::time::Duration;

    let daemon = GitDaemonTransport::default()
        .with_connect_timeout(Some(Duration::from_secs(1)))
        .with_io_timeout(Some(Duration::from_secs(2)));
    assert_eq!(daemon.connect_timeout, Some(Duration::from_secs(1)));
    assert_eq!(daemon.io_timeout, Some(Duration::from_secs(2)));

    let inputs = ProtocolPolicyInputs::default()
        .with_git_allow_protocol(Some("https".to_owned()))
        .with_git_protocol_from_user(Some("0".to_owned()))
        .with_specific_allow(Some("never".to_owned()))
        .with_blanket_allow(Some("user".to_owned()));
    assert_eq!(inputs.git_allow_protocol.as_deref(), Some("https"));
    assert_eq!(inputs.git_protocol_from_user.as_deref(), Some("0"));
    assert_eq!(inputs.specific_allow.as_deref(), Some("never"));
    assert_eq!(inputs.blanket_allow.as_deref(), Some("user"));

    let reflog = CloneReflog::new(signature(), "clone: from here");
    assert_eq!(reflog.identity, signature());
    assert_eq!(reflog.message, "clone: from here");
    let other: Signature = "Other <o@example.com> 1700000001 +0000"
        .parse()
        .expect("signature");
    let reflog = reflog
        .with_identity(other.clone())
        .with_message("clone: from there".to_owned());
    assert_eq!(reflog.identity, other);
    assert_eq!(reflog.message, "clone: from there");

    let cred = Credential::default()
        .with_protocol(Some("https".to_owned()))
        .with_host(Some("example.com".to_owned()))
        .with_path(Some("repo.git".to_owned()))
        .with_username(Some("ada".to_owned()))
        .with_password(Some("hunter2".to_owned()))
        .with_url(Some("https://example.com/repo.git".to_owned()))
        .with_extra(vec![
            (
                "oauth_refresh_token".to_owned(),
                "refresh-secret".to_owned(),
            ),
            ("capability[]".to_owned(), "authtype".to_owned()),
        ]);
    assert_eq!(cred.protocol.as_deref(), Some("https"));
    assert_eq!(cred.host.as_deref(), Some("example.com"));
    assert_eq!(cred.path.as_deref(), Some("repo.git"));
    assert_eq!(cred.username.as_deref(), Some("ada"));
    assert_eq!(cred.password.as_deref(), Some("hunter2"));
    assert_eq!(cred.url.as_deref(), Some("https://example.com/repo.git"));
    assert_eq!(cred.extra.len(), 2);
    let debug = format!("{cred:?}");
    assert!(!debug.contains("hunter2"), "password leaked: {debug}");
    assert!(!debug.contains("refresh-secret"), "token leaked: {debug}");
    assert!(debug.contains("ada") && debug.contains("capability[]"));
    assert!(debug.contains("<redacted>"));
    assert!(!format!("{:?}", Credential::default()).contains("<redacted>"));
}

#[test]
fn trailer_and_patch_input_setters_set_each_field() {
    use grit_lib::commit::trailers::interpret::{
        NewTrailerArg, TrailerIfExists, TrailerIfMissing, TrailerWhere,
    };
    use grit_lib::diff::{DiffEntry, DiffStatus, PatchResolvedSide, PatchSide};
    use grit_lib::objects::FileMode;

    let arg = NewTrailerArg::new("Acked-by: A");
    assert_eq!(arg.text, "Acked-by: A");
    assert_eq!(arg.where_, TrailerWhere::Default);
    assert_eq!(arg.if_exists, TrailerIfExists::Default);
    assert_eq!(arg.if_missing, TrailerIfMissing::Default);
    let arg = arg
        .with_text("Signed-off-by: B".to_owned())
        .with_where_(TrailerWhere::Start)
        .with_if_exists(TrailerIfExists::Replace)
        .with_if_missing(TrailerIfMissing::DoNothing);
    assert_eq!(arg.text, "Signed-off-by: B");
    assert_eq!(arg.where_, TrailerWhere::Start);
    assert_eq!(arg.if_exists, TrailerIfExists::Replace);
    assert_eq!(arg.if_missing, TrailerIfMissing::DoNothing);

    let entry = DiffEntry::new(
        DiffStatus::Modified,
        Some("a.txt".to_owned()),
        Some("a.txt".to_owned()),
        FileMode::REGULAR,
        FileMode::REGULAR,
        oid(1),
        oid(2),
    );
    let side = PatchResolvedSide::new(entry.clone(), PatchSide::Blob, PatchSide::Worktree);
    assert_eq!(side.entry, entry);
    assert_eq!(side.old, PatchSide::Blob);
    assert_eq!(side.new, PatchSide::Worktree);
    let side = side
        .with_entry(entry.clone())
        .with_old(PatchSide::Bytes(b"old".to_vec()))
        .with_new(PatchSide::Bytes(b"new".to_vec()));
    assert_eq!(side.old, PatchSide::Bytes(b"old".to_vec()));
    assert_eq!(side.new, PatchSide::Bytes(b"new".to_vec()));
}
