// API docs: https://docs.rs/grit-lib/latest/grit_lib/instaweb/index.html
use grit_lib::instaweb::{load_summary, peel_to_commit, recent_commits};
use grit_lib::repo::Repository;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None)?;
    let summary = load_summary(&repo)?;
    println!("repository: {}", summary.name);
    if let Some(head) = summary.head_oid.as_ref() {
        let commit = peel_to_commit(&repo, head)?;
        println!("HEAD commit: {commit}");
    }
    for row in recent_commits(&repo, 5)? {
        println!("{} {}", row.abbrev, row.subject);
    }
    Ok(())
}
