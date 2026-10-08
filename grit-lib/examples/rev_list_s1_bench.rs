//! Manual driver for the s1 acceptance scenario (`rev-list --objects v1.0.0` on git.git).
//!
//! Run: `GRIT_GIT_GIT_BARE=/path/to/git.git cargo run -p grit-lib --release --example rev_list_s1_bench`

use std::path::Path;
use std::time::Instant;

use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};

fn git_git_bare() -> std::path::PathBuf {
    std::env::var("GRIT_GIT_GIT_BARE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/git.git"))
}

fn main() -> grit_lib::error::Result<()> {
    let bare = git_git_bare();
    let repo = Repository::open(Path::new(&bare), None)?;
    let opts = RevListOptions {
        objects: true,
        ..Default::default()
    };
    let mut times = Vec::with_capacity(7);
    let mut object_count = 0usize;
    for _ in 0..7 {
        let start = Instant::now();
        let result = rev_list(&repo, &[String::from("v1.0.0")], &[], &opts)?;
        object_count = result.objects.len();
        times.push(start.elapsed().as_secs_f64());
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("finite durations"));
    let median = times[times.len() / 2];
    println!(
        "s1 rev-list --objects v1.0.0: median {median:.3}s (7 runs), {} objects",
        object_count
    );
    Ok(())
}
