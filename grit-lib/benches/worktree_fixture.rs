//! Fixtures for index, config, ignore, and attribute Criterion benchmarks.
//!
//! Builds deterministic repositories with `grit-lib` (same approach as the object
//! micro-benchmark fixtures in [`super::fixture`] / plan step 7).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use filetime::{set_file_mtime, FileTime};
use grit_lib::attributes::{load_gitattributes_stack, ParsedGitAttributes};
use grit_lib::config::{ConfigSet, LoadConfigOptions};
use grit_lib::environment::Environment;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::ObjectId;
use grit_lib::repo::{init_repository, Repository};
use tempfile::TempDir;

const BENCH_OID_BYTE: u8 = 0x42;

fn bench_oid(seed: u32) -> ObjectId {
    let mut bytes = [BENCH_OID_BYTE; 20];
    bytes[16..20].copy_from_slice(&seed.to_be_bytes());
    ObjectId::from_bytes(&bytes).expect("bench oid bytes")
}

fn make_index_entry(path: &str, seed: u32) -> IndexEntry {
    let path_bytes = path.as_bytes().to_vec();
    IndexEntry {
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        dev: 1,
        ino: seed,
        mode: MODE_REGULAR,
        uid: 1000,
        gid: 1000,
        size: 64,
        oid: bench_oid(seed),
        flags: path_bytes.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path_bytes,
        base_index_pos: 0,
    }
}

fn build_index(entry_count: usize, version: u32) -> Index {
    let mut idx = Index::empty(version);
    for i in 0..entry_count {
        let path = format!("d{:04}/f{:05}.txt", i / 100, i % 100);
        idx.add_or_replace(make_index_entry(&path, i as u32));
    }
    idx
}

fn write_index_file(index: &Index, path: &Path) {
    index.write(path).expect("write index fixture");
}

fn append_config_keys(buf: &mut String, section: &str, start: usize, count: usize) {
    for i in 0..count {
        let n = start + i;
        buf.push_str(&format!("[{section}.bench{n:04}]\n\tkey = value-{n}\n"));
    }
}

fn write_realistic_gitignore(path: &Path) {
    let content = r"# Rust / Node / Python / build artifacts
/target/
**/target/
/node_modules/
/dist/
/build/
*.o
*.a
*.so
*.dylib
*.class
*.pyc
__pycache__/
*.log
.env
.env.*
.DS_Store
*.swp
*.swo
/vendor/
/coverage/
*.tmp
!.gitkeep
";
    std::fs::write(path, content).expect("write gitignore");
}

fn build_status_nested_l_fixtures(base: &Path) -> (Repository, Repository) {
    fn populate(wt: &Path, with_nested_attrs: bool) {
        std::fs::write(wt.join(".gitattributes"), "* text=auto\n").ok();
        for d in 0..50 {
            let dir = wt.join(format!("dir{d:02}"));
            std::fs::create_dir_all(&dir).expect("status L dir");
            if with_nested_attrs {
                std::fs::write(dir.join(".gitattributes"), "* eol=lf\n").expect("nested ga");
            }
            for f in 0..40 {
                std::fs::write(
                    dir.join(format!("f{f:04}.txt")),
                    format!("content-{d}-{f}\n"),
                )
                .expect("status L file");
            }
        }
    }

    let nested_root = base.join("status-nested-L");
    let plain_root = base.join("status-plain-L");
    let nested_repo = init_repository(
        &nested_root,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("nested status repo");
    let plain_repo = init_repository(
        &plain_root,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("plain status repo");
    populate(nested_repo.work_tree.as_ref().expect("wt"), true);
    populate(plain_repo.work_tree.as_ref().expect("wt"), false);
    (nested_repo, plain_repo)
}

fn write_realistic_gitattributes(path: &Path) {
    let content = r"* text=auto
*.rs linguist-language=Rust diff=rust
*.c linguist-language=C diff=cpp
*.h linguist-language=C
*.md linguist-detectable
*.json -text
*.png -text
tests/** export-ignore
bench/** export-ignore
";
    std::fs::write(path, content).expect("write gitattributes");
}

fn generate_probe_paths(count: usize) -> Vec<String> {
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let bucket = i % 10;
        let path = match bucket {
            0 => format!("src/lib/module_{i:05}.rs"),
            1 => format!("target/debug/deps/pkg_{i:05}.rlib"),
            2 => format!("node_modules/pkg-{i}/index.js"),
            3 => format!("d{:04}/f{:05}.txt", i / 100, i % 100),
            4 => format!("docs/chapter_{i:05}.md"),
            5 => format!("tests/integration/t_{i:05}.rs"),
            6 => format!("vendor/lib_{i:05}.c"),
            7 => format!("build/out_{i:05}.o"),
            8 => format!("tmp/scratch_{i:05}.log"),
            _ => format!("mixed/sub/{i:05}/file.txt"),
        };
        paths.push(path);
    }
    paths
}

/// Pre-built worktree-related benchmark state.
pub struct WorktreeBenchFixtures {
    pub _root: TempDir,
    pub index_v2_10k_path: PathBuf,
    pub index_v2_100k_path: PathBuf,
    pub index_v4_10k_path: PathBuf,
    pub index_v4_100k_path: PathBuf,
    pub index_v2_10k: Index,
    pub index_v2_100k: Index,
    pub index_v4_10k: Index,
    pub index_v4_100k: Index,
    pub config_git_dir: PathBuf,
    pub config_touch_path: PathBuf,
    pub config_load_opts: LoadConfigOptions,
    pub ignore_repo: Repository,
    pub ignore_paths: Vec<String>,
    pub attr_stack: ParsedGitAttributes,
    pub attr_paths: Vec<String>,
    pub index_write_scratch: PathBuf,
    /// Repository with 10k tracked files on disk for status/staging scan benchmarks.
    pub scan_repo_10k: Repository,
    /// Repository with one `.gitattributes` per directory (L-sized status fixture).
    pub status_nested_attr_repo: Repository,
    /// Same tree layout without per-directory `.gitattributes` (baseline).
    pub status_plain_l_repo: Repository,
}

impl WorktreeBenchFixtures {
    fn build() -> Self {
        let root = tempfile::tempdir().expect("worktree bench tempdir");
        let base = root.path();

        let index_dir = base.join("indexes");
        std::fs::create_dir_all(&index_dir).expect("index dir");

        let index_v2_10k = build_index(10_000, 2);
        let index_v2_100k = build_index(100_000, 2);
        let index_v4_10k = build_index(10_000, 4);
        let index_v4_100k = build_index(100_000, 4);

        let index_v2_10k_path = index_dir.join("index-v2-10k");
        let index_v2_100k_path = index_dir.join("index-v2-100k");
        let index_v4_10k_path = index_dir.join("index-v4-10k");
        let index_v4_100k_path = index_dir.join("index-v4-100k");
        write_index_file(&index_v2_10k, &index_v2_10k_path);
        write_index_file(&index_v2_100k, &index_v2_100k_path);
        write_index_file(&index_v4_10k, &index_v4_10k_path);
        write_index_file(&index_v4_100k, &index_v4_100k_path);

        let config_root = base.join("config-fixture");
        let includes = config_root.join("includes");
        std::fs::create_dir_all(&includes).expect("config includes dir");
        let global_path = config_root.join("global.gitconfig");
        let extra_path = includes.join("extra.conf");
        let more_path = includes.join("more.conf");
        let mut global =
            String::from("[user]\n\tname = Bench User\n\temail = bench@grit-scm.test\n");
        append_config_keys(&mut global, "global", 0, 200);
        global.push_str("\n[include]\n\tpath = includes/extra.conf\n");
        std::fs::write(&global_path, global).expect("global config");

        let mut extra = String::from("[include]\n\tpath = more.conf\n");
        append_config_keys(&mut extra, "extra", 0, 150);
        std::fs::write(&extra_path, extra).expect("extra include");

        let mut more = String::new();
        append_config_keys(&mut more, "more", 0, 100);
        std::fs::write(&more_path, more).expect("more include");

        let config_git_dir = config_root.join("repo.git");
        std::fs::create_dir_all(&config_git_dir).expect("config repo git dir");
        let mut local = String::from("[core]\n\trepositoryformatversion = 0\n");
        append_config_keys(&mut local, "local", 0, 50);
        local.push_str("\n[include]\n\tpath = ../includes/extra.conf\n");
        std::fs::write(config_git_dir.join("config"), local).expect("local config");

        std::env::set_var("GIT_CONFIG_GLOBAL", &global_path);
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");

        let config_load_opts = LoadConfigOptions {
            include_system: false,
            include_ctx: grit_lib::config::IncludeContext {
                git_dir: Some(config_git_dir.clone()),
                ..Default::default()
            },
            ..LoadConfigOptions::default()
        };

        let ignore_root = base.join("ignore-repo");
        let ignore_repo = init_repository(
            &ignore_root,
            false,
            "main",
            None,
            grit_lib::RefStorageFormat::Files,
        )
        .expect("ignore repo init");
        let wt = ignore_repo.work_tree.as_ref().expect("work tree");
        write_realistic_gitignore(&wt.join(".gitignore"));
        std::fs::write(
            ignore_repo.git_dir.join("info/exclude"),
            "*.local\n/build/\n",
        )
        .expect("info/exclude");
        for d in 0..64 {
            let sub = wt.join(format!("d{d:02}"));
            std::fs::create_dir_all(&sub).expect("ignore subdir");
            write_realistic_gitignore(&sub.join(".gitignore"));
        }
        let ignore_paths = generate_probe_paths(100_000);

        let attr_root = base.join("attr-repo");
        let attr_repo = init_repository(
            &attr_root,
            false,
            "main",
            None,
            grit_lib::RefStorageFormat::Files,
        )
        .expect("attr repo init");
        let attr_wt = attr_repo.work_tree.as_ref().expect("work tree");
        write_realistic_gitattributes(&attr_wt.join(".gitattributes"));
        for d in 0..32 {
            let sub = attr_wt.join(format!("pkg{d:02}"));
            std::fs::create_dir_all(&sub).expect("attr subdir");
            write_realistic_gitattributes(&sub.join(".gitattributes"));
        }
        let attr_stack = load_gitattributes_stack(&attr_repo, attr_wt).expect("load attr stack");
        let _keep_attr_repo = attr_repo;
        let attr_paths = generate_probe_paths(100_000);

        let index_write_scratch = index_dir.join("write-scratch");

        let scan_root = base.join("scan-10k");
        let scan_repo = init_repository(
            &scan_root,
            false,
            "main",
            None,
            grit_lib::RefStorageFormat::Files,
        )
        .expect("scan repo init");
        let scan_wt = scan_repo.work_tree.as_ref().expect("work tree");
        let mut scan_index = Index::empty(2);
        for i in 0..10_000 {
            let dir = format!("d{:04}", i / 100);
            let rel = format!("{dir}/f{:05}.txt", i % 100);
            let path = scan_wt.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("scan subdir");
            }
            let bytes = format!("payload {i}\n");
            std::fs::write(&path, &bytes).expect("scan blob");
            let oid = scan_repo
                .odb
                .write(grit_lib::objects::ObjectKind::Blob, bytes.as_bytes())
                .expect("hash scan blob");
            let meta = std::fs::symlink_metadata(&path).expect("stat scan file");
            let entry =
                grit_lib::index::entry_from_metadata(&meta, rel.as_bytes(), oid, MODE_REGULAR);
            scan_index.add_or_replace(entry);
        }
        scan_index
            .write(&scan_repo.index_path())
            .expect("write scan index");

        let (status_nested_attr_repo, status_plain_l_repo) = build_status_nested_l_fixtures(base);

        Self {
            _root: root,
            index_v2_10k_path,
            index_v2_100k_path,
            index_v4_10k_path,
            index_v4_100k_path,
            index_v2_10k,
            index_v2_100k,
            index_v4_10k,
            index_v4_100k,
            config_git_dir,
            config_touch_path: extra_path,
            config_load_opts,
            ignore_repo,
            ignore_paths,
            attr_stack,
            attr_paths,
            index_write_scratch,
            scan_repo_10k: scan_repo,
            status_nested_attr_repo,
            status_plain_l_repo,
        }
    }

    /// Bust the in-process config cache so the next load re-reads files from disk.
    pub fn bump_config_stamp(&self) {
        let now = FileTime::now();
        set_file_mtime(&self.config_touch_path, now).expect("touch config include");
    }

    /// Load the layered config fixture (global + includes + local).
    pub fn load_config_cascade(&self) -> ConfigSet {
        ConfigSet::load_with_options(
            &Environment::capture_process(),
            Some(&self.config_git_dir),
            &self.config_load_opts,
        )
        .expect("config load")
    }

    #[must_use]
    pub fn global() -> &'static Self {
        static FIX: OnceLock<WorktreeBenchFixtures> = OnceLock::new();
        FIX.get_or_init(WorktreeBenchFixtures::build)
    }
}
