//! Git tree-sha256 reproduction (see `tag_tree_sha256_suite`) vs grit.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::tree_sha256::TreeSha256Options;
use grit_utils::tag_tree_sha256_suite::git_plain_tree_sha256_python;

fn git(cwd: &Path, args: &[&str]) -> String {
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

fn run_git_repro(root: &Path, rev: &str) -> String {
    let git_bin = Path::new("git");
    let script = git_plain_tree_sha256_python(git_bin, rev).expect("digest script path");
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .current_dir(root)
        .output()
        .expect("sh repro");
    assert!(
        out.status.success(),
        "git repro failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn grit_digest(root: &Path) -> String {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    repo.tree_sha256(head, &TreeSha256Options::default())
        .expect("tree_sha256")
        .digest
        .to_hex()
}

#[test]
fn nested_checked_out_submodule_path_deps_sub() {
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
        .expect("gitlink");
    git(root, &["commit", "-m", "super"]);

    let sub_wt = root.join("deps/sub");
    let grit_sub = {
        let repo = Repository::discover(Some(&sub_wt), &RepositoryOptions::empty()).expect("sub");
        let oid = grit_lib::objects::ObjectId::from_hex(&sub_head).expect("hex");
        repo.tree_sha256(oid, &TreeSha256Options::default())
            .expect("sub tree_sha256")
            .digest
            .to_hex()
    };
    assert_eq!(run_git_repro(&sub_wt, &sub_head), grit_sub);
    assert_eq!(run_git_repro(root, "HEAD"), grit_digest(root));
}
