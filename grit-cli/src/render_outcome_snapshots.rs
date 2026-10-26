//! In-process render snapshots for commands that need network or installers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::CliFixture;
use time::OffsetDateTime;

use crate::commands::auth::{AuthOutcome, LogoutOutcome};
use crate::commands::checkin::{
    CheckinBranchJson, CheckinOutcome, CheckinRuleJson, IntegrateOutcome,
};
use crate::commands::checkout::{
    CheckoutOutcome, PullRefreshJson, PullServerBranchJson, RefreshBehindJson, RefreshChangesJson,
    RefreshFollowedJson,
};
use crate::commands::pull::{PullOutcome, PullResult};
use crate::commands::serve_http::{ServeGrantJson, ServeUserJson, ServeUsersOutcome};
use crate::commands::slice::{SliceJson, SliceListOutcome};
use crate::commands::update::UpdateOutcome;
use crate::output::{render_to_string, HumanRender, MarkdownRender, OutputMode, RenderContext};
use grit_test_support::ModeCapture;
use serde::Serialize;

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

fn sample_ctx() -> RenderContext {
    RenderContext {
        now: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
    }
}

#[derive(Serialize)]
struct AuthRenderSnapshot {
    sign_in: AuthModesSnapshot,
    logout: AuthModesSnapshot,
    server_sign_in: AuthModesSnapshot,
    server_logout: AuthModesSnapshot,
}

#[derive(Serialize)]
struct AuthModesSnapshot {
    human: ModeCapture,
    json: ModeCapture,
    markdown: ModeCapture,
}

fn capture_outcome<T: HumanRender + MarkdownRender + serde::Serialize>(
    value: &T,
    ctx: &RenderContext,
) -> AuthModesSnapshot {
    AuthModesSnapshot {
        human: mode_capture(
            &render_to_string(value, OutputMode::Human, ctx).expect("human"),
            0,
        ),
        json: mode_capture(
            &render_to_string(value, OutputMode::Json, ctx).expect("json"),
            0,
        ),
        markdown: mode_capture(
            &render_to_string(value, OutputMode::Markdown, ctx).expect("markdown"),
            0,
        ),
    }
}

fn mode_capture(stdout: &str, exit: i32) -> ModeCapture {
    ModeCapture {
        exit,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

#[test]
fn auth_render() {
    let ctx = sample_ctx();
    let sign_in = AuthOutcome {
        authenticated: true,
        host: "github.com".to_owned(),
        user: None,
    };
    let logout = LogoutOutcome {
        logged_out: true,
        host: "github.com".to_owned(),
        dry_run: false,
        credential: None,
    };
    let server_sign_in = AuthOutcome {
        authenticated: true,
        host: "git.example.com:8080".to_owned(),
        user: Some("bob@example.com".to_owned()),
    };
    let server_logout = LogoutOutcome {
        logged_out: true,
        host: "git.example.com:8080".to_owned(),
        dry_run: false,
        credential: None,
    };
    let payload = AuthRenderSnapshot {
        sign_in: capture_outcome(&sign_in, &ctx),
        logout: capture_outcome(&logout, &ctx),
        server_sign_in: capture_outcome(&server_sign_in, &ctx),
        server_logout: capture_outcome(&server_logout, &ctx),
    };
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path(snapshot_dir());
    settings.set_prepend_module_to_snapshot(false);
    settings.bind(|| {
        insta::assert_json_snapshot!("grit_test_support__cli_fixture__auth_render", payload);
    });
}

#[test]
fn auth_logout_render() {
    let ctx = sample_ctx();
    let logout = LogoutOutcome {
        logged_out: true,
        host: "github.com".to_owned(),
        dry_run: false,
        credential: None,
    };
    let human = render_to_string(&logout, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(&logout, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(&logout, OutputMode::Markdown, &ctx).expect("markdown");
    CliFixture::snapshot_rendered_modes("auth_logout", snapshot_dir(), &human, &json, &markdown);
}

#[test]
fn update_render() {
    let ctx = sample_ctx();
    let outcome = UpdateOutcome {
        updated: true,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let human = render_to_string(&outcome, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(&outcome, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(&outcome, OutputMode::Markdown, &ctx).expect("markdown");

    CliFixture::snapshot_rendered_modes("update_render", snapshot_dir(), &human, &json, &markdown);
}

#[test]
fn checkout_render() {
    let ctx = sample_ctx();
    let outcome = CheckoutOutcome {
        url: "http://127.0.0.1:8080/demo.git".to_owned(),
        path: "demo".to_owned(),
        branch: "main".to_owned(),
        work: "alice/birch-pebble".to_owned(),
        base: "5a80f18e14a38d1fddfb042e0b7724901ae7b30f".to_owned(),
        snapshot: "fd590c7ae90eb06479c29d9640d83e72504483c1".to_owned(),
        paths: vec!["src/deep".to_owned()],
        files: 4,
        bytes: 300_010,
        download_bytes: 300_744,
        resumed_from: 0,
        id: "dbbb39a83cd15c85b5ed".to_owned(),
        seconds: 0.117,
    };
    let human = render_to_string(&outcome, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(&outcome, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(&outcome, OutputMode::Markdown, &ctx).expect("markdown");

    CliFixture::snapshot_rendered_modes(
        "checkout_render",
        snapshot_dir(),
        &human,
        &json,
        &markdown,
    );
}

fn snapshot_modes<T: Serialize + HumanRender + MarkdownRender>(name: &str, outcome: &T) {
    let ctx = sample_ctx();
    let human = render_to_string(outcome, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(outcome, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(outcome, OutputMode::Markdown, &ctx).expect("markdown");
    CliFixture::snapshot_rendered_modes(name, snapshot_dir(), &human, &json, &markdown);
}

fn sample_checkin() -> CheckinOutcome {
    CheckinOutcome {
        url: "http://127.0.0.1:8080/demo.git".to_owned(),
        gave_change_ids: 1,
        branches: vec![
            CheckinBranchJson {
                name: "alice/elm-orchid".to_owned(),
                status: "created".to_owned(),
                grafted: Some("7fb21e5a0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6e".to_owned()),
                previous: None,
                commits: 2,
                rule: None,
                message: None,
                reason: None,
            },
            CheckinBranchJson {
                name: "shared".to_owned(),
                status: "updated".to_owned(),
                grafted: Some("c41d0e2b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d".to_owned()),
                previous: Some("9a4b9113f0c1e4f7d9b2e6a3c8d0f1e2a3b4c5d6".to_owned()),
                commits: 2,
                rule: Some(CheckinRuleJson::KeepsEveryCommit),
                message: None,
                reason: None,
            },
            CheckinBranchJson {
                name: "main".to_owned(),
                status: "refused".to_owned(),
                grafted: None,
                previous: None,
                commits: 0,
                rule: None,
                message: Some(
                    "main is the server's target branch and can't be changed directly".to_owned(),
                ),
                reason: Some("target_branch".to_owned()),
            },
        ],
        refused: true,
    }
}

#[test]
fn checkin_render() {
    snapshot_modes("checkin_render", &sample_checkin());
}

/// The refresh `grit integrate` runs after the merge, as `grit pull` reports it.
fn sample_refresh() -> PullOutcome {
    PullOutcome {
        result: PullResult::Rebased,
        branch: "origin/main".to_owned(),
        oid: Some("2b12f15c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a".to_owned()),
        change_id: None,
        upstream: None,
        conflicts: None,
        conflict_details: None,
        operation: None,
        commit: None,
        next_steps: None,
        alternatives: None,
        autostash: None,
        ahead: None,
        behind: None,
        check: None,
        refresh: Some(PullRefreshJson {
            target: "main".to_owned(),
            upstream_ref: "refs/remotes/origin/main".to_owned(),
            base: "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1".to_owned(),
            snapshot: "e1d2c3b4a5f60718293a4b5c6d7e8f9012345678".to_owned(),
            previous_base: Some("d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4".to_owned()),
            previous_snapshot: Some("5c892b5783dbdeb57f7228eb6a13ffb2fa17d832".to_owned()),
            moved: true,
            changes: Some(RefreshChangesJson {
                files: 2,
                insertions: 3,
                deletions: 1,
            }),
            dry_run: false,
            followed: vec![RefreshFollowedJson {
                name: "main".to_owned(),
                from: "5c892b5783dbdeb57f7228eb6a13ffb2fa17d832".to_owned(),
                to: "e1d2c3b4a5f60718293a4b5c6d7e8f9012345678".to_owned(),
            }],
            behind: vec![RefreshBehindJson {
                name: "alice/side".to_owned(),
                tip: "8e2d26b0a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6".to_owned(),
            }],
            gave_change_ids: 0,
        }),
        server_branch: None,
        transfer: Default::default(),
    }
}

/// `grit pull` in a checkout whose branch had a commit made with git: it is
/// given a change id before the rebase.
#[test]
fn pull_refresh_render() {
    let mut outcome = sample_refresh();
    if let Some(r) = outcome.refresh.as_mut() {
        r.gave_change_ids = 1;
    }
    snapshot_modes("pull_refresh_render", &outcome);
}

/// `grit pull feature` in a checkout, from another branch: the server's
/// feature as the checkout sees it, with one commit of the user's (made with
/// git, so it got a change id first) rebased on top.
#[test]
fn pull_branch_render() {
    let outcome = PullOutcome {
        result: PullResult::Rebased,
        branch: "origin/feature".to_owned(),
        oid: Some("4c2d9e1b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d".to_owned()),
        change_id: None,
        upstream: None,
        conflicts: None,
        conflict_details: None,
        operation: None,
        commit: None,
        next_steps: None,
        alternatives: None,
        autostash: None,
        ahead: None,
        behind: None,
        check: None,
        refresh: None,
        server_branch: Some(PullServerBranchJson {
            name: "feature".to_owned(),
            upstream_ref: "refs/remotes/origin/feature".to_owned(),
            tip: "bcd6c48a1f2e3d4c5b6a79881726354a5b6c7d8e".to_owned(),
            projected: "19a3116f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b".to_owned(),
            commits: 2,
            made: 1,
            reused: 1,
            previous: Some("8e2d26b0a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6".to_owned()),
            own: 1,
            switched: true,
            was_on: Some("alice/elm-orchid".to_owned()),
            gave_change_ids: 1,
        }),
        transfer: Default::default(),
    };
    snapshot_modes("pull_branch_render", &outcome);
}

/// The branch HEAD was on, merged: it moved to the new snapshot and onto a
/// new work branch.
#[test]
fn integrate_render() {
    let mut refresh = sample_refresh();
    refresh.result = PullResult::Landed;
    refresh.oid = refresh.refresh.as_ref().map(|r| r.snapshot.clone());
    let outcome = IntegrateOutcome {
        name: "alice/elm-orchid".to_owned(),
        target: "main".to_owned(),
        status: "merged".to_owned(),
        previous: "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4".to_owned(),
        head: "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1".to_owned(),
        commits: 1,
        checkin: None,
        refresh: Some(refresh),
        moved_to: Some("alice/maple-elm".to_owned()),
        follows: None,
        refresh_error: None,
    };
    snapshot_modes("integrate_render", &outcome);
}

/// A merge into another branch than the one the checkout follows (the
/// server's HEAD moved): the branch keeps its commits here.
#[test]
fn integrate_elsewhere_render() {
    let mut refresh = sample_refresh();
    refresh.result = PullResult::UpToDate;
    refresh.oid = Some("5c892b5783dbdeb57f7228eb6a13ffb2fa17d832".to_owned());
    if let Some(r) = refresh.refresh.as_mut() {
        r.base = "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4".to_owned();
        r.snapshot = "5c892b5783dbdeb57f7228eb6a13ffb2fa17d832".to_owned();
        r.moved = false;
        r.changes = None;
        r.followed.clear();
        r.behind.clear();
    }
    let outcome = IntegrateOutcome {
        name: "alice/elm-orchid".to_owned(),
        target: "develop".to_owned(),
        status: "merged".to_owned(),
        previous: "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4".to_owned(),
        head: "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1".to_owned(),
        commits: 2,
        checkin: None,
        refresh: Some(refresh),
        moved_to: None,
        follows: Some("main".to_owned()),
        refresh_error: None,
    };
    snapshot_modes("integrate_elsewhere_render", &outcome);
}

/// A merge whose refresh failed: the merge is still reported.
#[test]
fn integrate_refresh_failed_render() {
    let outcome = IntegrateOutcome {
        name: "alice/elm-orchid".to_owned(),
        target: "main".to_owned(),
        status: "merged".to_owned(),
        previous: "d08f98a93fcf6c0bd1f4d7e1c5a0b9e8f7d6c5b4".to_owned(),
        head: "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1".to_owned(),
        commits: 1,
        checkin: None,
        refresh: None,
        moved_to: None,
        follows: None,
        refresh_error: Some("local changes would be overwritten: src/deep/b.rs".to_owned()),
    };
    snapshot_modes("integrate_refresh_failed_render", &outcome);
}

#[test]
fn slice_render() {
    let outcome = SliceListOutcome {
        url: "http://127.0.0.1:8080/demo.git".to_owned(),
        target: "main".to_owned(),
        head: "2f44ddc44831f89c598c98491de99dab2af647c8".to_owned(),
        slices: vec![SliceJson {
            name: "desktop".to_owned(),
            description: Some("The desktop app and its UI package".to_owned()),
            paths: vec!["apps/desktop".to_owned(), "packages/ui".to_owned()],
        }],
    };
    snapshot_modes("slice_render", &outcome);
}

#[test]
fn serve_render() {
    let ctx = sample_ctx();
    let outcome = ServeUsersOutcome {
        database: "/srv/git/grit-serve.db".to_owned(),
        users: vec![
            ServeUserJson {
                email: "admin@example.com".to_owned(),
                admin: true,
                temporary_password: false,
                tokens: 1,
                grants: Vec::new(),
            },
            ServeUserJson {
                email: "bob@example.com".to_owned(),
                admin: false,
                temporary_password: true,
                tokens: 2,
                grants: vec![
                    ServeGrantJson {
                        repository: "app".to_owned(),
                        access: "write".to_owned(),
                    },
                    ServeGrantJson {
                        repository: "docs".to_owned(),
                        access: "read".to_owned(),
                    },
                ],
            },
        ],
    };
    let human = render_to_string(&outcome, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(&outcome, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(&outcome, OutputMode::Markdown, &ctx).expect("markdown");

    CliFixture::snapshot_rendered_modes("serve_render", snapshot_dir(), &human, &json, &markdown);
}
