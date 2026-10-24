//! In-process render snapshots for commands that need network or installers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::CliFixture;
use time::OffsetDateTime;

use crate::commands::auth::{AuthOutcome, LogoutOutcome};
use crate::commands::serve_http::{ServeGrantJson, ServeUserJson, ServeUsersOutcome};
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
        user: Some("bob".to_owned()),
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
fn serve_render() {
    let ctx = sample_ctx();
    let outcome = ServeUsersOutcome {
        database: "/srv/git/grit-serve.db".to_owned(),
        users: vec![
            ServeUserJson {
                name: "admin".to_owned(),
                admin: true,
                tokens: 1,
                grants: Vec::new(),
            },
            ServeUserJson {
                name: "bob".to_owned(),
                admin: false,
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
