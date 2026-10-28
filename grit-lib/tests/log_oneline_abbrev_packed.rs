//! Packed-history log abbreviations must stay correct and avoid per-commit loose probes.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;
use grit_lib::OidAbbrevBatch;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn abbrev_known_packed_linear_history_skips_loose_exists_probes() {
    let root = TempDir::new().expect("tempdir");
    let dir = root.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let import = r#"python3 - <<'PY' | git fast-import --quiet
import random
random.seed(1)
t=1500000000
prev=None
for i in range(1,801):
    t+=random.randint(30,4000)
    msg='change %d'%i
    print('commit refs/heads/main\nmark :%d\nauthor A <a@e.t> %d +0000\ncommitter A <a@e.t> %d +0000\ndata %d\n%s'%(i,t,t,len(msg),msg))
    if prev: print('from :%d'%prev)
    for _ in range(random.randint(1,3)):
        b=''.join('line %d\n'%random.randint(0,10**6) for _ in range(20))
        print('M 100644 inline d%d/f%d.txt\ndata %d\n%s'%(random.randint(0,6),random.randint(0,60),len(b),b))
    prev=i
PY"#;
    let out = Command::new("bash")
        .current_dir(dir)
        .args(["-c", import])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("import");
    assert!(
        out.status.success(),
        "fast-import: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(dir, &["gc", "--quiet", "--aggressive"]);

    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
    let mut commits = Vec::new();
    repo.odb()
        .for_each_object(&mut |oid| {
            if repo.odb().read(oid).expect("read").kind == ObjectKind::Commit {
                commits.push(*oid);
            }
            std::ops::ControlFlow::Continue(())
        })
        .expect("walk");
    commits.sort_by_key(|o| o.to_hex());

    let mut batch = OidAbbrevBatch::new(7);
    let mut git_lines: Vec<String> = git(dir, &["log", "--format=%h %s"])
        .lines()
        .map(str::to_owned)
        .collect();
    let mut grit_lines = Vec::with_capacity(commits.len());
    for oid in commits.iter().rev() {
        let abbrev = batch.abbrev_known(&repo, *oid).expect("abbrev");
        let obj = repo.odb().read(oid).expect("read");
        let subject = grit_lib::objects::read_commit_subject_line(&obj.data).expect("subject");
        grit_lines.push(format!("{abbrev} {subject}"));
    }
    grit_lines.sort();
    git_lines.sort();
    assert_eq!(grit_lines, git_lines);
}
