//! t3070-wildmatch: grit wildmatch vs upstream vector table and git ls-files cross-check.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use grit_lib::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};
use grit_test_support::{git, git_cmd, unique_tmp};

mod vectors {
    include!("wildmatch_vectors_data.inc");
}
use vectors::{WildmatchVector, PATHOLOGICAL, T3070_VECTORS};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Glob,
    Iglob,
    Pathmatch,
    Ipathmatch,
}

impl Mode {
    /// Git's `test-tool wildmatch` names are inverted: `wildmatch`/`iwildmatch` use
    /// `WM_PATHNAME`; `pathmatch`/`ipathmatch` use plain flags (see upstream `test-wildmatch.c`).
    fn flags(self) -> u32 {
        match self {
            Self::Glob => WM_PATHNAME,
            Self::Iglob => WM_PATHNAME | WM_CASEFOLD,
            Self::Pathmatch => 0,
            Self::Ipathmatch => WM_CASEFOLD,
        }
    }

    fn expected(self, v: &WildmatchVector) -> bool {
        match self {
            Self::Glob => v.glob != 0,
            Self::Iglob => v.iglob != 0,
            Self::Pathmatch => v.pathmatch != 0,
            Self::Ipathmatch => v.ipathmatch != 0,
        }
    }

    const ALL: [Self; 4] = [Self::Glob, Self::Iglob, Self::Pathmatch, Self::Ipathmatch];
}

fn grit_matches(mode: Mode, pattern: &str, text: &str) -> bool {
    wildmatch(pattern.as_bytes(), text.as_bytes(), mode.flags())
}

#[test]
fn t3070_vectors_all_modes() {
    for (idx, v) in T3070_VECTORS.iter().enumerate() {
        for mode in Mode::ALL {
            let got = grit_matches(mode, v.pattern, v.text);
            let want = mode.expected(v);
            assert_eq!(
                got, want,
                "vector #{idx} mode {:?} pattern {:?} text {:?}",
                mode, v.pattern, v.text
            );
        }
    }
}

#[test]
fn t3070_pathological_wildmatch_under_time_bound() {
    let (text, pattern) = PATHOLOGICAL;
    let start = Instant::now();
    let _ = grit_matches(Mode::Glob, pattern, text);
    assert!(
        start.elapsed() < Duration::from_millis(50),
        "pathological wildmatch took {:?}",
        start.elapsed()
    );
}

/// Extra vectors for trailing-space and `\` edge cases (t3070 lines that use `E` or skip file creation).
const EXTRA_VECTORS: &[WildmatchVector] = &[
    // t3070 also has trailing-space rows where test-tool wildmatch disagrees with ls-files;
    // those are covered in `ignore_rules` via check-ignore / pathspec corpus, not here.
    WildmatchVector {
        glob: 0,
        iglob: 0,
        pathmatch: 0,
        ipathmatch: 0,
        text: "XXX/\\",
        pattern: "*/\\",
    },
    WildmatchVector {
        glob: 1,
        iglob: 1,
        pathmatch: 1,
        ipathmatch: 1,
        text: "XXX/\\",
        pattern: "*/\\\\",
    },
];

#[test]
fn t3070_extra_trailing_space_and_backslash_vectors() {
    for v in EXTRA_VECTORS {
        for mode in Mode::ALL {
            assert_eq!(
                grit_matches(mode, v.pattern, v.text),
                mode.expected(v),
                "extra vector mode {:?}",
                mode
            );
        }
    }
}

fn can_create_path(rel: &str) -> bool {
    if rel.is_empty() || rel == "." {
        return false;
    }
    if rel.contains("//") || rel.ends_with('/') {
        return false;
    }
    if rel.contains('\\') && !cfg!(windows) {
        return false;
    }
    true
}

#[test]
fn t3070_ls_files_pathmatch_cross_check() {
    // Plain `git ls-files` pathspecs follow test-tool `pathmatch` (flags 0), not `wildmatch`.
    let tmp = unique_tmp("wildmatch", "ls-files");
    git_cmd(&["init"]).in_dir(&tmp).suc();
    git_cmd(&["config", "core.autocrlf", "false"])
        .in_dir(&tmp)
        .suc();

    let sample: Vec<&WildmatchVector> = T3070_VECTORS
        .iter()
        .filter(|v| can_create_path(v.text) && !v.pattern.is_empty())
        .take(40)
        .collect();

    for v in sample {
        let rel = Path::new(v.text);
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(tmp.join(parent)).expect("mkdir");
            }
        }
        std::fs::write(tmp.join(rel), b"x").expect("touch");
        git_cmd(&["add", "-A"]).in_dir(&tmp).suc();

        let grit_want = grit_matches(Mode::Pathmatch, v.pattern, v.text);
        let out = git_cmd(&["ls-files", "-z", "--", v.pattern])
            .in_dir(&tmp)
            .suc()
            .stdout;
        let listed: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
        let git_got = listed.iter().any(|p| *p == v.text);
        assert_eq!(
            git_got, grit_want,
            "ls-files pathmatch pattern {:?} text {:?} listed={listed:?}",
            v.pattern, v.text
        );

        git_cmd(&["reset", "--hard"]).in_dir(&tmp).suc();
        git_cmd(&["clean", "-fd"]).in_dir(&tmp).suc();
    }
}

#[test]
fn git_wildmatch_via_check_ignore_no_index_glob() {
    // Spot-check that pathname wildmatch used by ignore agrees with git for simple globs.
    let tmp = unique_tmp("wildmatch", "check-ignore-glob");
    git(&tmp, &["init"]);
    std::fs::write(tmp.join(".gitignore"), "*.log\n").unwrap();
    git_cmd(&["add", ".gitignore"]).in_dir(&tmp).suc();
    let out = git_cmd(&["check-ignore", "-v", "--no-index", "build.log"])
        .in_dir(&tmp)
        .suc()
        .stdout;
    assert!(out.contains(".gitignore"));
    assert!(grit_matches(Mode::Pathmatch, "*.log", "build.log"));
}
