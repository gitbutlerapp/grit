//! Tests for [`Repository::config`] snapshot and hot-path config load counts.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use std::env;

    use crate::config::cascade_load_counters;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use crate::diff::diff_tree_to_worktree_with_git_dir;
    use crate::environment::{Environment, RepositoryOptions};
    use crate::index::{entry_from_stat, Index, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::porcelain::checkout::checkout_between_trees;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::NullProgress;
    use crate::reftable::is_reftable_repo;
    use crate::repo::{init_repository, init_repository_separate_git_dir, Repository};
    use crate::rev_list::{rev_list, RevListOptions};

    fn init_repo(root: &Path) -> Repository {
        init_repository(root, false, "main", None, "files").expect("init")
    }

    fn ident() -> String {
        "Test User <t@example.com> 1 +0000".to_owned()
    }

    fn assert_at_most_config_loads(op: &str, max: usize) {
        assert!(
            cascade_load_counters::total_loads() <= max,
            "{op}: expected at most {max} config cascade loads, got uncached={} validated={}",
            cascade_load_counters::uncached_loads(),
            cascade_load_counters::cache_validated_loads(),
        );
    }

    #[test]
    fn porcelain_ops_load_config_cascade_once() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("README"), b"hello\n").unwrap();

        cascade_load_counters::measure(|| {
            status(&repo, &StatusOptions::default(), &mut NullProgress).unwrap();
        });
        assert_at_most_config_loads("status", 1);

        let stage_root = root.join("stage-repo");
        let repo2 = init_repo(&stage_root);
        fs::write(stage_root.join("README2"), b"hello2\n").unwrap();
        cascade_load_counters::measure(|| {
            stage(&repo2, &StageOptions::default(), &mut NullProgress).unwrap();
        });
        assert_at_most_config_loads("stage", 1);

        let commit_root = root.join("commit-repo");
        let repo3 = init_repo(&commit_root);
        fs::write(commit_root.join("c.txt"), b"x\n").unwrap();
        stage(&repo3, &StageOptions::default(), &mut NullProgress).unwrap();
        let req = CommitRequest {
            message: "c".into(),
            author: ident(),
            committer: ident(),
            allow_empty: false,
        };
        cascade_load_counters::measure(|| {
            create_commit(&repo3, &req, &mut NullProgress).unwrap();
        });
        // Index write, ref update, and object-format / MIDX config probes may each revalidate
        // the repository config cache once when using a shared repository snapshot.
        assert_at_most_config_loads("create_commit", 4);

        let head = crate::refs::resolve_ref(&repo3.git_dir, "HEAD").unwrap();
        let parent_tree = {
            let obj = repo3.odb.read(&head).unwrap();
            crate::objects::parse_commit(&obj.data).unwrap().tree
        };
        fs::write(commit_root.join("other.txt"), b"y\n").unwrap();
        stage(&repo3, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut index = repo3.load_index().unwrap();
        let tree2 = crate::write_tree::write_tree_update_index(
            &repo3.odb,
            &mut index,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .unwrap();

        cascade_load_counters::measure(|| {
            checkout_between_trees(&repo3, Some(&parent_tree), &tree2).unwrap();
        });
        assert_at_most_config_loads("checkout_between_trees", 3);

        cascade_load_counters::measure(|| {
            rev_list(
                &repo3,
                &["HEAD".to_string()],
                &[],
                &RevListOptions::default(),
            )
            .unwrap();
        });
        assert_at_most_config_loads("rev_list", 2);
    }

    #[test]
    fn repository_config_revalidates_global_cascade_stamps() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.cfg");
        fs::write(&global, "[user]\n\tname = Alice\n").unwrap();

        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        init_repository(&root, false, "main", None, "files").expect("init");

        let mut env = Environment::empty();
        env.cwd = root.clone();
        env.git_config_global = Some(global.to_string_lossy().into_owned());
        env.git_config_nosystem = Some("true".into());
        env.git_config_system = Some("/dev/null".into());

        let git_dir = root.join(".git");
        let repo = Repository::open_with(
            &RepositoryOptions::with_environment(env),
            &git_dir,
            Some(&root),
        )
        .expect("open");
        assert_eq!(
            repo.config().expect("config").get("user.name").as_deref(),
            Some("Alice")
        );

        fs::write(
            &global,
            "[user]\n\tname = Alice With A Much Longer Name For Stamp Change\n",
        )
        .unwrap();
        assert_eq!(
            repo.config().expect("config").get("user.name").as_deref(),
            Some("Alice With A Much Longer Name For Stamp Change")
        );
    }

    #[test]
    fn repository_config_reload_after_write() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);

        assert!(repo.config().unwrap().get("grit.factoryTestKey").is_none());

        let config_path = repo.git_dir.join("config");
        let mut cfg = ConfigFile::from_path(&config_path, ConfigScope::Local)
            .unwrap()
            .expect("local config");
        cfg.set("grit.factoryTestKey", "before").unwrap();
        cfg.write().unwrap();
        repo.reload_config().unwrap();
        assert_eq!(
            repo.config().unwrap().get("grit.factoryTestKey"),
            Some("before".to_string())
        );

        cfg.set("grit.factoryTestKey", "after").unwrap();
        cfg.write().unwrap();
        repo.reload_config().unwrap();
        assert_eq!(
            repo.config().unwrap().get("grit.factoryTestKey"),
            Some("after".to_string())
        );
    }

    #[test]
    fn diff_tree_to_worktree_uses_repo_git_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Linked worktree: autocrlf only on the common repository config.
        let main = root.join("main");
        fs::create_dir_all(&main).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&main)
            .status()
            .unwrap()
            .success());
        for args in [
            ["config", "user.email", "t@example.com"],
            ["config", "user.name", "t"],
            ["config", "core.autocrlf", "true"],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .unwrap();
        }
        fs::write(main.join("seed.txt"), b"s\n").unwrap();
        Command::new("git")
            .args(["add", "seed.txt"])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "seed"])
            .current_dir(&main)
            .status()
            .unwrap();

        let linked = root.join("linked");
        assert!(Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "linked-branch",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&main)
            .status()
            .unwrap()
            .success());

        let linked_repo = Repository::discover(Some(&linked)).unwrap();
        fs::write(linked.join("crlf.txt"), b"line\r\n").unwrap();
        let wt_oid = linked_repo.odb.write(ObjectKind::Blob, b"line\n").unwrap();
        let mut index = Index::new();
        let entry =
            entry_from_stat(&linked.join("crlf.txt"), b"crlf.txt", wt_oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
        let tree_oid =
            crate::write_tree::write_tree_from_index(&linked_repo.odb, &index, "").expect("tree");

        let cfg = linked_repo.config().unwrap();
        let entries = diff_tree_to_worktree_with_git_dir(
            &linked_repo.odb,
            Some(&tree_oid),
            &linked,
            &linked_repo.git_dir,
            &index,
            Some(cfg.as_ref()),
        )
        .unwrap();
        assert!(
            entries.is_empty(),
            "autocrlf from common git dir should treat CRLF worktree as clean: {entries:?}"
        );

        // Gitfile + separate git dir layout.
        let wt = root.join("gitfile-wt");
        let gd = root.join("gitfile-store.git");
        let gitfile_repo =
            init_repository_separate_git_dir(&wt, &gd, "main", None, "files").unwrap();
        fs::write(
            gd.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n",
        )
        .unwrap();
        gitfile_repo.reload_config().unwrap();

        fs::write(wt.join("g.txt"), b"a\r\n").unwrap();
        let blob = gitfile_repo.odb.write(ObjectKind::Blob, b"a\n").unwrap();
        let mut idx = Index::new();
        idx.add_or_replace(
            entry_from_stat(&wt.join("g.txt"), b"g.txt", blob, MODE_REGULAR).unwrap(),
        );
        let tree =
            crate::write_tree::write_tree_from_index(&gitfile_repo.odb, &idx, "").expect("tree");
        let cfg = gitfile_repo.config().unwrap();
        let clean = diff_tree_to_worktree_with_git_dir(
            &gitfile_repo.odb,
            Some(&tree),
            &wt,
            &gitfile_repo.git_dir,
            &idx,
            Some(cfg.as_ref()),
        )
        .unwrap();
        assert!(
            clean.is_empty(),
            "gitfile repo must load config from git_dir"
        );
    }

    #[test]
    fn index_write_with_config_honors_skip_hash_git_compat() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);

        for (skip, label) in [(false, "false"), (true, "true")] {
            fs::write(
                repo.git_dir.join("config"),
                format!(
                    "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[index]\n\tskipHash = {label}\n"
                ),
            )
            .unwrap();
            repo.reload_config().unwrap();
            let cfg = repo.config().unwrap();

            fs::write(root.join("f.txt"), format!("v-{label}\n").as_bytes()).unwrap();
            stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
            let index = repo.load_index().unwrap();
            let path = repo.index_path();
            index
                .write_with_config(&path, cfg.as_ref())
                .expect("write index");

            let status = Command::new("git")
                .args(["ls-files", "-s", "f.txt"])
                .current_dir(root)
                .output()
                .expect("git ls-files");
            assert!(
                status.status.success(),
                "git ls-files failed for skipHash={label}"
            );
            assert!(
                String::from_utf8_lossy(&status.stdout).contains("f.txt"),
                "git must read index with skipHash={label}"
            );

            let fsck = Command::new("git")
                .args(["fsck"])
                .current_dir(root)
                .output()
                .expect("git fsck");
            assert!(
                fsck.status.success(),
                "git fsck failed for skipHash={label}: {}",
                String::from_utf8_lossy(&fsck.stderr)
            );

            let index_bytes = fs::read(&path).expect("read index");
            let hash_len = repo.odb.hash_algo().len();
            let trailing = &index_bytes[index_bytes.len().saturating_sub(hash_len)..];
            if skip {
                assert!(
                    trailing.iter().all(|&b| b == 0),
                    "skipHash=true should write zero checksum"
                );
            } else {
                assert!(
                    trailing.iter().any(|&b| b != 0),
                    "skipHash=false should write nonzero checksum"
                );
            }
            let _ = skip;
        }
    }

    #[test]
    fn repository_format_ignores_global_extensions() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.gitconfig");
        fs::write(&global, "[extensions]\n\trefstorage = reftable\n").unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let repo = init_repo(&root);
        fs::write(root.join("tracked.txt"), b"x\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let prev_global = env::var("GIT_CONFIG_GLOBAL").ok();
        let prev_system = env::var("GIT_CONFIG_SYSTEM").ok();
        env::set_var("GIT_CONFIG_GLOBAL", &global);
        env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");

        let discovered = Repository::discover(Some(&root)).expect("discover");
        let result = status(&discovered, &StatusOptions::default(), &mut NullProgress);

        if let Some(v) = prev_global {
            env::set_var("GIT_CONFIG_GLOBAL", v);
        } else {
            env::remove_var("GIT_CONFIG_GLOBAL");
        }
        if let Some(v) = prev_system {
            env::set_var("GIT_CONFIG_SYSTEM", v);
        } else {
            env::remove_var("GIT_CONFIG_SYSTEM");
        }

        result.expect("global extensions must not fail repository format check");
        assert!(
            !is_reftable_repo(&discovered.git_dir),
            "global refstorage must not enable reftable backend"
        );
    }

    #[test]
    fn includeif_onbranch_load_with_reftable_probe_does_not_deadlock() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("real");
        let repo = init_repo(&root);
        let git_dir = repo.git_dir.clone();
        fs::write(
            git_dir.join("config"),
            "[includeIf \"onbranch:main\"]\n\tpath = branch.conf\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("branch.conf"),
            "[snapshotTest]\n\tkey = onmain\n",
        )
        .unwrap();

        let git_dir_thread = git_dir.clone();
        let probe = std::thread::spawn(move || is_reftable_repo(&git_dir_thread));
        let cfg = ConfigSet::load(
            &crate::environment::Environment::capture_process(),
            Some(&git_dir),
            true,
        )
        .expect("load cascade");
        assert_eq!(cfg.get("snapshotTest.key"), Some("onmain".to_string()));
        assert!(!probe.join().expect("probe thread panicked"));
    }
}
