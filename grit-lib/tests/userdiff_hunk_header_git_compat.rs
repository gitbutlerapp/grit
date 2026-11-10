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
        RenameDetection, UncommittedDiff,
    };
    use grit_lib::repo::Repository;

    let repo = Repository::open(root, &RepositoryOptions::default()).expect("open repo");
    let index = repo.load_index().expect("index");
    let mut uncommitted = UncommittedDiff::new(&repo, index);
    let head = repo.read_head_commit().expect("head");
    let entries = uncommitted
        .worktree_entries(Some(&head.tree), RenameDetection::Off)
        .expect("entries");
    let changes = uncommitted
        .load_worktree_changes(entries.clone())
        .expect("changes");
    let resolved: Vec<PatchResolvedSide> = entries
        .into_iter()
        .zip(changes)
        .map(|(entry, fc)| PatchResolvedSide {
            entry,
            old: PatchSide::Bytes(fc.old_bytes.data),
            new: PatchSide::Worktree,
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
