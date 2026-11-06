//! Ref-name policy matrix ported from upstream `t1402-check-ref-format.sh`.

#![allow(clippy::unwrap_used)]

use grit_lib::refs::check_ref_format::{RefName, RefNamePolicy, RefNameRef};
use std::process::Command;

mod refname_policy_data;
use refname_policy_data::{
    T1402Expect, T1402_CASES, T1402_NORMALIZE_INVALID, T1402_NORMALIZE_VALID,
};

fn policy_from_opts(opts: &[&str]) -> RefNamePolicy {
    let mut policy = RefNamePolicy::default();
    for opt in opts {
        match *opt {
            "--allow-onelevel" => policy = policy.allow_onelevel(),
            "--refspec-pattern" => policy = policy.refspec_pattern(),
            "--normalize" => policy = policy.normalize(),
            other => panic!("unknown t1402 option {other:?}"),
        }
    }
    policy
}

fn grit_accepts(ref_name: &str, policy: &RefNamePolicy, normalize: bool) -> bool {
    if normalize {
        RefName::parse(ref_name, policy).is_ok()
    } else {
        RefNameRef::parse(ref_name, policy).is_ok()
    }
}

fn grit_normalized(ref_name: &str, opts: &[&str]) -> Option<String> {
    let policy = policy_from_opts(opts);
    if !opts.contains(&"--normalize") {
        return None;
    }
    RefName::parse(ref_name, &policy)
        .ok()
        .map(|n| n.as_str().to_owned())
}

#[test]
fn t1402_matrix_matches_grit_policy() {
    for case in T1402_CASES {
        if case.prereq == Some("MINGW") {
            continue;
        }
        let policy = policy_from_opts(case.opts);
        let normalize = case.opts.contains(&"--normalize");
        let ok = grit_accepts(case.ref_name, &policy, normalize);
        match case.expect {
            T1402Expect::Valid => assert!(ok, "expected valid: {:?}", case.ref_name),
            T1402Expect::Invalid => assert!(!ok, "expected invalid: {:?}", case.ref_name),
        }
    }
    assert!(
        T1402_CASES.len() >= 80,
        "t1402 corpus should have at least 80 cases"
    );
}

#[test]
fn t1402_normalize_pairs_match_git_check_ref_format() {
    if Command::new("git")
        .args(["check-ref-format", "--normalize", "heads/foo"])
        .status()
        .is_err()
    {
        return;
    }
    for (input, want) in T1402_NORMALIZE_VALID {
        let got = grit_normalized(input, &["--normalize"]).expect("grit normalize");
        assert_eq!(got, *want);
        let git_out = Command::new("git")
            .args(["check-ref-format", "--normalize", input])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            git_out.status.success(),
            "git rejected valid normalize {input:?}"
        );
        let git_stdout = String::from_utf8_lossy(&git_out.stdout);
        let git_name = git_stdout.trim();
        assert_eq!(git_name, *want);
        assert_eq!(got.as_str(), git_name);
    }
    for input in T1402_NORMALIZE_INVALID {
        assert!(grit_normalized(input, &["--normalize"]).is_none());
        let git_out = Command::new("git")
            .args(["check-ref-format", "--normalize", input])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            !git_out.status.success(),
            "git accepted invalid normalize {input:?}"
        );
    }
}

#[test]
fn t1402_cross_check_git_check_ref_format() {
    if Command::new("git")
        .args(["check-ref-format", "refs/heads/main"])
        .status()
        .is_err()
    {
        return;
    }
    for case in T1402_CASES {
        if case.prereq == Some("MINGW") {
            continue;
        }
        let mut args = vec!["check-ref-format".to_owned()];
        args.extend(case.opts.iter().map(|s| s.to_string()));
        args.push(case.ref_name.to_owned());
        let git_out = Command::new("git")
            .args(&args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        let git_ok = git_out.status.success();
        let policy = policy_from_opts(case.opts);
        let normalize = case.opts.contains(&"--normalize");
        let grit_ok = grit_accepts(case.ref_name, &policy, normalize);
        assert_eq!(
            git_ok, grit_ok,
            "git/grit disagree on {:?} opts {:?}: git={git_ok} grit={grit_ok}",
            case.ref_name, case.opts
        );
        if git_ok && case.opts.contains(&"--normalize") {
            let git_stdout = String::from_utf8_lossy(&git_out.stdout);
            let git_norm = git_stdout.trim();
            let grit_norm = grit_normalized(case.ref_name, case.opts).expect("grit normalize");
            assert_eq!(git_norm, grit_norm);
        }
    }
}
