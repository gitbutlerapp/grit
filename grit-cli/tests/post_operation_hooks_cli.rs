//! `grit switch`, `merge`, `commit --amend` and `rebase` run the same post-operation hooks as
//! git (issue #1816), checked by running one sequence with each tool and comparing hook logs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const HOOKS: [&str; 4] = ["post-checkout", "post-merge", "post-rewrite", "post-commit"];

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Git,
    Grit,
}

struct Repo {
    dir: tempfile::TempDir,
    log: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join(".git").join("hook.log");
        let repo = Self { dir, log };
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo.git(&["config", "user.name", "Hook Tester"]);
        repo.git(&["config", "user.email", "hooks@grit.test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        for hook in HOOKS {
            let path = repo.wt().join(".git/hooks").join(hook);
            std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho \"{hook} $*\" >> '{log}'\n\
                     if [ {hook} = post-rewrite ]; then\n\
                     while read -r line; do echo \"  $line\" >> '{log}'; done\nfi\n",
                    log = repo.log.display()
                ),
            )
            .expect("write hook");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        repo
    }

    fn wt(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, program: &str, args: &[&str]) -> String {
        let out = Command::new(program)
            .args(args)
            .current_dir(self.wt())
            .output()
            .expect("spawn");
        assert!(
            out.status.success(),
            "{program} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// System git with hooks disabled, for setup and inspection.
    fn git(&self, args: &[&str]) -> String {
        let mut full = vec!["-c", "core.hooksPath=/dev/null"];
        full.extend_from_slice(args);
        self.run("git", &full)
    }

    /// Run `args` with `tool`, hooks enabled.
    fn tool(&self, tool: Tool, args: &[&str]) -> String {
        match tool {
            Tool::Git => self.run("git", args),
            Tool::Grit => self.run(GRIT, args),
        }
    }

    fn commit(&self, path: &str, contents: &str, msg: &str) {
        std::fs::write(self.wt().join(path), contents).expect("write");
        self.git(&["add", path]);
        self.git(&["commit", "-qm", msg]);
    }

    /// Hook log with every object id replaced by its commit subject (merge subjects become
    /// `merge`, the null id becomes `null`), so logs from both tools compare.
    fn labelled_log(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .map(|line| {
                line.split(' ')
                    .map(|word| self.label(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn label(&self, word: &str) -> String {
        if word.len() != 40 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return word.to_owned();
        }
        if word.bytes().all(|b| b == b'0') {
            return "null".into();
        }
        let subject = self.git(&["log", "-1", "--format=%s", word]);
        let subject = subject.trim();
        if subject.starts_with("Merge") {
            "merge".into()
        } else {
            subject.to_owned()
        }
    }
}

/// The reproduction from issue #1816.
fn issue_sequence(tool: Tool) -> Vec<String> {
    let repo = Repo::new();
    repo.commit("f", "1\n", "c1");
    repo.git(&["branch", "side"]);
    repo.commit("f", "2\n", "c2");
    repo.git(&["switch", "-q", "side"]);
    repo.commit("s", "s\n", "s1");
    repo.git(&["switch", "-q", "main"]);

    repo.tool(tool, &["switch", "side"]);
    repo.tool(tool, &["switch", "-c", "nb"]);
    repo.tool(tool, &["switch", "main"]);
    match tool {
        Tool::Git => repo.tool(tool, &["merge", "-q", "--no-edit", "side"]),
        Tool::Grit => repo.tool(tool, &["merge", "side"]),
    };
    repo.tool(tool, &["commit", "--amend", "-m", "amended"]);
    repo.git(&["switch", "-q", "side"]);
    repo.tool(tool, &["rebase", "main"]);
    repo.labelled_log()
}

#[test]
fn issue_sequence_runs_the_same_hooks_as_git() {
    let git = issue_sequence(Tool::Git);
    let grit = issue_sequence(Tool::Grit);
    assert_eq!(
        git,
        [
            "post-checkout c2 s1 1",
            "post-checkout s1 s1 1",
            "post-checkout s1 c2 1",
            "post-merge 0",
            "post-commit ",
            "post-rewrite amend",
            "  merge amended",
            "post-checkout s1 amended 1",
        ],
        "system git reference log changed"
    );
    assert_eq!(grit, git);
}

/// Rebase two commits onto a diverged `main`.
fn real_rebase(tool: Tool) -> Vec<String> {
    let repo = Repo::new();
    repo.commit("f", "1\n", "c1");
    repo.git(&["switch", "-q", "-c", "topic"]);
    repo.commit("t1", "t1\n", "t1");
    repo.commit("t2", "t2\n", "t2");
    repo.git(&["switch", "-q", "main"]);
    repo.commit("f", "2\n", "c2");
    repo.git(&["switch", "-q", "topic"]);
    repo.tool(tool, &["rebase", "main"]);
    repo.labelled_log()
}

#[test]
fn rebase_reports_rewritten_pairs_like_git() {
    let git = real_rebase(Tool::Git);
    let grit = real_rebase(Tool::Grit);
    // Git checks out the onto commit before replaying and runs post-commit for every replayed
    // commit; grit applies the rebase atomically, so its post-checkout names the final tip and
    // there is no per-commit post-commit. Both start from the old tip and report the same
    // rewritten pairs.
    assert_eq!(
        git,
        [
            "post-checkout t2 c2 1",
            "post-commit ",
            "post-commit ",
            "post-rewrite rebase",
            "  t1 t1",
            "  t2 t2",
        ],
        "system git reference log changed"
    );
    assert_eq!(
        grit,
        [
            "post-checkout t2 t2 1",
            "post-rewrite rebase",
            "  t1 t1",
            "  t2 t2",
        ]
    );
}

#[test]
fn switch_json_lists_post_checkout() {
    let repo = Repo::new();
    repo.commit("f", "1\n", "c1");
    repo.git(&["branch", "side"]);
    let out = repo.tool(Tool::Grit, &["switch", "side", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(json["hooks_run"], serde_json::json!(["post-checkout"]));

    std::fs::remove_file(repo.wt().join(".git/hooks/post-checkout")).expect("rm hook");
    let out = repo.tool(Tool::Grit, &["switch", "main", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(json["hooks_run"], serde_json::json!([]));
}

#[test]
fn amend_json_lists_post_rewrite() {
    let repo = Repo::new();
    repo.commit("f", "1\n", "c1");
    let out = repo.tool(
        Tool::Grit,
        &["commit", "--amend", "-m", "c1 again", "--json"],
    );
    let json: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        json["hooks_run"],
        serde_json::json!(["post-commit", "post-rewrite"])
    );
}
