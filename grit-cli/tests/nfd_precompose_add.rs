//! Modern CLI: NFD worktree paths stage as NFC index entries when precompose is enabled.

use std::fs;
use std::process::Command;

use grit_lib::index::Index;

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

#[test]
fn grit_init_add_nfd_untracked_stores_nfc_in_index() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();

    for cmd in [
        Command::new(grit)
            .arg("init")
            .current_dir(dir.path())
            .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
            .env("GIT_CONFIG_GLOBAL", &null)
            .env("GIT_CONFIG_SYSTEM", &null),
        {
            let nfd = format!("cafe\u{0301}.txt");
            fs::write(dir.path().join(&nfd), b"x\n").expect("write nfd file");
            Command::new(grit)
                .arg("add")
                .current_dir(dir.path())
                .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
                .env("GIT_CONFIG_GLOBAL", &null)
                .env("GIT_CONFIG_SYSTEM", &null)
        },
    ] {
        let out = cmd.output().expect("run grit");
        assert!(
            out.status.success(),
            "grit failed: status={:?} stderr={} stdout={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }

    let index = Index::load(&dir.path().join(".git/index")).expect("load index");
    assert_eq!(index.entries.len(), 1, "expected one staged file");
    let path = &index.entries[0].path;
    assert_eq!(
        path.as_slice(),
        b"caf\xc3\xa9.txt",
        "index must store NFC UTF-8 spelling (issue #910)"
    );
}
