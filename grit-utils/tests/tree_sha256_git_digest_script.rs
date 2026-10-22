//! `tree_sha256_git_digest.py` matches grit and handles nested submodule paths.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::TreeSha256Options;

fn git(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn nested_submodule_path_resolves_under_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let sub_src = root.join("sub-src");
    std::fs::create_dir(&sub_src).unwrap();
    git(&sub_src, &["init", "-q", "-b", "main"]);
    std::fs::write(sub_src.join("f.txt"), b"x\n").unwrap();
    git(&sub_src, &["add", "f.txt"]);
    git(&sub_src, &["commit", "-m", "sub"]);
    let sub_head = git(&sub_src, &["rev-parse", "HEAD"]);

    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(
        root.join(".gitmodules"),
        format!(
            "[submodule \"deps/sub\"]\n\tpath = deps/sub\n\turl = file://{}\n",
            sub_src.display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("deps")).unwrap();
    git(
        root,
        &["clone", "-q", sub_src.to_str().unwrap(), "deps/sub"],
    );
    git(root, &["add", ".gitmodules"]);
    Command::new("git")
        .current_dir(root)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},deps/sub"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("gitlink deps/sub");
    git(root, &["commit", "-m", "super"]);

    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/tree_sha256_git_digest.py");
    let py_out = Command::new("python3")
        .arg(&script)
        .arg("git")
        .arg("HEAD")
        .current_dir(root)
        .output()
        .expect("python digest");
    assert!(
        py_out.status.success(),
        "digest script failed: {}",
        String::from_utf8_lossy(&py_out.stderr)
    );
    let script_digest = String::from_utf8_lossy(&py_out.stdout).trim().to_string();

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let grit_digest = repo
        .tree_sha256(head, &TreeSha256Options::default())
        .expect("tree_sha256")
        .digest
        .to_hex();

    assert_eq!(
        script_digest, grit_digest,
        "script vs grit digest mismatch"
    );
}
