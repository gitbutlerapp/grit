//! The graft log: which real commit each checkout commit stands for, and
//! back.
//!
//! A check-in grafts a checkout's commits onto the real history, and a
//! projection ([`super::project`]) shows a server branch's real commits to a
//! checkout as commits on its snapshots. Both record each pair they make, so
//! later work finds them: a commit built on a projected commit grafts onto
//! the real one it stands for, a check-in doesn't replay a commit it grafted
//! before, and a projection shows a checkout its own commits rather than new
//! copies of them.
//!
//! The pairs are kept in plain files under the directory the log is opened
//! at, in two sets: `raw/<xx>` by checkout commit and `real/<xx>` by real
//! commit, `<xx>` being the commit's first two hex digits, so a lookup reads
//! one small file however many pairs came before. Each line is
//! `<from> <to>`. Files are only ever appended to, with one write per file per
//! batch, so check-ins running at once don't interleave their lines. The way
//! back (`real/`) is written first: a pair whose way forward is missing is
//! made again, the same, by the next graft of its commit, which then writes
//! both; one missing only its way back would stay so. A line cut short (by a
//! crash mid-write) is ignored, and the next batch starts on a new line.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::objects::ObjectId;

use super::Result;

/// The side of the log a file belongs to.
#[derive(Debug, Clone, Copy)]
enum Side {
    /// Keyed by checkout commit: where each was grafted.
    Raw,
    /// Keyed by real commit: the checkout commits that stand for it.
    Real,
}

impl Side {
    fn dir(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Real => "real",
        }
    }
}

/// A file's pairs, by the commit they start from.
type Pairs = HashMap<ObjectId, Vec<ObjectId>>;

/// A repository's graft log, kept in a directory (see the module docs).
///
/// Files are read the first time a lookup needs them and kept, with what
/// this log records added, so one request reads each file at most once. A
/// log opened later sees everything recorded before.
#[derive(Debug)]
pub struct GraftLog {
    dir: PathBuf,
    read: Mutex<HashMap<PathBuf, Pairs>>,
}

impl GraftLog {
    /// The log kept in `dir`, which need not exist yet.
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            read: Mutex::new(HashMap::new()),
        }
    }

    /// The directory the log is kept in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, side: Side, oid: &ObjectId) -> PathBuf {
        let hex = oid.to_hex();
        self.dir.join(side.dir()).join(&hex[..2])
    }

    /// What `oid` maps to on `side`, oldest first.
    fn lookup(&self, side: Side, oid: &ObjectId) -> Result<Vec<ObjectId>> {
        let file = self.file(side, oid);
        let mut read = self.read.lock().unwrap_or_else(|e| e.into_inner());
        if !read.contains_key(&file) {
            let pairs = read_pairs(&file)?;
            read.insert(file.clone(), pairs);
        }
        Ok(read
            .get(&file)
            .and_then(|pairs| pairs.get(oid))
            .cloned()
            .unwrap_or_default())
    }

    /// The real commit the checkout commit `raw` stands for: the one it was
    /// grafted to, or the one it is a projection of.
    ///
    /// # Returns
    ///
    /// `None` when the log has no pair for `raw`; the first one recorded when
    /// it has several.
    ///
    /// # Errors
    ///
    /// Read failures other than a missing file.
    pub fn grafted(&self, raw: &ObjectId) -> Result<Option<ObjectId>> {
        Ok(self.lookup(Side::Raw, raw)?.into_iter().next())
    }

    /// The checkout commits that stand for the real commit `real`, oldest
    /// first: those grafted to it and its projections, from any checkout.
    ///
    /// # Errors
    ///
    /// Read failures other than a missing file.
    pub fn raws(&self, real: &ObjectId) -> Result<Vec<ObjectId>> {
        self.lookup(Side::Real, real)
    }

    /// Record `pairs`, each a checkout commit and the real commit it stands
    /// for. The way back is written first (see the module docs), and each
    /// file gets one append.
    ///
    /// # Errors
    ///
    /// Write failures; pairs written before one fails stay.
    pub fn record(&self, pairs: &[(ObjectId, ObjectId)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let back: Vec<(ObjectId, ObjectId)> =
            pairs.iter().map(|(raw, real)| (*real, *raw)).collect();
        self.append(Side::Real, &back)?;
        self.append(Side::Raw, pairs)
    }

    fn append(&self, side: Side, pairs: &[(ObjectId, ObjectId)]) -> Result<()> {
        let mut by_file: HashMap<PathBuf, String> = HashMap::new();
        {
            let mut read = self.read.lock().unwrap_or_else(|e| e.into_inner());
            for (from, to) in pairs {
                let file = self.file(side, from);
                by_file.entry(file.clone()).or_default().push_str(&format!(
                    "{} {}\n",
                    from.to_hex(),
                    to.to_hex()
                ));
                if let Some(known) = read.get_mut(&file) {
                    let tos = known.entry(*from).or_default();
                    if !tos.contains(to) {
                        tos.push(*to);
                    }
                }
            }
        }
        for (file, lines) in by_file {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(&file)?;
            // A line cut short by a crash would swallow the first one here.
            let text = if ends_mid_line(&mut out)? {
                format!("\n{lines}")
            } else {
                lines
            };
            out.write_all(text.as_bytes())?;
        }
        Ok(())
    }
}

/// Whether `file` is non-empty and its last byte is not a newline.
fn ends_mid_line(file: &mut std::fs::File) -> std::io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// The pairs in `file`, skipping lines that are not two whole object ids of
/// one hash (one cut short by a crash, or one another writer is still
/// writing).
fn read_pairs(file: &Path) -> Result<Pairs> {
    let text = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Pairs::new()),
        Err(e) => return Err(e.into()),
    };
    let mut pairs = Pairs::new();
    // Only lines that end in a newline are whole.
    let complete = text
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(&text[..0], |end| &text[..end]);
    for line in complete.split(|b| *b == b'\n') {
        let Some((from, to)) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.split_once(' '))
        else {
            continue;
        };
        let (Ok(from), Ok(to)) = (ObjectId::from_hex(from), ObjectId::from_hex(to)) else {
            continue;
        };
        // A SHA-256 id cut short at 40 digits would read as a SHA-1 one.
        if from.algo() != to.algo() {
            continue;
        }
        let tos = pairs.entry(from).or_default();
        if !tos.contains(&to) {
            tos.push(to);
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u32) -> ObjectId {
        ObjectId::from_hex(&format!("{n:040x}")).expect("oid")
    }

    /// An id whose first two hex digits are `prefix`.
    fn in_shard(prefix: &str, n: u32) -> ObjectId {
        ObjectId::from_hex(&format!("{prefix}{n:038x}")).expect("oid")
    }

    #[test]
    fn pairs_are_found_both_ways_by_this_log_and_later_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = GraftLog::at(dir.path().join("grafts"));
        assert_eq!(log.dir(), dir.path().join("grafts"));
        assert_eq!(log.grafted(&oid(1)).expect("lookup"), None);
        assert!(log.raws(&oid(100)).expect("lookup").is_empty());

        log.record(&[(oid(1), oid(100)), (oid(2), oid(100))])
            .expect("record");
        log.record(&[(oid(3), oid(300)), (oid(1), oid(100))])
            .expect("record");
        // This log saw its own writes, though it read the files before.
        assert_eq!(log.grafted(&oid(1)).expect("lookup"), Some(oid(100)));
        assert_eq!(log.grafted(&oid(3)).expect("lookup"), Some(oid(300)));
        assert_eq!(log.raws(&oid(100)).expect("lookup"), vec![oid(1), oid(2)]);

        // A log opened later reads the same, without duplicates.
        let later = GraftLog::at(dir.path().join("grafts"));
        assert_eq!(later.grafted(&oid(2)).expect("lookup"), Some(oid(100)));
        assert_eq!(later.raws(&oid(100)).expect("lookup"), vec![oid(1), oid(2)]);
        assert_eq!(later.raws(&oid(300)).expect("lookup"), vec![oid(3)]);
        assert_eq!(later.grafted(&oid(300)).expect("lookup"), None);
        assert_eq!(later.raws(&oid(3)).expect("lookup"), Vec::new());
    }

    #[test]
    fn files_are_split_by_the_first_two_hex_digits_and_appended_once_per_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = GraftLog::at(dir.path());
        let (a, b, c) = (in_shard("ab", 1), in_shard("ab", 2), in_shard("cd", 3));
        let real = in_shard("ef", 9);
        log.record(&[(a, real), (b, real), (c, real)])
            .expect("record");
        let raw_ab = std::fs::read_to_string(dir.path().join("raw/ab")).expect("raw/ab");
        assert_eq!(
            raw_ab,
            format!(
                "{} {}\n{} {}\n",
                a.to_hex(),
                real.to_hex(),
                b.to_hex(),
                real.to_hex()
            )
        );
        assert!(dir.path().join("raw/cd").is_file());
        let back = std::fs::read_to_string(dir.path().join("real/ef")).expect("real/ef");
        assert_eq!(back.lines().count(), 3);
        assert!(back.starts_with(&real.to_hex()));
        // Nothing to record writes nothing.
        let empty = tempfile::tempdir().expect("tempdir");
        GraftLog::at(empty.path().join("none"))
            .record(&[])
            .expect("record");
        assert!(!empty.path().join("none").exists());
    }

    #[test]
    fn a_line_cut_short_is_ignored_and_the_next_batch_starts_a_new_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, real) = (in_shard("12", 1), in_shard("34", 2));
        let (b, other) = (in_shard("12", 3), in_shard("34", 4));
        std::fs::create_dir_all(dir.path().join("raw")).expect("mkdir");
        // A whole line, then one a crash cut off.
        std::fs::write(
            dir.path().join("raw/12"),
            format!(
                "{} {}\n{} {}",
                a.to_hex(),
                real.to_hex(),
                b.to_hex(),
                &other.to_hex()[..9]
            ),
        )
        .expect("write");
        let log = GraftLog::at(dir.path());
        assert_eq!(log.grafted(&a).expect("lookup"), Some(real));
        assert_eq!(log.grafted(&b).expect("lookup"), None);

        // Its graft is made again and recorded: it is found, and so is the
        // first pair.
        log.record(&[(b, other)]).expect("record");
        let later = GraftLog::at(dir.path());
        assert_eq!(later.grafted(&b).expect("lookup"), Some(other));
        assert_eq!(later.grafted(&a).expect("lookup"), Some(real));
        assert_eq!(later.raws(&other).expect("lookup"), vec![b]);
        // Junk is skipped too.
        std::fs::write(dir.path().join("raw/56"), "not a pair\n\nxyz abc\n").expect("write");
        assert_eq!(later.grafted(&in_shard("56", 1)).expect("lookup"), None);
    }

    #[test]
    fn check_ins_recording_at_once_keep_every_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let real = in_shard("aa", 0);
        let writers: Vec<_> = (0..8u32)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let log = GraftLog::at(path);
                    for i in 0..25u32 {
                        let raw = in_shard("bb", w * 100 + i);
                        log.record(&[(raw, real)]).expect("record");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        let log = GraftLog::at(path);
        assert_eq!(log.raws(&real).expect("lookup").len(), 200);
        for w in 0..8u32 {
            for i in 0..25u32 {
                assert_eq!(
                    log.grafted(&in_shard("bb", w * 100 + i)).expect("lookup"),
                    Some(real)
                );
            }
        }
    }
}
