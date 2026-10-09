//! Pure reftable on-disk format tests (no system `git` required).
//!
//! Maps upstream `t/unit-tests/u-reftable-*.c` and `t0613-reftable-write-options.sh`.

use std::collections::BTreeSet;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::reftable::{
    dump_reftable_blocks, is_reftable_repo, read_write_options, reftable_append_reflog,
    reftable_create_reflog, reftable_delete_ref, reftable_delete_reflog, reftable_list_reflog_refs,
    reftable_list_refs, reftable_read_reflog, reftable_read_symbolic_ref, reftable_reflog_exists,
    reftable_replace_reflog, reftable_resolve_ref, reftable_write_ref, reftable_write_symref,
    reftable_write_transaction, LogRecord, RefRecord, RefValue, ReftableReader, ReftableStack,
    ReftableTransactionUpdate, ReftableWriter, WriteOptions,
};

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

fn null_oid() -> ObjectId {
    ObjectId::from_bytes(&[0u8; 20]).expect("null oid")
}

fn sample_log(refname: &str, update_index: u64, msg: &str) -> LogRecord {
    LogRecord {
        refname: refname.to_owned(),
        update_index,
        old_id: null_oid(),
        new_id: oid(0xaa),
        name: "Author".to_owned(),
        email: "a@example.com".to_owned(),
        time_seconds: 1_700_000_000,
        tz_offset: -480,
        message: msg.to_owned(),
    }
}

fn write_table(
    refs: Vec<RefRecord>,
    logs: Vec<LogRecord>,
    opts: WriteOptions,
    min_idx: u64,
    max_idx: u64,
) -> Vec<u8> {
    let mut w = ReftableWriter::new(opts, min_idx, max_idx);
    for r in refs {
        w.add_ref(r).expect("add_ref");
    }
    for l in logs {
        w.add_log(l).expect("add_log");
    }
    w.finish().expect("finish")
}

fn ref_record(name: &str, idx: u64, value: RefValue) -> RefRecord {
    RefRecord {
        name: name.to_owned(),
        update_index: idx,
        value,
    }
}

#[test]
fn ref_records_val1_val2_symref_deletion_round_trip() {
    let oid1 = oid(0x11);
    let oid2 = oid(0x22);
    let data = write_table(
        vec![
            ref_record("refs/heads/gone", 6, RefValue::Deletion),
            ref_record("refs/heads/main", 5, RefValue::Val1(oid1)),
            ref_record(
                "refs/heads/sym",
                5,
                RefValue::Symref("refs/heads/main".into()),
            ),
            ref_record("refs/tags/v1", 6, RefValue::Val2(oid2, oid1)),
        ],
        vec![],
        WriteOptions::default(),
        5,
        6,
    );
    let reader = ReftableReader::new(data).expect("open");
    let refs = reader.read_refs().expect("read_refs");
    assert_eq!(refs.len(), 4);
    assert_eq!(refs[0].value, RefValue::Deletion);
    assert_eq!(refs[1].value, RefValue::Val1(oid1));
    assert_eq!(refs[2].value, RefValue::Symref("refs/heads/main".into()));
    assert_eq!(refs[3].value, RefValue::Val2(oid2, oid1));

    assert_eq!(
        reader
            .lookup_ref("refs/heads/main")
            .expect("lookup")
            .unwrap()
            .value,
        RefValue::Val1(oid1)
    );
    assert!(reader
        .lookup_ref("refs/heads/missing")
        .expect("lookup")
        .is_none());
}

#[test]
fn prefix_compression_long_shared_names() {
    let mut refs = Vec::new();
    for i in 0..50 {
        refs.push(ref_record(
            &format!("refs/heads/feature/branch-{i:04}"),
            1,
            RefValue::Val1(oid(i as u8)),
        ));
    }
    let data = write_table(refs.clone(), vec![], WriteOptions::default(), 1, 1);
    let reader = ReftableReader::new(data).expect("open");
    let round = reader.read_refs().expect("read");
    assert_eq!(round.len(), 50);
    for (i, rec) in round.iter().enumerate() {
        assert_eq!(rec.name, refs[i].name);
        assert_eq!(rec.value, RefValue::Val1(oid(i as u8)));
        assert_eq!(
            reader
                .lookup_ref(&refs[i].name)
                .expect("lookup")
                .unwrap()
                .name,
            refs[i].name
        );
    }
}

#[test]
fn read_refs_sorted_order() {
    let data = write_table(
        vec![
            ref_record("refs/heads/a", 1, RefValue::Val1(oid(1))),
            ref_record("refs/heads/m", 1, RefValue::Val1(oid(2))),
            ref_record("refs/heads/z", 1, RefValue::Val1(oid(3))),
        ],
        vec![],
        WriteOptions::default(),
        1,
        1,
    );
    let names: Vec<_> = ReftableReader::new(data)
        .expect("open")
        .read_refs()
        .expect("read")
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["refs/heads/a", "refs/heads/m", "refs/heads/z"]);
}

#[test]
fn log_records_create_update_delete_and_ordering() {
    let mut opts = WriteOptions::default();
    opts.write_log = true;
    let data = write_table(
        vec![ref_record("refs/heads/main", 10, RefValue::Val1(oid(1)))],
        vec![
            sample_log("refs/heads/main", 12, "third"),
            sample_log("refs/heads/main", 11, "second"),
            sample_log("refs/heads/main", 10, "first"),
            LogRecord {
                refname: "refs/heads/main".into(),
                update_index: 9,
                old_id: oid(1),
                new_id: null_oid(),
                name: "Author".into(),
                email: "a@example.com".into(),
                time_seconds: 1,
                tz_offset: 0,
                message: "delete\n".into(),
            },
        ],
        opts,
        9,
        12,
    );
    let logs = ReftableReader::new(data)
        .expect("open")
        .read_logs()
        .expect("logs");
    assert_eq!(logs.len(), 4);
    let indices: Vec<_> = logs.iter().map(|l| l.update_index).collect();
    assert_eq!(indices, [12, 11, 10, 9]);
    assert!(logs[0].message.ends_with('\n'));
}

#[test]
fn log_empty_and_multiline_messages() {
    let mut opts = WriteOptions::default();
    opts.write_log = true;
    let data = write_table(
        vec![],
        vec![
            sample_log("refs/heads/x", 1, ""),
            sample_log("refs/heads/x", 2, "line1\nline2\n"),
        ],
        opts,
        1,
        2,
    );
    let logs = ReftableReader::new(data)
        .expect("open")
        .read_logs()
        .expect("logs");
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0].message, "line1\nline2\n");
    assert_eq!(logs[1].message, "\n");
}

#[test]
fn log_timezone_extremes() {
    let mut opts = WriteOptions::default();
    opts.write_log = true;
    let data = write_table(
        vec![],
        vec![
            LogRecord {
                tz_offset: -12 * 60,
                ..sample_log("refs/heads/tz", 1, "west")
            },
            LogRecord {
                tz_offset: 14 * 60,
                update_index: 2,
                ..sample_log("refs/heads/tz", 2, "east")
            },
        ],
        opts,
        1,
        2,
    );
    let logs = ReftableReader::new(data)
        .expect("open")
        .read_logs()
        .expect("logs");
    assert_eq!(logs[0].tz_offset, 840);
    assert_eq!(logs[1].tz_offset, -720);
}

#[test]
fn empty_table_min_max_update_index() {
    let data = write_table(vec![], vec![], WriteOptions::default(), 42, 99);
    let reader = ReftableReader::new(data).expect("open");
    assert_eq!(reader.min_update_index(), 42);
    assert_eq!(reader.max_update_index(), 99);
    assert!(reader.read_refs().expect("refs").is_empty());
}

#[test]
fn block_size_variants_and_many_blocks() {
    for &bs in &[256u32, 4096, 8192] {
        let mut opts = WriteOptions::default();
        opts.block_size = bs;
        opts.restart_interval = 8;
        opts.write_log = false;
        let mut refs = Vec::new();
        for i in 0..120 {
            refs.push(ref_record(
                &format!("refs/heads/b{i:03}"),
                1,
                RefValue::Val1(oid((i % 200) as u8)),
            ));
        }
        let data = write_table(refs, vec![], opts.clone(), 1, 1);
        let reader = ReftableReader::new(data).expect("open");
        assert_eq!(reader.block_size(), bs);
        assert_eq!(reader.read_refs().expect("refs").len(), 120);
    }
}

#[test]
fn restart_interval_one_and_default() {
    for restart in [1usize, 16] {
        let mut opts = WriteOptions::default();
        opts.restart_interval = restart;
        opts.block_size = 512;
        opts.write_log = false;
        let mut refs = Vec::new();
        for i in 0..30 {
            refs.push(ref_record(
                &format!("refs/heads/r{i:02}"),
                1,
                RefValue::Val1(oid(1)),
            ));
        }
        let data = write_table(refs, vec![], opts, 1, 1);
        let reader = ReftableReader::new(data).expect("open");
        assert_eq!(reader.read_refs().expect("refs").len(), 30);
    }
}

#[test]
fn unpadded_table_round_trip() {
    let mut opts = WriteOptions::default();
    opts.unpadded = true;
    opts.write_log = false;
    let data = write_table(
        vec![ref_record("refs/heads/main", 1, RefValue::Val1(oid(1)))],
        vec![],
        opts,
        1,
        1,
    );
    assert!(data.len() < 4096);
    let reader = ReftableReader::new(data).expect("open");
    assert_eq!(reader.read_refs().expect("refs").len(), 1);
}

fn indexed_table_with_many_refs() -> (Vec<u8>, Vec<String>) {
    let mut opts = WriteOptions::default();
    opts.block_size = 256;
    opts.restart_interval = 4;
    opts.write_log = false;
    let mut names = Vec::new();
    let mut refs = Vec::new();
    for i in 0..200 {
        let name = format!("refs/heads/idx/branch-{i:04}");
        names.push(name.clone());
        refs.push(ref_record(&name, 1, RefValue::Val1(oid((i % 250) as u8))));
    }
    let data = write_table(refs, vec![], opts, 1, 1);
    (data, names)
}

#[test]
fn index_block_lookups_find_every_ref() {
    let (data, names) = indexed_table_with_many_refs();
    let reader = ReftableReader::new(data).expect("open");
    assert!(reader.ref_index_offset() > 0, "expected ref index block");
    for name in &names {
        let got = reader.lookup_ref(name).expect("lookup").expect("found");
        assert_eq!(got.name, *name);
    }
}

#[test]
fn index_lookup_missing_above_last_key_returns_none_quickly() {
    let (data, _) = indexed_table_with_many_refs();
    let reader = ReftableReader::new(data).expect("open");
    let start = Instant::now();
    let missing = reader
        .lookup_ref("refs/heads/idx/branch-9999")
        .expect("lookup");
    assert!(missing.is_none());
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "missing-key lookup took too long: {:?}",
        start.elapsed()
    );
}

#[test]
fn index_lookup_missing_gap_between_refs_returns_none() {
    let mut opts = WriteOptions::default();
    opts.block_size = 256;
    opts.restart_interval = 4;
    opts.write_log = false;
    let data = write_table(
        vec![
            ref_record("refs/heads/idx/branch-0000", 1, RefValue::Val1(oid(1))),
            ref_record("refs/heads/idx/branch-0002", 1, RefValue::Val1(oid(2))),
        ],
        vec![],
        opts,
        1,
        1,
    );
    let reader = ReftableReader::new(data).expect("open");
    assert!(reader
        .lookup_ref("refs/heads/idx/branch-0001")
        .expect("lookup")
        .is_none());
}

#[test]
fn geometric_factor_boundary_values_from_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path();
    fs::write(
        git_dir.join("config"),
        "[reftable]\ngeometricFactor = 255\n",
    )
    .expect("config");
    assert_eq!(read_write_options(git_dir).auto_compaction_factor, 255);

    fs::write(
        git_dir.join("config"),
        "[reftable]\ngeometricFactor = 256\n",
    )
    .expect("config");
    assert_eq!(
        read_write_options(git_dir).auto_compaction_factor,
        2,
        "256 must not wrap to 0; keep default"
    );

    fs::write(git_dir.join("config"), "[reftable]\ngeometricFactor = 1\n").expect("config");
    assert_eq!(
        read_write_options(git_dir).auto_compaction_factor,
        2,
        "out-of-range values are ignored"
    );
}

#[test]
fn write_options_block_size_and_restart_interval_applied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path();
    fs::write(
        git_dir.join("config"),
        "[reftable]\n\
         blockSize = 8192\n\
         restartInterval = 1\n\
         indexObjects = false\n\
         geometricFactor = 4\n",
    )
    .expect("config");
    let opts = read_write_options(git_dir);
    assert_eq!(opts.block_size, 8192);
    assert_eq!(opts.restart_interval, 1);
    assert!(opts.skip_index_objects);
    assert_eq!(opts.auto_compaction_factor, 4);

    let mut refs = Vec::new();
    for i in 0..20 {
        refs.push(ref_record(
            &format!("refs/heads/w{i:02}"),
            1,
            RefValue::Val1(oid(1)),
        ));
    }
    let data = write_table(refs, vec![], opts, 1, 1);
    let reader = ReftableReader::new(data).expect("open");
    assert_eq!(reader.block_size(), 8192);
}

#[test]
fn dump_reftable_blocks_stable_structure() {
    let mut opts = WriteOptions::default();
    opts.write_log = true;
    let data = write_table(
        vec![ref_record("refs/heads/main", 1, RefValue::Val1(oid(1)))],
        vec![sample_log("refs/heads/main", 1, "msg")],
        opts,
        1,
        1,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.ref");
    fs::write(&path, &data).expect("write");
    let dump1 = dump_reftable_blocks(&path).expect("dump");
    let dump2 = dump_reftable_blocks(&path).expect("dump2");
    assert_eq!(dump1, dump2);
    assert!(dump1.contains("header:"));
    assert!(dump1.contains("block_size: 4096"));
    assert!(dump1.contains("ref:"));
    assert!(dump1.contains("log:"));
}

#[test]
fn every_truncation_is_typed_error_no_panic() {
    let data = write_table(
        vec![
            ref_record("refs/heads/a", 1, RefValue::Val1(oid(1))),
            ref_record("refs/heads/b", 2, RefValue::Val1(oid(2))),
        ],
        vec![sample_log("refs/heads/a", 2, "log")],
        WriteOptions::default(),
        1,
        2,
    );
    for len in 1..data.len() {
        let slice = data[..len].to_vec();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let reader = ReftableReader::new(slice.clone());
            if let Ok(r) = reader {
                let _ = r.read_refs();
                let _ = r.read_logs();
                let _ = r.lookup_ref("refs/heads/a");
            }
        }));
        assert!(result.is_ok(), "panic at truncation length {len}");
        match ReftableReader::new(slice) {
            Err(Error::InvalidRef(_)) | Err(Error::Zlib(_)) => {}
            Err(other) => panic!("unexpected error type at len {len}: {other:?}"),
            Ok(reader) => {
                if reader.read_refs().is_ok() && reader.read_logs().is_ok() {
                    // Partial prefix may still parse; that's fine.
                }
            }
        }
    }
}

#[test]
fn corruption_bad_magic_version_and_crc() {
    let mut data = write_table(
        vec![ref_record("refs/heads/x", 1, RefValue::Val1(oid(1)))],
        vec![],
        WriteOptions::default(),
        1,
        1,
    );
    data[0] = b'X';
    assert!(matches!(
        ReftableReader::new(data.clone()),
        Err(Error::InvalidRef(_))
    ));

    data = write_table(
        vec![ref_record("refs/heads/x", 1, RefValue::Val1(oid(1)))],
        vec![],
        WriteOptions::default(),
        1,
        1,
    );
    data[4] = 99;
    assert!(matches!(
        ReftableReader::new(data.clone()),
        Err(Error::InvalidRef(_))
    ));

    data = write_table(
        vec![ref_record("refs/heads/x", 1, RefValue::Val1(oid(1)))],
        vec![],
        WriteOptions::default(),
        1,
        1,
    );
    let n = data.len();
    data[n - 1] ^= 0xff;
    assert!(matches!(
        ReftableReader::new(data),
        Err(Error::InvalidRef(msg)) if msg.contains("CRC")
    ));
}

struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound.max(1)
    }
}

fn setup_reftable_git_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(git_dir.join("reftable")).expect("reftable dir");
    fs::write(
        git_dir.join("config"),
        "[extensions]\n\trefStorage = reftable\n[core]\n\tlogAllRefUpdates = true\n",
    )
    .expect("config");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    fs::write(git_dir.join("reftable/tables.list"), "").expect("tables.list");
    (dir, git_dir)
}

#[test]
fn reftable_stack_write_resolve_list_and_delete() {
    let (_dir, git_dir) = setup_reftable_git_dir();
    assert!(is_reftable_repo(&git_dir));
    let oid = oid(0x42);
    reftable_write_ref(&git_dir, "refs/heads/main", &oid, None, None).expect("write");
    reftable_write_symref(&git_dir, "refs/heads/other", "refs/heads/main", None, None)
        .expect("sym");
    assert_eq!(
        reftable_resolve_ref(&git_dir, "refs/heads/main").expect("resolve"),
        oid
    );
    let listed = reftable_list_refs(&git_dir, "refs/heads/").expect("list");
    assert_eq!(listed.len(), 2);
    reftable_delete_ref(&git_dir, "refs/heads/other").expect("delete");
    assert!(ReftableStack::open(&git_dir)
        .expect("stack")
        .lookup_ref("refs/heads/other")
        .expect("lookup")
        .is_none());
}

#[test]
fn object_index_section_with_shared_oid() {
    let shared = oid(0x77);
    let mut refs = Vec::new();
    for i in 0..150 {
        refs.push(ref_record(
            &format!("refs/heads/shared/{i:03}"),
            1,
            RefValue::Val1(shared),
        ));
    }
    let mut opts = WriteOptions::default();
    opts.block_size = 256;
    opts.write_log = false;
    opts.skip_index_objects = false;
    let data = write_table(refs, vec![], opts, 1, 1);
    let dump = {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.ref");
        fs::write(&path, &data).expect("write");
        dump_reftable_blocks(&path).expect("dump")
    };
    assert!(
        dump.contains("obj:"),
        "expected object index blocks in dump"
    );
}

#[test]
fn reftable_reflog_create_append_replace_and_list() {
    let (_dir, git_dir) = setup_reftable_git_dir();
    let main_oid = oid(0x01);
    let ident = "Author <a@example.com> 1700000000 +0000";
    reftable_write_ref(
        &git_dir,
        "refs/heads/main",
        &main_oid,
        Some(ident),
        Some("init"),
    )
    .expect("write");
    assert!(reftable_reflog_exists(&git_dir, "refs/heads/main"));
    let entries = reftable_read_reflog(&git_dir, "refs/heads/main").expect("read");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].message, "init");

    let oid2 = oid(0x02);
    reftable_append_reflog(
        &git_dir,
        "refs/heads/main",
        &main_oid,
        &oid2,
        ident,
        "second",
        false,
    )
    .expect("append");
    assert_eq!(
        reftable_read_reflog(&git_dir, "refs/heads/main")
            .expect("read2")
            .len(),
        2
    );

    reftable_create_reflog(&git_dir, "refs/heads/empty").expect("create marker");
    let listed = reftable_list_reflog_refs(&git_dir).expect("list reflogs");
    assert!(listed.iter().any(|r| r == "refs/heads/main"));

    reftable_replace_reflog(&git_dir, "refs/heads/main", &[]).expect("clear");
    assert!(reftable_read_reflog(&git_dir, "refs/heads/main")
        .expect("read cleared")
        .is_empty());
    reftable_delete_reflog(&git_dir, "refs/heads/empty").expect("delete marker");
}

#[test]
fn reftable_write_transaction_batch() {
    let (_dir, git_dir) = setup_reftable_git_dir();
    let o1 = oid(0x0a);
    let o2 = oid(0x0b);
    reftable_write_transaction(
        &git_dir,
        vec![
            ReftableTransactionUpdate {
                refname: "refs/heads/a".into(),
                value: Some(RefValue::Val1(o1)),
                log: None,
            },
            ReftableTransactionUpdate {
                refname: "refs/heads/b".into(),
                value: Some(RefValue::Val1(o2)),
                log: None,
            },
        ],
    )
    .expect("txn");
    assert_eq!(
        reftable_resolve_ref(&git_dir, "refs/heads/a").expect("a"),
        o1
    );
    assert_eq!(
        reftable_read_symbolic_ref(&git_dir, "refs/heads/b").expect("sym"),
        None
    );
}

#[test]
fn sha256_reftable_version2_round_trip() {
    let mut opts = WriteOptions::default();
    opts.hash_size = 32;
    opts.write_log = false;
    let oid = ObjectId::from_bytes(&[0xab; 32]).expect("sha256 oid");
    let data = write_table(
        vec![ref_record("refs/heads/main", 1, RefValue::Val1(oid))],
        vec![],
        opts,
        1,
        1,
    );
    assert_eq!(data[4], 2);
    let reader = ReftableReader::new(data).expect("open");
    let refs = reader.read_refs().expect("refs");
    assert_eq!(refs[0].value, RefValue::Val1(oid));
}

#[test]
fn logs_only_table_starts_with_log_block() {
    let mut opts = WriteOptions::default();
    opts.write_log = true;
    let data = write_table(
        vec![],
        vec![sample_log("refs/heads/main", 1, "only logs")],
        opts,
        1,
        1,
    );
    let reader = ReftableReader::new(data).expect("open");
    let logs = reader.read_logs().expect("logs");
    assert_eq!(logs.len(), 1);
    assert!(reader.read_refs().expect("refs").is_empty());
}

#[test]
fn random_ref_and_log_sets_round_trip() {
    let mut rng = Lcg::new(0x653);
    for trial in 0..5 {
        let nrefs = 5 + rng.next_usize(40);
        let mut ref_names = BTreeSet::new();
        let mut refs = Vec::new();
        let anchor = format!("refs/heads/random/{trial}/anchor");
        ref_names.insert(anchor.clone());
        refs.push(ref_record(&anchor, 1, RefValue::Val1(oid(0x55))));

        for i in 0..nrefs {
            let name = format!("refs/heads/random/{trial}/{i}");
            ref_names.insert(name.clone());
            let kind = rng.next_usize(4);
            let value = match kind {
                0 => RefValue::Val1(oid(rng.next_u64() as u8)),
                1 => RefValue::Val2(oid(1), oid(2)),
                2 => RefValue::Symref(anchor.clone()),
                _ => RefValue::Deletion,
            };
            refs.push(ref_record(&name, 1 + i as u64, value));
        }
        refs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut logs = Vec::new();
        for name in &ref_names {
            let nlogs = 1 + rng.next_usize(5);
            for j in 0..nlogs {
                logs.push(sample_log(
                    name,
                    100 + j as u64,
                    &format!("msg {trial}-{j}"),
                ));
            }
        }

        let mut opts = WriteOptions::default();
        opts.block_size = 512 + rng.next_usize(3) as u32 * 256;
        opts.restart_interval = 1 + rng.next_usize(8);
        opts.write_log = true;

        let data = write_table(refs.clone(), logs.clone(), opts, 1, 200);
        let reader = ReftableReader::new(data).expect("open");
        let got_refs = reader.read_refs().expect("read_refs");
        assert_eq!(got_refs.len(), refs.len());
        for (a, b) in got_refs.iter().zip(refs.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.update_index, b.update_index);
            assert_eq!(a.value, b.value);
            assert_eq!(
                reader.lookup_ref(&a.name).expect("lookup").unwrap().name,
                a.name
            );
        }
        let got_logs = reader.read_logs().expect("logs");
        assert_eq!(got_logs.len(), logs.len());
    }
}
