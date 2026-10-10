//! Alternates ordering and missing-directory handling for composed ODB stores.

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::read_alternates_recursive;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn init_primary_with_alternates(objects_dir: &std::path::Path, lines: &str) -> PathBuf {
    fs::create_dir_all(objects_dir.join("info")).expect("info");
    fs::write(objects_dir.join("info/alternates"), lines).expect("alternates");
    objects_dir.to_path_buf()
}

#[test]
fn odb_sources_follow_file_env_submodule_order() {
    let root = TempDir::new().expect("tempdir");
    let file_alt = root.path().join("file-alt/objects");
    let env_alt = root.path().join("env-alt/objects");
    fs::create_dir_all(&file_alt).expect("file alt");
    fs::create_dir_all(&env_alt).expect("env alt");

    let primary_objects = root.path().join("primary/objects");
    init_primary_with_alternates(&primary_objects, &format!("{}\n", file_alt.display()));

    let file_odb = Odb::new(&file_alt);
    let file_oid = file_odb
        .write(ObjectKind::Blob, b"from-file-alt")
        .expect("write file alt");

    let env_odb = Odb::new(&env_alt);
    let env_oid = env_odb
        .write(ObjectKind::Blob, b"from-env-alt")
        .expect("write env alt");

    let primary = Odb::new(&primary_objects).with_env_alternate_dirs(vec![env_alt.clone()]);

    let chain = read_alternates_recursive(&primary_objects).expect("chain");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0],
        file_alt.canonicalize().unwrap_or(file_alt.clone())
    );

    assert!(primary.read(&file_oid).is_ok());
    assert!(primary.read(&env_oid).is_ok());

    let mut seen = Vec::new();
    primary
        .sources()
        .expect("sources")
        .stores()
        .iter()
        .for_each(|store| seen.push(store.hash_algo()));
    assert_eq!(seen.len(), 2, "file + env alternates");
}

#[test]
fn odb_skips_missing_alternate_directory() {
    let root = TempDir::new().expect("tempdir");
    let primary_objects = root.path().join("primary/objects");
    init_primary_with_alternates(&primary_objects, "/nonexistent/alternate/objects\n");
    let resolved = read_alternates_recursive(&primary_objects).expect("read");
    assert_eq!(
        resolved.len(),
        1,
        "unresolvable alternates lines are still listed"
    );
    let odb = Odb::new(&primary_objects);
    assert_eq!(
        odb.sources().expect("sources").stores().len(),
        1,
        "missing alternate dir still yields a files source entry"
    );
    let miss = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("oid");
    assert!(!odb.exists(&miss));
}

#[test]
fn odb_warmed_sources_cache_respects_env_alternate_dirs_on_clone() {
    let root = TempDir::new().expect("tempdir");
    let primary_objects = root.path().join("primary/objects");
    fs::create_dir_all(&primary_objects).expect("primary objects");

    let alt_objects = root.path().join("env-alt/objects");
    fs::create_dir_all(&alt_objects).expect("alt objects");
    let alt_odb = Odb::new(&alt_objects);
    let alt_oid = alt_odb
        .write(ObjectKind::Blob, b"only-in-env-alt")
        .expect("write alt");

    let base = Odb::new(&primary_objects);
    assert_eq!(
        base.sources().expect("warm empty").stores().len(),
        0,
        "precondition: no file alternates"
    );

    let configured = base
        .clone()
        .with_env_alternate_dirs(vec![alt_objects.clone()]);
    assert_eq!(
        configured.sources().expect("sources").stores().len(),
        1,
        "env alternate must appear after warming an empty cache on the clone"
    );
    let via_configured = configured.read(&alt_oid).expect("read alt");
    let direct = alt_odb.read(&alt_oid).expect("read direct");
    assert_eq!(via_configured.kind, direct.kind);
    assert_eq!(via_configured.data, direct.data);
}
