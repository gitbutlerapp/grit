//! s1 acceptance: `rev-list --objects v1.0.0` on upstream git.git within 10× of the 6.6s baseline.
//!
//! Requires a bare clone at `GRIT_GIT_GIT_BARE` or `/tmp/git.git`.
//!
//! Run (release only): `GRIT_S1_BENCH=1 cargo test -p grit-lib --release --test rev_list_s1_git`

use std::time::Instant;

use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};

fn git_git_bare() -> std::path::PathBuf {
    std::env::var("GRIT_GIT_GIT_BARE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/git.git"))
}

#[test]
fn rev_list_objects_v1_s1_median_under_ceiling() {
    if std::env::var("GRIT_S1_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIP rev_list_objects_v1_s1_median_under_ceiling (set GRIT_S1_BENCH=1)");
        return;
    }
    let bare = git_git_bare();
    if !bare.is_dir() {
        panic!(
            "GRIT_S1_BENCH=1 but bare repo missing at {}",
            bare.display()
        );
    }
    let repo = Repository::open(&bare, None).expect("open git.git bare");
    let opts = RevListOptions {
        objects: true,
        ..Default::default()
    };
    let mut times = Vec::with_capacity(7);
    for _ in 0..7 {
        let start = Instant::now();
        let result = rev_list(&repo, &[String::from("v1.0.0")], &[], &opts).expect("rev-list");
        assert!(
            !result.objects.is_empty(),
            "expected objects from v1.0.0 walk"
        );
        times.push(start.elapsed().as_secs_f64());
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let median = times[times.len() / 2];
    const BASELINE_S: f64 = 6.6;
    const CEILING_S: f64 = BASELINE_S / 10.0;
    assert!(
        median <= CEILING_S,
        "s1 median {median:.3}s exceeds ceiling {CEILING_S:.3}s (7 samples: {times:?})"
    );
}
