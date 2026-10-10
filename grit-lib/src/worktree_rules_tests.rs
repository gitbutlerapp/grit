//! Tests for per-operation [`crate::worktree_rules::WorktreeRules`] loading.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use crate::diff::diff_index_to_worktree_with_options;
    use crate::diff::diff_tree_to_worktree_with_git_dir_and_rules;
    use crate::diff::DiffIndexToWorktreeOptions;
    use crate::index::{entry_from_stat, MODE_REGULAR};
    use crate::porcelain::add::{stage, StageOptions};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::NullProgress;
    use crate::repo::{init_repository, Repository};
    use crate::worktree_rules::file_load_counters;
    use crate::write_tree::write_tree_from_index;

    fn init_repo(root: &Path) -> Repository {
        init_repository(root, false, "main", None, crate::RefStorageFormat::Files).expect("init")
    }

    fn nested_attributes_fixture(root: &Path) -> Repository {
        let repo = init_repo(root);
        let wt = repo.work_tree.as_ref().expect("work tree");
        fs::write(wt.join(".gitattributes"), "* text=auto\n").unwrap();
        fs::write(wt.join(".gitignore"), "ignored-root\n").unwrap();
        for d in 0..50 {
            let dir = wt.join(format!("dir{d:02}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".gitattributes"), format!("dir{d:02}/* eol=lf\n")).unwrap();
            fs::write(dir.join(".gitignore"), format!("ignored-{d}\n")).unwrap();
            for f in 0..40 {
                let rel = format!("dir{d:02}/file{f:04}.txt");
                let body = if f % 3 == 0 {
                    format!("line\r\n")
                } else {
                    format!("body-{d}-{f}\n")
                };
                fs::write(wt.join(&rel), body).unwrap();
            }
        }
        repo
    }

    #[test]
    fn worktree_rules_load_each_attributes_file_once() {
        let tmp = TempDir::new().unwrap();
        let repo = nested_attributes_fixture(tmp.path());

        file_load_counters::reset();
        status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        assert!(
            file_load_counters::max_reads_per_file() <= 1,
            "status: each attributes/ignore file read at most once (max={})",
            file_load_counters::max_reads_per_file()
        );

        file_load_counters::reset();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert!(
            file_load_counters::max_reads_per_file() <= 1,
            "stage: each attributes/ignore file read at most once (max={})",
            file_load_counters::max_reads_per_file()
        );

        file_load_counters::reset();
        let mut index = repo.load_index().unwrap();
        let wt = repo.work_tree.as_ref().unwrap();
        let rules = std::sync::Arc::new(std::sync::Mutex::new(
            crate::worktree_rules::WorktreeRules::from_repository(&repo, &index).unwrap(),
        ));
        diff_index_to_worktree_with_options(
            &repo.odb,
            &mut index,
            wt,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir.clone()),
                worktree_rules: Some(rules),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            file_load_counters::max_reads_per_file() <= 1,
            "diff: each attributes/ignore file read at most once (max={})",
            file_load_counters::max_reads_per_file()
        );

        let probe_tmp = TempDir::new().unwrap();
        let probe_repo = attr_magic_probe_fixture(probe_tmp.path());
        let probe_wt = probe_repo.work_tree.as_ref().unwrap();
        let root_ga = probe_wt.join(".gitattributes");
        file_load_counters::reset();
        status(
            &probe_repo,
            &StatusOptions {
                pathspecs: vec![":(attr:probe)".to_string()],
                ..StatusOptions::default()
            },
            &mut NullProgress,
        )
        .unwrap();
        assert!(
            file_load_counters::max_reads_per_file() <= 1,
            "attr-magic status: each attributes file read at most once (max={})",
            file_load_counters::max_reads_per_file()
        );
        assert!(
            file_load_counters::reads_for(&root_ga) >= 1,
            "attr-magic status must load root .gitattributes"
        );
    }

    fn attr_magic_probe_fixture(root: &Path) -> Repository {
        let repo = init_repo(root);
        let wt = repo.work_tree.as_ref().expect("work tree");
        fs::write(wt.join(".gitattributes"), "* probe\n").unwrap();
        for i in 0..50 {
            fs::write(wt.join(format!("probe{i:02}.txt")), format!("body-{i}\n")).unwrap();
        }
        repo
    }

    #[test]
    fn diff_tree_to_worktree_nested_eol_attributes_matches_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let wt = repo.work_tree.as_ref().unwrap();
        fs::create_dir_all(wt.join("d")).unwrap();
        fs::write(wt.join("d/.gitattributes"), "*.txt text eol=crlf\n").unwrap();
        fs::write(wt.join("d/f.txt"), b"line one\nline two\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut index = repo.load_index().unwrap();
        let tree_oid = write_tree_from_index(&repo.odb, &index, "").expect("tree");

        for args in [
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "t"][..],
            &["add", "-A"][..],
            &["commit", "-qm", "seed"][..],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        }

        fs::write(wt.join("d/f.txt"), b"line one\r\nline two\r\n").unwrap();
        let blob_oid = index.get(b"d/f.txt", 0).unwrap().oid;
        index.add_or_replace(
            entry_from_stat(&wt.join("d/f.txt"), b"d/f.txt", blob_oid, MODE_REGULAR).unwrap(),
        );

        let rules = crate::worktree_rules::WorktreeRules::from_repository(&repo, &index).unwrap();
        let entries = diff_tree_to_worktree_with_git_dir_and_rules(
            &repo.odb,
            Some(&tree_oid),
            wt,
            &repo.git_dir,
            &index,
            Some(rules.config()),
            Some(&rules),
        )
        .unwrap();
        assert!(
            entries.is_empty(),
            "nested eol=crlf should treat CRLF worktree as clean: {entries:?}"
        );

        let git_diff = Command::new("git")
            .args(["diff", "--", "d/f.txt"])
            .current_dir(root)
            .output()
            .expect("git diff");
        assert!(git_diff.status.success());
        assert!(
            git_diff.stdout.is_empty(),
            "system git diff should be empty: {:?}",
            String::from_utf8_lossy(&git_diff.stdout)
        );
    }

    #[test]
    fn info_attributes_override_nested_gitattributes_for_staging_oid() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/.gitattributes"), "*.txt text eol=lf\n").unwrap();
        fs::write(repo.git_dir.join("info/attributes"), "d/*.txt -text\n").unwrap();
        fs::write(root.join("d/f.txt"), b"a\r\nb\r\n").unwrap();

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let index = repo.load_index().unwrap();
        let oid = index.get(b"d/f.txt", 0).unwrap().oid;
        assert_eq!(
            oid.to_string(),
            "c30dea8a3641ea99b125d04d599d843712292759",
            "info/attributes must win over nested .gitattributes (-text keeps CRLF bytes)"
        );

        let reset = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "reset"])
            .status()
            .expect("git reset");
        assert!(reset.success());
        let git_add = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "add", "-A"])
            .status()
            .expect("git add");
        assert!(git_add.success());
        let git_ls = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "ls-files", "-s", "d/f.txt"])
            .output()
            .expect("git ls-files");
        assert!(git_ls.status.success());
        let git_line = String::from_utf8_lossy(&git_ls.stdout);
        assert!(
            git_line.contains("c30dea8a3641ea99b125d04d599d843712292759"),
            "system git staged OID: {git_line}"
        );
    }

    #[test]
    fn staging_with_eol_attributes_matches_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let wt = repo.work_tree.as_ref().unwrap();

        fs::write(wt.join(".gitattributes"), "* text=auto\n").unwrap();
        for d in 0..5 {
            let dir = wt.join(format!("tree{d}"));
            fs::create_dir_all(&dir).unwrap();
            let attr = if d % 2 == 0 {
                "* eol=lf\n"
            } else {
                "* -text\n"
            };
            fs::write(dir.join(".gitattributes"), attr).unwrap();
            fs::write(
                dir.join("data.txt"),
                if d % 2 == 0 { "a\r\nb\r\n" } else { "bin\x00" },
            )
            .unwrap();
        }

        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let grit_index = repo.load_index().unwrap();
        let grit_by_path: std::collections::BTreeMap<String, String> = grit_index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| {
                let path = String::from_utf8_lossy(&e.path).into_owned();
                (path, e.oid.to_string())
            })
            .collect();

        let reset = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "reset"])
            .status()
            .expect("git reset");
        assert!(reset.success());
        let git_add = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "add", "-A"])
            .status()
            .expect("git add");
        assert!(git_add.success());

        let git_ls = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "ls-files", "-s"])
            .output()
            .expect("git ls-files");
        assert!(git_ls.status.success());
        let git_by_path: std::collections::BTreeMap<String, String> =
            String::from_utf8_lossy(&git_ls.stdout)
                .lines()
                .filter_map(|line| {
                    let (meta, path) = line.split_once('\t')?;
                    let oid = meta.split_whitespace().nth(1)?;
                    Some((path.to_string(), oid.to_string()))
                })
                .collect();

        for path in git_by_path.keys() {
            assert_eq!(
                grit_by_path.get(path),
                git_by_path.get(path),
                "OID mismatch for {path}"
            );
        }
        for path in grit_by_path.keys().filter(|p| p.ends_with("data.txt")) {
            assert!(git_by_path.contains_key(path), "git did not stage {path}");
        }

        let diff_files = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "diff-files"])
            .output()
            .expect("git diff-files");
        assert!(diff_files.status.success());
        assert!(
            diff_files.stdout.is_empty(),
            "git diff-files should be empty after grit stage: {:?}",
            String::from_utf8_lossy(&diff_files.stdout)
        );
    }
}
