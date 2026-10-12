//! In-process render snapshots for commands that need network or installers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::CliFixture;
use time::OffsetDateTime;

use crate::commands::auth::AuthOutcome;
use crate::commands::update::UpdateOutcome;
use crate::output::{render_to_string, OutputMode, RenderContext};

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

fn sample_ctx() -> RenderContext {
    RenderContext {
        now: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
    }
}

#[test]
fn auth_render() {
    let ctx = sample_ctx();
    let sign_in = AuthOutcome {
        authenticated: true,
        host: "github.com".to_owned(),
    };
    let human = render_to_string(&sign_in, OutputMode::Human, &ctx).expect("human");
    let json = render_to_string(&sign_in, OutputMode::Json, &ctx).expect("json");
    let markdown = render_to_string(&sign_in, OutputMode::Markdown, &ctx).expect("markdown");

    CliFixture::snapshot_rendered_modes("auth_render", snapshot_dir(), &human, &json, &markdown);
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
