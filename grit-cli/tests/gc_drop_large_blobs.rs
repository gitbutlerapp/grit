//! `grit gc drop-large-blobs`: a large blob offloaded to a large-object promisor,
//! read back (a lazy fetch), then dropped again and fetched back once more.

use std::path::PathBuf;

use grit_test_support::CliFixture;
use serde_json::Value;

/// Letters from a xorshift sequence: they do not compress away.
fn big_text() -> String {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

/// A repository whose deleted `big.bin` lives in the promisor `lop` and was read
/// back since, so a promisor pack holds it. Returns the blob id.
fn fetched_back(fixture: &CliFixture) -> String {
    fixture.write_repo_file("big.bin", &big_text());
    fixture.run_ok(&["add", "big.bin"]);
    fixture.run_ok(&["commit", "-m", "big"]);
    let oid = blob_oid(fixture);
    fixture.run_ok(&["rm", "big.bin"]);
    fixture.run_ok(&["commit", "-m", "drop big"]);
    let lop = fixture.home_root().join("lop.git");
    let lop = lop.to_str().expect("utf-8 path");
    fixture.run_ok(&["init", "--bare", lop]);
    fixture.run_ok(&["remote", "add", "lop", lop]);
    fixture.run_ok(&[
        "gc",
        "offload",
        "--to",
        "lop",
        "--blobs",
        "10k",
        "--configure-lop",
    ]);
    let shown = fixture.run_ok(&["cat", &oid]);
    assert_eq!(shown.stdout.len(), 20_000);
    oid
}

fn blob_oid(_fixture: &CliFixture) -> String {
    let raw = grit_lib::objects::pack::hash_object_bytes(
        grit_lib::objects::ObjectKind::Blob,
        big_text().as_bytes(),
        20,
    )
    .expect("hash");
    grit_lib::objects::pack::oid_bytes_to_hex(&raw)
}

fn task(doc: &Value) -> &Value {
    let tasks = doc["tasks"].as_array().expect("tasks");
    tasks
        .iter()
        .find(|t| t["task"] == "drop-large-blobs")
        .unwrap_or_else(|| panic!("no drop-large-blobs task in {doc}"))
}

fn validate(doc: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../content/schemas/gc.drop-large-blobs.v1.json");
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("schema")).expect("parse");
    let compiled = jsonschema::validator_for(&schema).expect("compile schema");
    assert!(
        compiled.is_valid(doc),
        "{:?}",
        compiled
            .iter_errors(doc)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
}

fn pack_listing(fixture: &CliFixture) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = std::fs::read_dir(fixture.path().join(".git/objects/pack"))
        .expect("pack dir")
        .flatten()
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().map(|m| m.len()).unwrap_or(0),
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn dry_run_lists_then_a_run_drops_and_the_blob_comes_back() {
    let fixture = CliFixture::new();
    let oid = fetched_back(&fixture);

    // The default window keeps a blob fetched a moment ago.
    let default = fixture.run_ok(&["gc", "drop-large-blobs", "--dry-run", "--json"]);
    let doc: Value = serde_json::from_str(&default.stdout).expect("json");
    validate(&doc);
    assert_eq!(task(&doc)["dropped"], 0);
    assert_eq!(task(&doc)["kept"]["recently_fetched"], 1);
    assert_eq!(task(&doc)["min_size"], 10240, "the promisor's blob:limit");
    assert!(task(&doc)["keep_newer_than"].is_i64());

    let before = pack_listing(&fixture);
    let dry = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--older-than",
        "now",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(pack_listing(&fixture), before);
    let doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    validate(&doc);
    assert_eq!(doc["dry_run"], true);
    let t = task(&doc);
    assert_eq!(t["candidates"], 1);
    assert_eq!(t["dropped"], 1);
    assert_eq!(t["blobs"][0]["oid"], oid.as_str());
    assert_eq!(t["blobs"][0]["size"], 20_000);
    assert!(t.get("keep_newer_than").is_none());

    let human = fixture.run_ok(&["gc", "drop-large-blobs", "--older-than", "now", "--dry-run"]);
    assert!(
        human
            .stdout
            .contains("drop-large-blobs: would drop 1 promised blob >= 10.0 KiB"),
        "{}",
        human.stdout
    );
    assert!(human.stdout.contains(&format!("  {oid} 20000")));

    let real = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--min-size",
        "10k",
        "--older-than",
        "now",
    ]);
    assert!(
        real.stdout
            .contains("drop-large-blobs: dropped 1 promised blob >= 10.0 KiB, reclaimed"),
        "{}",
        real.stdout
    );
    fixture.run_ok(&["fsck"]);
    let again = fixture.run_ok(&["gc", "drop-large-blobs", "--older-than", "now", "--json"]);
    let doc: Value = serde_json::from_str(&again.stdout).expect("json");
    validate(&doc);
    assert_eq!(task(&doc)["candidates"], 0);

    let shown = fixture.run_ok(&["cat", &oid]);
    assert_eq!(shown.stdout.len(), 20_000, "lazy fetch from the promisor");
}

#[test]
fn markdown_has_a_task_section_and_a_blob_table() {
    let fixture = CliFixture::new();
    let oid = fetched_back(&fixture);
    let out = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--older-than",
        "now",
        "--dry-run",
        "--markdown",
    ]);
    assert!(
        out.stdout.contains("#### drop-large-blobs"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("- **dropped:** 1"));
    assert!(out.stdout.contains(&format!("| `{oid}` | 20000 |")));
}

#[test]
fn gc_runs_the_task_when_enabled() {
    let fixture = CliFixture::new();
    fetched_back(&fixture);
    let plain = fixture.run_ok(&["gc", "--dry-run", "--json"]);
    let doc: Value = serde_json::from_str(&plain.stdout).expect("json");
    assert!(
        !doc["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .any(|t| t["task"] == "drop-large-blobs"),
        "off by default: {doc}"
    );

    let config = fixture.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str("[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\texpire = now\n");
    std::fs::write(&config, text).expect("write config");
    let out = fixture.run_ok(&["gc", "--json"]);
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(task(&doc)["dropped"], 1);
    fixture.run_ok(&["fsck"]);
}

#[test]
fn errors_are_reported() {
    let fixture = CliFixture::new();
    let out = fixture.run(&["gc", "drop-large-blobs", "--json"]);
    assert!(!out.ok());
    let doc: Value = serde_json::from_str(&out.stdout).expect("json error");
    assert!(
        doc["error"]
            .as_str()
            .is_some_and(|e| e.contains("promisor remote")),
        "{doc}"
    );
    for (flag, value) in [
        ("--min-size", "none"),
        ("--min-size", "lots"),
        ("--older-than", "not a date at all"),
    ] {
        let bad = fixture.run(&["gc", "drop-large-blobs", flag, value]);
        assert!(!bad.ok());
        assert!(bad.stderr.contains(flag), "{}", bad.dump(flag));
    }
}

fn append_config(fixture: &CliFixture, text: &str) {
    let config = fixture.path().join(".git/config");
    let mut all = std::fs::read_to_string(&config).expect("config");
    all.push_str(text);
    std::fs::write(&config, all).expect("write config");
}

#[test]
fn older_than_overrides_expire_and_keeps_the_configured_min_size() {
    let fixture = CliFixture::new();
    fetched_back(&fixture);
    append_config(
        &fixture,
        "[maintenance \"drop-large-blobs\"]\n\texpire = never\n\tminSize = 15k\n",
    );
    let refused = fixture.run(&["gc", "drop-large-blobs", "--dry-run"]);
    assert!(!refused.ok());
    assert!(
        refused.stderr.contains("--older-than"),
        "{}",
        refused.dump("never")
    );

    let out = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--older-than",
        "now",
        "--dry-run",
        "--json",
    ]);
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    validate(&doc);
    assert_eq!(task(&doc)["min_size"], 15 * 1024, "{doc}");
    assert_eq!(task(&doc)["dropped"], 1);

    // A minSize above the blob keeps it, with expire still overridden.
    append_config(&fixture, "\tminSize = 30k\n");
    let out = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--older-than",
        "now",
        "--dry-run",
        "--json",
    ]);
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(task(&doc)["min_size"], 30 * 1024);
    assert_eq!(task(&doc)["candidates"], 0);
}

#[test]
fn older_than_replaces_an_invalid_expire() {
    let fixture = CliFixture::new();
    fetched_back(&fixture);
    append_config(
        &fixture,
        "[maintenance \"drop-large-blobs\"]\n\texpire = bogus\n",
    );
    let bad = fixture.run(&["gc", "drop-large-blobs", "--dry-run"]);
    assert!(!bad.ok());
    assert!(bad.stderr.contains("bogus"), "{}", bad.dump("bogus"));
    let out = fixture.run_ok(&[
        "gc",
        "drop-large-blobs",
        "--older-than",
        "now",
        "--dry-run",
        "--json",
    ]);
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(task(&doc)["dropped"], 1);
}

/// Loose object files under `.git/objects`.
fn loose_objects(fixture: &CliFixture) -> usize {
    std::fs::read_dir(fixture.path().join(".git/objects"))
        .expect("objects")
        .flatten()
        .filter(|e| e.file_name().len() == 2)
        .map(|e| std::fs::read_dir(e.path()).map_or(0, |d| d.count()))
        .sum()
}

#[test]
fn gc_rejects_an_invalid_expire_before_changing_anything() {
    let fixture = CliFixture::new();
    fetched_back(&fixture);
    fixture.write_repo_file("new.txt", "new\n");
    fixture.run_ok(&["add", "new.txt"]);
    let loose = loose_objects(&fixture);
    assert!(loose > 0);
    let packs = pack_listing(&fixture);
    append_config(
        &fixture,
        "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\texpire = bogus\n",
    );
    let out = fixture.run(&["gc"]);
    assert!(!out.ok());
    assert!(out.stderr.contains("bogus"), "{}", out.dump("gc"));
    assert_eq!(loose_objects(&fixture), loose, "repack must not have run");
    assert_eq!(pack_listing(&fixture), packs);
}
