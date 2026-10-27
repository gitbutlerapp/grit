//! A long-lived ODB must read more packs than its process descriptor limit.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::log::{collect_decorations_for_commits, DecorationKind};
use grit_lib::repo::Repository;
use grit_test_support::objects::many_tag_packs;

#[test]
fn many_mapped_packs_under_reduced_descriptor_limit() {
    let exe = std::env::current_exe().unwrap();
    for mode in ["objects", "decorations", "pressure"] {
        let output = Command::new(&exe)
            .args([
                "--exact",
                "pack_descriptor_pressure::descriptor_pressure_child",
                "--nocapture",
            ])
            .env("GRIT_PACK_DESCRIPTOR_TEST", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn descriptor_pressure_child() {
    let Ok(mode) = std::env::var("GRIT_PACK_DESCRIPTOR_TEST") else {
        return;
    };
    let (fixture, refs) = many_tag_packs(96).unwrap();
    let refs: Vec<(ObjectId, ObjectId)> = refs
        .into_iter()
        .map(|(tag, commit)| (tag.parse().unwrap(), commit.parse().unwrap()))
        .collect();
    let repo = Repository::discover(Some(fixture.path()), &RepositoryOptions::empty()).unwrap();
    // Leave fewer slots than the pool's normal capacity to exercise exhaustion recovery.
    let _reserved: Vec<_> = if mode == "pressure" {
        (0..48)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect()
    } else {
        Vec::new()
    };
    let limit = libc::rlimit {
        rlim_cur: 64,
        rlim_max: 64,
    };
    // SAFETY: only this isolated child lowers its own descriptor limit, after fixture setup.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    if mode != "decorations" {
        // Visit twice, in opposite orders, to exercise retained mappings after eviction.
        for (tag, _) in refs.iter().chain(refs.iter().rev()) {
            let object = repo
                .odb()
                .read(tag)
                .unwrap_or_else(|err| panic!("reading {tag}: {err}"));
            assert_eq!(object.kind, ObjectKind::Tag);
        }
    } else {
        let commits: Vec<_> = refs.iter().map(|(_, commit)| *commit).collect();
        for _ in 0..2 {
            let decorations = collect_decorations_for_commits(&repo, false, &commits).unwrap();
            let tags = decorations
                .values()
                .flatten()
                .filter(|item| item.kind == DecorationKind::Tag)
                .count();
            assert_eq!(
                tags, 96,
                "every tag must be decorated under descriptor pressure"
            );
        }
    }
}
