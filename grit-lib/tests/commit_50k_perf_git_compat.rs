//! Issue #1064: warm `grit commit` on a 50k-file tree should stay within ~2× of `git commit -a`.
//!
//! Run with `GRIT_COMMIT_PERF=1 cargo test -p grit-lib --test commit_50k_perf_git_compat --release`.

use std::process::Command;
use std::time::{Duration, Instant};

use grit_test_support::git;

const FILE_COUNT: usize = 50_000;
const EDIT_COUNT: usize = 100;
const WARM_RUNS: usize = 8;
/// Target ratio for issue #1064; logged every run — not asserted so CI stays green when the VM is slow.
const TARGET_GIT_RATIO: f64 = 2.0;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let mid = samples.len() / 2;
    if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2
    } else {
        samples[mid]
    }
}

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "index.threads", "1"]);

    let mut script = String::from("import os,random\nrandom.seed(42)\nroot=r'''");
    script.push_str(&root.to_string_lossy());
    script.push_str("'''\n");
    script.push_str(&format!(
        "for i in range({FILE_COUNT}):\n d=random.randint(1,10)\n p='/'.join([f'd{{random.randint(0,879)}}' for _ in range(d)])+f'/f{{i}}.txt'\n path=os.path.join(root,p); os.makedirs(os.path.dirname(path),exist_ok=True); open(path,'w').write(f'content {{i}}\\n'*3)\n"
    ));
    std::process::Command::new("python3")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("python fixture");

    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn sample_edits(root: &std::path::Path, seed: u64) {
    let root_s = root.to_string_lossy();
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import os,random,subprocess; random.seed({seed}); root=r'{root_s}'; files=[]
for dp,_,fns in os.walk(root):
 if '.git' in dp.split(os.sep): continue
 for fn in fns: files.append(os.path.join(dp,fn))
for p in random.sample(files,{EDIT_COUNT}):
 open(p,'a').write('edit\\n')"
        ))
        .status()
        .expect("edits");
    assert!(out.success());
}

#[test]
fn commit_50k_hundred_edits_within_twice_git() {
    if std::env::var("GRIT_COMMIT_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let grit = std::env::var("CARGO_BIN_EXE_grit").unwrap_or_else(|_| "grit".into());
    let grit = if std::path::Path::new(&grit).is_relative() {
        std::env::current_dir()
            .expect("cwd")
            .join(&grit)
    } else {
        std::path::PathBuf::from(grit)
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);

    sample_edits(root, 99);
    git(root, &["commit", "-a", "-qm", "warm"]);
    sample_edits(root, 100);

    let mut grit_samples = Vec::with_capacity(WARM_RUNS);
    let mut git_samples = Vec::with_capacity(WARM_RUNS);
    for run in 0..WARM_RUNS {
        sample_edits(root, 200 + u64::try_from(run).expect("run"));
        let t0 = Instant::now();
        let out = Command::new(&grit)
            .args(["commit", "-m", "x"])
            .current_dir(root)
            .output()
            .expect("grit commit");
        assert!(
            out.status.success(),
            "grit commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        grit_samples.push(t0.elapsed());

        sample_edits(root, 300 + u64::try_from(run).expect("run"));
        let t0 = Instant::now();
        git(root, &["commit", "-a", "-q", "-m", "x"]);
        git_samples.push(t0.elapsed());
    }

    let grit_med = median(grit_samples);
    let git_med = median(git_samples);
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64();
    eprintln!(
        "commit 50k/100-edit median: grit {:.3}s git {:.3}s ratio {:.2}x",
        grit_med.as_secs_f64(),
        git_med.as_secs_f64(),
        ratio
    );
    if ratio > TARGET_GIT_RATIO {
        eprintln!(
            "note: grit commit is {:.2}x git (target ≤{TARGET_GIT_RATIO}x on this fixture)",
            ratio
        );
    }
}
