// API docs: https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/fn.stage.html
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None)?;
    let outcome = stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )?;
    println!(
        "staged {} paths ({} new, {} modified, {} removed)",
        outcome.total(),
        outcome.added,
        outcome.modified,
        outcome.removed
    );
    Ok(())
}
