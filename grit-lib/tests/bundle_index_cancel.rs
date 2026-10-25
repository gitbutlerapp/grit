//! Cancellation during bundle pack indexing (after `IndexingObjects` starts).

use grit_lib::bundle::Bundle;
use grit_lib::bundle::BundleError;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::progress::{FlagCancel, ProgressPhase, ProgressSink, ProgressUnit};
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct CancelAfterIndexing {
    flag: FlagCancel,
    saw_indexing: Arc<AtomicBool>,
}

impl ProgressSink for CancelAfterIndexing {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        if phase == ProgressPhase::IndexingObjects {
            self.saw_indexing.store(true, Ordering::Relaxed);
            self.flag.cancel();
        }
        let _ = total;
    }
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.start(phase, total);
        let _ = unit;
    }
}

#[test]
fn bundle_unbundle_returns_cancelled_during_index_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", src.to_str().unwrap()],
    );
    for i in 0..300 {
        std::fs::write(src.join(format!("f{i}.txt")), format!("blob {i}\n")).expect("write");
        git(&src, &["add", "."]);
    }
    git(&src, &["commit", "-q", "-m", "many blobs"]);

    let bundle_path = tmp.path().join("big.bundle");
    git(
        &src,
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    );

    let dest = tmp.path().join("dest");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", dest.to_str().unwrap()],
    );
    git(&dest, &["commit", "-q", "--allow-empty", "-m", "seed"]);

    let repo = Repository::open(
        &dest.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&dest),
    )
    .expect("open");
    let bundle = Bundle::open(&bundle_path).expect("open bundle");
    let flag = FlagCancel::new();
    let saw = Arc::new(AtomicBool::new(false));
    let mut progress = CancelAfterIndexing {
        flag: flag.clone(),
        saw_indexing: Arc::clone(&saw),
    };

    let err = bundle
        .unbundle_with_progress(&repo, &mut progress, &flag)
        .expect_err("cancel during index-pack");
    assert!(
        err.is_cancelled(),
        "expected cancelled bundle error, got {err:?}"
    );
    assert!(
        saw.load(Ordering::Relaxed),
        "IndexingObjects phase must have started before cancel"
    );
}
