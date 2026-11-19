//! Built-in `userdiff` drivers: hunk headers must match system `git diff` byte-for-byte.

use std::path::Path;
use std::process::Command;

use grit_lib::config::ConfigSet;
use grit_lib::diff::userdiff::matcher_for_driver;
use grit_lib::diff::{unified_diff, UnifiedDiffOptions};

fn git_diff_hunk_headers(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["diff", "-U3", "--no-color"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff");
    assert!(
        out.status.success(),
        "git diff: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("@@"))
        .map(|l| l.to_owned())
        .collect()
}

fn grit_hunk_headers(driver: &str, path: &str, before: &str, after: &str) -> Vec<String> {
    let config = ConfigSet::default();
    let matcher = matcher_for_driver(&config, driver)
        .expect("matcher")
        .expect("builtin matcher");
    let options = UnifiedDiffOptions::new()
        .with_paths(path, path)
        .with_funcname(&matcher);
    let diff = unified_diff(before, after, &options);
    diff.lines()
        .filter(|l| l.starts_with("@@"))
        .map(|l| l.to_owned())
        .collect()
}

fn assert_hunk_headers_match_git(driver: &str, rel_path: &str, before: &str, after: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join(".gitattributes"), format!("* diff={driver}\n")).unwrap();
    std::fs::create_dir_all(root.join(Path::new(rel_path).parent().unwrap_or(Path::new("")))).ok();
    std::fs::write(root.join(rel_path), before).unwrap();
    Command::new("git")
        .current_dir(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    Command::new("git")
        .current_dir(root)
        .args(["add", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "base", "--author", "T <t@e.com>"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_COMMITTER_NAME", "T")
        .status()
        .expect("git commit");
    std::fs::write(root.join(rel_path), after).unwrap();

    let git_hdrs = git_diff_hunk_headers(root);
    let grit_hdrs = grit_hunk_headers(driver, rel_path, before, after);
    assert_eq!(
        git_hdrs, grit_hdrs,
        "driver {driver} hunk headers differ from git diff"
    );
}

macro_rules! driver_case {
    ($test_name:ident, $driver:literal, $path:literal, $before:literal, $after:literal) => {
        #[test]
        fn $test_name() {
            assert_hunk_headers_match_git($driver, $path, $before, $after);
        }
    };
}

driver_case!(
    userdiff_hunk_rust,
    "rust",
    "lib.rs",
    "pub fn alpha() {\n    old\n}\n",
    "pub fn alpha() {\n    new\n}\n"
);
driver_case!(
    userdiff_hunk_python,
    "python",
    "mod.py",
    "def alpha():\n    old\n",
    "def alpha():\n    new\n"
);
driver_case!(
    userdiff_hunk_golang,
    "golang",
    "main.go",
    "package main\n\nfunc alpha() {\n\told\n}\n",
    "package main\n\nfunc alpha() {\n\tnew\n}\n"
);
driver_case!(
    userdiff_hunk_java,
    "java",
    "Main.java",
    "class Main {\n    void alpha() {\n        old;\n    }\n}\n",
    "class Main {\n    void alpha() {\n        new;\n    }\n}\n"
);
driver_case!(
    userdiff_hunk_cpp,
    "cpp",
    "main.cpp",
    "int alpha() {\n    return 1;\n}\n",
    "int alpha() {\n    return 2;\n}\n"
);
driver_case!(
    userdiff_hunk_ruby,
    "ruby",
    "app.rb",
    "def alpha\n  old\nend\n",
    "def alpha\n  new\nend\n"
);
driver_case!(
    userdiff_hunk_php,
    "php",
    "app.php",
    "<?php\nfunction alpha() {\n    old;\n}\n",
    "<?php\nfunction alpha() {\n    new;\n}\n"
);
driver_case!(
    userdiff_hunk_perl,
    "perl",
    "app.pl",
    "sub alpha {\n    old;\n}\n",
    "sub alpha {\n    new;\n}\n"
);
driver_case!(
    userdiff_hunk_bash,
    "bash",
    "run.sh",
    "function alpha() {\n    old\n}\n",
    "function alpha() {\n    new\n}\n"
);
driver_case!(
    userdiff_hunk_html,
    "html",
    "page.html",
    "<html>\n<H1>Title</H1>\nold\n</html>\n",
    "<html>\n<H1>Title</H1>\nnew\n</html>\n"
);
driver_case!(
    userdiff_hunk_markdown,
    "markdown",
    "doc.md",
    "# Title\n\nold\n",
    "# Title\n\nnew\n"
);
driver_case!(
    userdiff_hunk_ini,
    "ini",
    "app.ini",
    "[section]\nkey=old\n",
    "[section]\nkey=new\n"
);
driver_case!(
    userdiff_hunk_css,
    "css",
    "app.css",
    ".alpha {\n  color: old;\n}\n",
    ".alpha {\n  color: new;\n}\n"
);
driver_case!(
    userdiff_hunk_kotlin,
    "kotlin",
    "Main.kt",
    "fun alpha() {\n    old\n}\n",
    "fun alpha() {\n    new\n}\n"
);
driver_case!(
    userdiff_hunk_csharp,
    "csharp",
    "Main.cs",
    "class Main {\n    void Alpha() {\n        old;\n    }\n}\n",
    "class Main {\n    void Alpha() {\n        new;\n    }\n}\n"
);
driver_case!(
    userdiff_hunk_objc,
    "objc",
    "main.m",
    "@implementation Foo\n- (void)alpha {\n    old;\n}\n@end\n",
    "@implementation Foo\n- (void)alpha {\n    new;\n}\n@end\n"
);
driver_case!(
    userdiff_hunk_pascal,
    "pascal",
    "main.pas",
    "procedure Alpha;\nbegin\n  old;\nend;\n",
    "procedure Alpha;\nbegin\n  new;\nend;\n"
);
driver_case!(
    userdiff_hunk_elixir,
    "elixir",
    "app.ex",
    "defmodule M do\n  def alpha do\n    old\n  end\nend\n",
    "defmodule M do\n  def alpha do\n    new\n  end\nend\n"
);
driver_case!(
    userdiff_hunk_matlab,
    "matlab",
    "demo.m",
    "function alpha\n  old\nend\n",
    "function alpha\n  new\nend\n"
);
driver_case!(
    userdiff_hunk_scheme,
    "scheme",
    "app.scm",
    "(define (alpha)\n  old)\n",
    "(define (alpha)\n  new)\n"
);
driver_case!(
    userdiff_hunk_tex,
    "tex",
    "doc.tex",
    "\\section{Title}\nold\n",
    "\\section{Title}\nnew\n"
);
driver_case!(
    userdiff_hunk_bibtex,
    "bibtex",
    "refs.bib",
    "@article{key,\n  title = {old}\n}\n",
    "@article{key,\n  title = {new}\n}\n"
);
driver_case!(
    userdiff_hunk_dts,
    "dts",
    "board.dts",
    "/ {\n    node {\n        old;\n    };\n};\n",
    "/ {\n    node {\n        new;\n    };\n};\n"
);
driver_case!(
    userdiff_hunk_ada,
    "ada",
    "main.adb",
    "procedure Alpha is\nbegin\n   old;\nend Alpha;\n",
    "procedure Alpha is\nbegin\n   new;\nend Alpha;\n"
);
driver_case!(
    userdiff_hunk_fortran,
    "fortran",
    "main.f90",
    "program main\n  old\nend program main\n",
    "program main\n  new\nend program main\n"
);
driver_case!(
    userdiff_hunk_fountain,
    "fountain",
    "script.fountain",
    "INT. ROOM - DAY\n\nold\n",
    "INT. ROOM - DAY\n\nnew\n"
);
driver_case!(
    userdiff_hunk_r,
    "r",
    "analysis.R",
    "alpha <- function() {\n  old\n}\n",
    "alpha <- function() {\n  new\n}\n"
);

fn grit_worktree_patch_hunk_headers(root: &Path) -> Vec<String> {
    use std::io::Cursor;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diff::{
        write_patch_resolved, DiffOptions, PatchResolvedSide, PatchSide, PatchTextOptions,
        RenameDetection,
    };
    use grit_lib::repo::Repository;
    use grit_test_support::rev_parse;

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo");
    let mut uncommitted = repo.diff().uncommitted().expect("uncommitted");
    let head_tree = rev_parse(root, "HEAD^{tree}");
    let entries = uncommitted
        .worktree_entries(Some(&head_tree), RenameDetection::Off)
        .expect("entries");
    let changes = uncommitted
        .load_worktree_changes(entries.clone())
        .expect("changes");
    let resolved: Vec<PatchResolvedSide> = entries
        .into_iter()
        .zip(changes)
        .map(|(entry, fc)| {
            PatchResolvedSide::new(
                entry,
                PatchSide::Bytes(fc.old_bytes.data),
                PatchSide::Worktree,
            )
        })
        .collect();
    let opts = PatchTextOptions::from_repo(&repo, DiffOptions::default()).expect("patch opts");
    let mut out = Cursor::new(Vec::new());
    write_patch_resolved(&repo, &resolved, &opts, &mut out).expect("write_patch_resolved");
    String::from_utf8_lossy(&out.into_inner())
        .lines()
        .filter(|l| l.starts_with("@@"))
        .map(|l| l.to_owned())
        .collect()
}

fn assert_worktree_hunk_headers_match_git(
    c_cpp_before: &str,
    c_cpp_after: &str,
    pl_before: &str,
    pl_after: &str,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(
        root.join(".gitattributes"),
        "*.c diff=cpp\n*.pl diff=perl\n",
    )
    .unwrap();
    std::fs::write(root.join("a.c"), c_cpp_before).unwrap();
    std::fs::write(root.join("a.pl"), pl_before).unwrap();
    Command::new("git")
        .current_dir(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    Command::new("git")
        .current_dir(root)
        .args(["add", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "base", "--author", "T <t@e.com>"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_COMMITTER_NAME", "T")
        .status()
        .expect("git commit");
    std::fs::write(root.join("a.c"), c_cpp_after).unwrap();
    std::fs::write(root.join("a.pl"), pl_after).unwrap();
    let git_hdrs = git_diff_hunk_headers(root);
    let grit_hdrs = grit_worktree_patch_hunk_headers(root);
    assert_eq!(
        git_hdrs, grit_hdrs,
        "gitattributes diff drivers: hunk headers differ from git diff"
    );
}

#[test]
fn userdiff_attr_cpp_goto_and_perl_foreach_match_git() {
    let c_before = "int f(int x)\n{\n\tif (x)\n\t\tgoto out;\n\tx++;\n\tx++;\n\tx++;\n\tx++;\n\tx++;\n\tx++;\n\tx++;\nout:\n\tx++;\n\tx++;\n\tx++;\n\tx++;\n\treturn x;\n}\n";
    let c_after = c_before.replace("return x;", "return x + 1;");
    let pl_before = "use strict;\nforeach my $x (1..3) {\n    print \"a\\n\";\n    print \"b\\n\";\n    print \"c\\n\";\n    print \"d\\n\";\n    print \"e\\n\";\n    print \"f\\n\";\n}\nprint \"end\\n\";\n";
    let pl_after = pl_before.replace("end", "END");
    assert_worktree_hunk_headers_match_git(c_before, &c_after, pl_before, &pl_after);
}

#[test]
fn userdiff_attr_custom_xfuncname_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join(".gitattributes"), "*.xyz diff=custom\n").unwrap();
    std::fs::write(root.join("a.xyz"), "CUSTOM foo:\n    old\n").unwrap();
    Command::new("git")
        .current_dir(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    Command::new("git")
        .current_dir(root)
        .args(["config", "diff.custom.xfuncname", "^CUSTOM .*:$"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git config");
    Command::new("git")
        .current_dir(root)
        .args(["add", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "base", "--author", "T <t@e.com>"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_COMMITTER_NAME", "T")
        .status()
        .expect("git commit");
    std::fs::write(root.join("a.xyz"), "CUSTOM foo:\n    new\n").unwrap();
    let git_hdrs = git_diff_hunk_headers(root);
    let grit_hdrs = grit_worktree_patch_hunk_headers(root);
    assert_eq!(
        git_hdrs, grit_hdrs,
        "custom xfuncname driver: hunk headers differ from git diff"
    );
}

fn git_headers(root: &Path, args: &[&str]) -> Vec<String> {
    let mut all = vec!["-c", "core.quotepath=false", "diff", "-U3", "--no-color"];
    all.extend(args);
    grit_test_support::git(root, &all)
        .lines()
        .filter(|l| l.starts_with("@@"))
        .map(str::to_owned)
        .collect()
}

fn grit_target_headers(root: &Path, target: grit_lib::diff::DiffTarget) -> Vec<String> {
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diff::structured::format_hunk_range;
    use grit_lib::repo::Repository;
    use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo");
    let mut opts = grit_lib::diff::DiffOptions::default();
    opts.word_level_inline = false;
    let everything = Pathspec::entire_tree(PathspecGlobalFlags::default());
    let mut files = repo
        .diff()
        .files(&target, &everything, &opts)
        .expect("diff files");
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
        .iter()
        .flat_map(|f| &f.hunks)
        .map(|h| {
            let context = h
                .context
                .as_deref()
                .map(|c| format!(" {c}"))
                .unwrap_or_default();
            format!(
                "@@ -{} +{} @@{context}",
                format_hunk_range(h.old_start, h.old_lines),
                format_hunk_range(h.new_start, h.new_lines)
            )
        })
        .collect()
}

/// Nested `.gitattributes` files pick the driver for the paths below them only, for every diff
/// target (worktree, index and tree-to-tree) and for the patch writer.
#[test]
fn nested_gitattributes_diff_drivers_match_git() {
    use grit_lib::diff::DiffTarget;
    use grit_test_support::git;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "diff.section.xfuncname", "^SECTION .*$"]);
    git(root, &["config", "diff.label.xfuncname", "^LABEL .*$"]);

    let body = |tail: &str| {
        format!(
            "SECTION one\nLABEL two\nint f(void)\n{{\n\ta;\n\tb;\n\tc;\n\td;\n\te;\n\t{tail};\n}}\n"
        )
    };
    let files = [
        "top.c",
        "sub/mid.c",
        "sub/deep/x.c",
        "sub/deep/er/y.c",
        "other/z.c",
    ];
    std::fs::write(root.join(".gitattributes"), "*.c diff=cpp\n").unwrap();
    for dir in ["sub/deep/er", "other"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("sub/.gitattributes"), "*.c diff=section\n").unwrap();
    std::fs::write(
        root.join("sub/deep/.gitattributes"),
        "x.c diff=label\ner/** diff=label\n",
    )
    .unwrap();
    std::fs::write(root.join("other/.gitattributes"), "*.c diff=section\n").unwrap();
    std::fs::write(root.join("other/unrelated.c"), body("old")).unwrap();
    for f in files {
        std::fs::write(root.join(f), body("old")).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    for f in files {
        std::fs::write(root.join(f), body("new")).unwrap();
    }
    let worktree = git_headers(root, &[]);
    assert_eq!(worktree.len(), files.len(), "{worktree:?}");
    assert!(worktree.iter().any(|h| h.ends_with("@@ SECTION one")));
    assert!(worktree.iter().any(|h| h.ends_with("@@ LABEL two")));
    assert!(worktree.iter().any(|h| h.ends_with("@@ int f(void)")));
    assert_eq!(
        grit_target_headers(root, DiffTarget::WorktreeVsIndex),
        worktree
    );
    assert_eq!(grit_worktree_patch_hunk_headers(root), worktree);

    git(root, &["add", "-A"]);
    assert_eq!(
        grit_target_headers(root, DiffTarget::IndexVsHead),
        git_headers(root, &["--cached"])
    );

    git(root, &["commit", "-qm", "change"]);
    let tip: grit_lib::objects::ObjectId =
        git(root, &["rev-parse", "HEAD"]).trim().parse().unwrap();
    let base: grit_lib::objects::ObjectId =
        git(root, &["rev-parse", "HEAD~1"]).trim().parse().unwrap();
    assert_eq!(
        grit_target_headers(
            root,
            DiffTarget::Trees {
                old: Some(base),
                new: tip,
            }
        ),
        git_headers(root, &["HEAD~1", "HEAD"])
    );
}
