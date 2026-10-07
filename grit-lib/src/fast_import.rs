//! Minimal [`git fast-import`](https://git-scm.com/docs/git-fast-import) stream support.
//!
//! Handles the subset of commands used by upstream tests: `blob` (with optional
//! `mark`), `commit` (with `author`/`committer`, `data` in byte-count or `<<delim>`
//! form, optional `from`, `deleteall`, `M` / `D` file commands, `M ... inline`
//! with a following `data` command, and `N` / `N inline` on `refs/notes/*`),
//! `reset`, `done`, and comment lines.

use std::collections::HashMap;
use std::io::BufRead;

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::ConfigSet;
use crate::diff::zero_oid;
use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry, MODE_GITLINK, MODE_REGULAR, MODE_TREE};
use crate::objects::{
    parse_commit, serialize_commit, serialize_tag, CommitData, ObjectId, ObjectKind, TagData,
};
use crate::refs::{
    append_reflog, read_head, resolve_ref, should_autocreate_reflog_for_mode, write_ref,
    LogRefsConfig,
};
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;
use crate::write_tree::write_tree_from_index;

/// Options for [`import_stream`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FastImportOptions {
    /// When true, allow updating refs that would otherwise be rejected as non-fast-forward
    /// (Git's `feature force` / `--force`).
    pub force: bool,
}

/// Import objects and refs from a fast-import stream read from `reader`.
///
/// # Errors
///
/// Returns [`Error`] variants for I/O, corrupt stream input, or missing marks/refs.
pub fn import_stream(repo: &Repository, reader: impl BufRead) -> Result<()> {
    import_stream_with_options(repo, reader, FastImportOptions::default())
}

/// Import with explicit options (e.g. `--force`).
pub fn import_stream_with_options(
    repo: &Repository,
    mut reader: impl BufRead,
    options: FastImportOptions,
) -> Result<()> {
    let log_refs = ConfigSet::load(Some(&repo.git_dir), true)
        .map(|c| c.effective_log_refs_config(&repo.git_dir))
        .unwrap_or_else(|_| crate::refs::effective_log_refs_config(&repo.git_dir));
    let mut imp = Importer {
        repo,
        log_refs,
        marks: HashMap::new(),
        branch_tips: HashMap::new(),
        feature_done: false,
        stashed_line: None,
        pending_byte: None,
        force: options.force,
        reader: &mut reader,
    };
    imp.run()
}

struct Importer<'a, R: BufRead> {
    repo: &'a Repository,
    log_refs: LogRefsConfig,
    marks: HashMap<u32, ObjectId>,
    branch_tips: HashMap<String, ObjectId>,
    /// When set, a terminating `done` command is required before EOF.
    feature_done: bool,
    /// Line read too far while parsing a `commit` or `reset`; next top-level command.
    stashed_line: Option<String>,
    /// Byte read while handling optional `LF` after a `data` block; must precede next line.
    pending_byte: Option<u8>,
    force: bool,
    reader: &'a mut R,
}

impl<'a, R: BufRead> Importer<'a, R> {
    fn fast_import_reflog_identity_from_env() -> String {
        let name = std::env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "Unknown".to_owned());
        let email = std::env::var("GIT_COMMITTER_EMAIL").unwrap_or_default();
        let date = std::env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{now} +0000")
        });
        format!("{name} <{email}> {date}")
    }

    /// Update a ref and append a reflog line (matches `git fast-import` ref transactions).
    fn update_ref_with_reflog(
        &self,
        refname: &str,
        new_oid: &ObjectId,
        identity: &str,
        message: &str,
    ) -> Result<()> {
        let old_oid = resolve_ref(&self.repo.git_dir, refname).unwrap_or_else(|_| zero_oid());
        write_ref(&self.repo.git_dir, refname, new_oid)?;
        if should_autocreate_reflog_for_mode(refname, self.log_refs) {
            let _ = append_reflog(
                &self.repo.git_dir,
                refname,
                &old_oid,
                new_oid,
                identity,
                message,
                false,
            );
        }
        Ok(())
    }

    /// Apply a fast-import update for `refname`, treating symbolic `HEAD` like Git's harness.
    ///
    /// When `refname` is `HEAD` and `HEAD` points at `refs/heads/<branch>`, update that branch
    /// and keep `HEAD` symbolic. Writing a raw OID into `HEAD` would detach it and break helpers
    /// such as `test_commit_bulk` followed by `git branch -M` (e.g. t5327 with
    /// `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME`).
    fn update_ref_for_fast_import(
        &self,
        refname: &str,
        new_oid: &ObjectId,
        identity: &str,
        message: &str,
    ) -> Result<()> {
        if refname == "HEAD" {
            if let Some(target) = read_head(&self.repo.git_dir)? {
                if target.starts_with("refs/heads/") {
                    return self.update_ref_with_reflog(&target, new_oid, identity, message);
                }
            }
        }
        self.update_ref_with_reflog(refname, new_oid, identity, message)
    }

    /// Read a `data` command body: either `data <n>` (exact bytes) or `data <<delim>` (line-delimited).
    fn read_data_payload(&mut self, data_line_trimmed: &str) -> Result<Vec<u8>> {
        let rest = data_line_trimmed.strip_prefix("data ").ok_or_else(|| {
            Error::IndexError(format!(
                "fast-import: expected data line, got: {data_line_trimmed}"
            ))
        })?;
        if let Some(delim) = rest.strip_prefix("<<") {
            let delim = delim.trim_end();
            if delim.is_empty() {
                return Err(Error::IndexError(
                    "fast-import: empty data delimiter".to_owned(),
                ));
            }
            return self.read_data_delimited(delim);
        }
        let size: usize = rest
            .trim_end()
            .parse()
            .map_err(|_| Error::IndexError(format!("fast-import: invalid data size: {rest}")))?;
        let mut payload = vec![0u8; size];
        self.reader
            .read_exact(&mut payload)
            .map_err(|_| Error::IndexError("fast-import: truncated data".to_owned()))?;
        self.consume_optional_lf_after_data()?;
        Ok(payload)
    }

    /// Delimited `data` format: raw lines until a line equal to `delim` (see git-fast-import).
    fn read_data_delimited(&mut self, delim: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let line = self.read_line_any()?.ok_or_else(|| {
                Error::IndexError(format!(
                    "fast-import: EOF in data (terminator '{delim}' not found)"
                ))
            })?;
            if line.trim_end() == delim {
                break;
            }
            out.extend_from_slice(line.as_bytes());
        }
        self.consume_optional_lf_after_data()?;
        Ok(out)
    }

    fn run(&mut self) -> Result<()> {
        while let Some(line) = self.next_command_line()? {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "done" {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("feature ") {
                let name = rest.trim();
                if name == "force" {
                    self.force = true;
                } else if name == "done" {
                    self.feature_done = true;
                }
                continue;
            }
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed == "blob" {
                self.read_blob()?;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("commit ") {
                let refname = rest.trim().to_string();
                validate_fast_import_refname(&refname)?;
                self.read_commit(&refname)?;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("reset ") {
                let refname = rest.trim().to_string();
                validate_fast_import_refname(&refname)?;
                self.read_reset(&refname)?;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("tag ") {
                let name = rest.trim().to_string();
                self.read_tag(&name)?;
                continue;
            }
            return Err(Error::IndexError(format!(
                "fast-import: unsupported command: {trimmed}"
            )));
        }
        if self.feature_done {
            return Err(Error::IndexError(
                "fast-import: stream ended before required \"done\" command".to_owned(),
            ));
        }
        Ok(())
    }

    fn next_command_line(&mut self) -> Result<Option<String>> {
        if let Some(l) = self.stashed_line.take() {
            return Ok(Some(l));
        }
        self.read_line_nonempty()
    }

    fn read_line_nonempty(&mut self) -> Result<Option<String>> {
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self.read_line_into(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            if !buf.trim().is_empty() {
                return Ok(Some(buf));
            }
        }
    }

    fn read_line_any(&mut self) -> Result<Option<String>> {
        let mut buf = String::new();
        let n = self.read_line_into(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(buf))
    }

    fn read_line_into(&mut self, buf: &mut String) -> Result<usize> {
        buf.clear();
        if let Some(b) = self.pending_byte.take() {
            if b == b'\n' {
                buf.push('\n');
                return Ok(1);
            }
            buf.push(char::from(b));
        }
        let prev = buf.len();
        let n = self.reader.read_line(buf).map_err(Error::Io)?;
        Ok(prev + n)
    }

    fn read_blob(&mut self) -> Result<()> {
        let mut mark: Option<u32> = None;
        loop {
            let line = self.read_line_nonempty()?.ok_or_else(|| {
                Error::IndexError("fast-import: unexpected EOF in blob".to_owned())
            })?;
            let t = line.trim_end();
            if let Some(id) = t.strip_prefix("mark :") {
                mark = Some(
                    id.parse()
                        .map_err(|_| Error::IndexError(format!("fast-import: bad mark: {t}")))?,
                );
                continue;
            }
            if t.starts_with("original-oid ") {
                continue;
            }
            let payload = self.read_data_payload(t)?;
            let oid = self.repo.odb.write(ObjectKind::Blob, &payload)?;
            if let Some(m) = mark {
                self.marks.insert(m, oid);
            }
            return Ok(());
        }
    }

    /// After `data` payload, an extra LF is optional (see git-fast-import docs).
    fn consume_optional_lf_after_data(&mut self) -> Result<()> {
        let mut one = [0u8; 1];
        match self.reader.read(&mut one) {
            Ok(0) => Ok(()),
            Ok(1) => {
                if one[0] != b'\n' {
                    self.pending_byte = Some(one[0]);
                }
                Ok(())
            }
            Ok(_) => unreachable!(),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn read_commit(&mut self, refname: &str) -> Result<()> {
        let mut mark: Option<u32> = None;
        let mut author: Option<String> = None;
        let mut committer: Option<String> = None;

        loop {
            let line = self.read_line_nonempty()?.ok_or_else(|| {
                Error::IndexError("fast-import: unexpected EOF in commit".to_owned())
            })?;
            let t = line.trim_end();
            if let Some(id) = t.strip_prefix("mark :") {
                mark = Some(
                    id.parse()
                        .map_err(|_| Error::IndexError(format!("fast-import: bad mark: {t}")))?,
                );
                continue;
            }
            if t.starts_with("original-oid ") {
                continue;
            }
            if let Some(rest) = t.strip_prefix("author ") {
                author = Some(rest.to_owned());
                continue;
            }
            if let Some(rest) = t.strip_prefix("committer ") {
                committer = Some(rest.to_owned());
                continue;
            }
            if t.starts_with("gpgsig ") || t.starts_with("encoding ") {
                return Err(Error::IndexError(format!(
                    "fast-import: unsupported commit header: {t}"
                )));
            }
            if t.starts_with("data ") {
                let message = self.read_data_payload(t)?;
                let committer = committer.ok_or_else(|| {
                    Error::IndexError("fast-import: commit missing committer".to_owned())
                })?;
                let author = author.unwrap_or_else(|| committer.clone());
                self.finish_commit(refname, mark, author, committer, message)?;
                return Ok(());
            }
            return Err(Error::IndexError(format!(
                "fast-import: unexpected in commit before message: {t}"
            )));
        }
    }

    fn finish_commit(
        &mut self,
        refname: &str,
        mark: Option<u32>,
        author: String,
        committer: String,
        message: Vec<u8>,
    ) -> Result<()> {
        #[derive(Debug)]
        enum FileChangeOp {
            DeleteAll,
            Delete(Vec<u8>),
            Modify {
                mode: u32,
                blob_oid: ObjectId,
                path: Vec<u8>,
            },
            NoteModify {
                blob_oid: ObjectId,
                target_commit: ObjectId,
            },
            Rename(Vec<u8>, Vec<u8>),
            Copy(Vec<u8>, Vec<u8>),
        }

        let mut from_oid: Option<ObjectId> = None;
        let mut merge_oids: Vec<ObjectId> = Vec::new();
        let mut ops: Vec<FileChangeOp> = Vec::new();
        let mut pending_inline: Option<(u32, Vec<u8>)> = None;
        let notes_ref = refname.starts_with("refs/notes/");

        while let Some(line) = self.read_line_any()? {
            let t = line.trim_end();
            if t.is_empty() {
                continue;
            }
            if let Some((mode, path)) = pending_inline.take() {
                if !t.starts_with("data ") {
                    return Err(Error::IndexError(format!(
                        "fast-import: expected data after M ... inline, got: {t}"
                    )));
                }
                let payload = self.read_data_payload(t)?;
                let blob_oid = self.repo.odb.write(ObjectKind::Blob, &payload)?;
                ops.push(FileChangeOp::Modify {
                    mode,
                    blob_oid,
                    path,
                });
                continue;
            }
            if let Some(spec) = t.strip_prefix("from ") {
                from_oid = Some(self.resolve_commit_ish(spec.trim())?);
                continue;
            }
            if let Some(spec) = t.strip_prefix("merge ") {
                let spec = spec.trim();
                merge_oids.push(self.resolve_commit_ish(spec)?);
                continue;
            }
            if t == "deleteall" {
                ops.push(FileChangeOp::DeleteAll);
                continue;
            }
            if let Some(rest) = t.strip_prefix("M ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() < 3 {
                    return Err(Error::IndexError(format!("fast-import: bad M line: {t}")));
                }
                let mode = u32::from_str_radix(parts[0], 8).map_err(|_| {
                    Error::IndexError(format!("fast-import: bad file mode: {}", parts[0]))
                })?;
                let blob_ref = parts[1];
                if parts.len() != 3 {
                    return Err(Error::IndexError(format!("fast-import: bad M line: {t}")));
                }
                let path = parts[2].as_bytes().to_vec();
                if blob_ref == "inline" {
                    pending_inline = Some((mode, path));
                    continue;
                }
                let blob_oid = self.resolve_blob_ref(blob_ref)?;
                ops.push(FileChangeOp::Modify {
                    mode,
                    blob_oid,
                    path,
                });
                continue;
            }
            if let Some(rest) = t.strip_prefix("D ") {
                ops.push(FileChangeOp::Delete(rest.as_bytes().to_vec()));
                continue;
            }
            if let Some(rest) = t.strip_prefix("N ") {
                if !notes_ref {
                    return Err(Error::IndexError(format!(
                        "fast-import: N (notemodify) only allowed on refs/notes/*, not {refname}"
                    )));
                }
                let (data_ref, commit_spec) = parse_notemodify_operands(rest)?;
                let target_commit = self.resolve_note_target_commit(commit_spec)?;
                let blob_oid = match data_ref {
                    NoteBlobSpec::Inline => {
                        let next = self.read_line_nonempty()?.ok_or_else(|| {
                            Error::IndexError(
                                "fast-import: expected data after N inline".to_owned(),
                            )
                        })?;
                        let nt = next.trim_end();
                        if !nt.starts_with("data ") {
                            return Err(Error::IndexError(format!(
                                "fast-import: expected data after N inline, got: {nt}"
                            )));
                        }
                        let payload = self.read_data_payload(nt)?;
                        self.repo.odb.write(ObjectKind::Blob, &payload)?
                    }
                    NoteBlobSpec::Mark(id) => *self.marks.get(&id).ok_or_else(|| {
                        Error::IndexError(format!("fast-import: unknown mark :{id}"))
                    })?,
                    NoteBlobSpec::Oid(oid) => {
                        if oid.is_zero() {
                            ObjectId::zero()
                        } else {
                            let obj = self.repo.odb.read(&oid)?;
                            if obj.kind != ObjectKind::Blob {
                                return Err(Error::IndexError(format!(
                                    "fast-import: N dataref {oid} is not a blob"
                                )));
                            }
                            oid
                        }
                    }
                };
                ops.push(FileChangeOp::NoteModify {
                    blob_oid,
                    target_commit,
                });
                continue;
            }
            if let Some(rest) = t.strip_prefix("R ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() != 2 {
                    return Err(Error::IndexError(format!("fast-import: bad R line: {t}")));
                }
                ops.push(FileChangeOp::Rename(
                    parts[0].as_bytes().to_vec(),
                    parts[1].as_bytes().to_vec(),
                ));
                continue;
            }
            if let Some(rest) = t.strip_prefix("C ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() != 2 {
                    return Err(Error::IndexError(format!("fast-import: bad C line: {t}")));
                }
                ops.push(FileChangeOp::Copy(
                    parts[0].as_bytes().to_vec(),
                    parts[1].as_bytes().to_vec(),
                ));
                continue;
            }
            self.stashed_line = Some(line);
            break;
        }

        if pending_inline.is_some() {
            return Err(Error::IndexError(
                "fast-import: unterminated M ... inline (missing data)".to_owned(),
            ));
        }

        let empty_tree: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
            .parse()
            .map_err(|_| Error::IndexError("fast-import: empty tree oid".to_owned()))?;

        let mut parents: Vec<ObjectId> = Vec::new();
        if let Some(oid) = from_oid {
            parents.push(oid);
        }
        parents.extend(merge_oids);

        let (parent_tree, parents_for_commit) = if let Some(&first_parent) = parents.first() {
            let obj = self.repo.odb.read(&first_parent)?;
            if obj.kind != ObjectKind::Commit {
                return Err(Error::IndexError(format!(
                    "fast-import: parent {first_parent} is not a commit"
                )));
            }
            let c = parse_commit(&obj.data)?;
            (c.tree, parents)
        } else if let Some(tip) = self.branch_tips.get(refname).copied() {
            let obj = self.repo.odb.read(&tip)?;
            if obj.kind != ObjectKind::Commit {
                return Err(Error::IndexError(format!(
                    "fast-import: branch tip {tip} is not a commit"
                )));
            }
            let c = parse_commit(&obj.data)?;
            (c.tree, vec![tip])
        } else {
            (empty_tree, Vec::new())
        };

        let mut index = tree_to_index(&self.repo.odb, &parent_tree)?;
        for op in ops {
            match op {
                FileChangeOp::DeleteAll => index.entries.clear(),
                FileChangeOp::Delete(path) => {
                    index.entries.retain(|e| e.path != path);
                }
                FileChangeOp::Modify {
                    mode,
                    blob_oid,
                    path,
                } => {
                    let mode = normalize_mode(mode)?;
                    index.add_or_replace(index_entry(path, mode, blob_oid));
                }
                FileChangeOp::NoteModify {
                    blob_oid,
                    target_commit,
                } => {
                    remove_note_entries_for_target(&mut index, &target_commit);
                    if !blob_oid.is_zero() {
                        let after_remove = count_notes_in_index(&index);
                        let fanout = notes_fanout_for_count(after_remove.saturating_add(1));
                        let note_path = construct_note_path_with_fanout(&target_commit, fanout);
                        index.add_or_replace(index_entry(note_path, MODE_REGULAR, blob_oid));
                    }
                }
                FileChangeOp::Rename(src, dst) => {
                    let Some(pos) = index.entries.iter().position(|e| e.path == src) else {
                        return Err(Error::IndexError(format!(
                            "fast-import: filerename source missing: {}",
                            String::from_utf8_lossy(&src)
                        )));
                    };
                    let mut ent = index.entries.remove(pos);
                    ent.path = dst;
                    index.add_or_replace(ent);
                }
                FileChangeOp::Copy(src, dst) => {
                    let Some(ent) = index.entries.iter().find(|e| e.path == src).cloned() else {
                        return Err(Error::IndexError(format!(
                            "fast-import: filecopy source missing: {}",
                            String::from_utf8_lossy(&src)
                        )));
                    };
                    let mut copy_ent = ent;
                    copy_ent.path = dst;
                    index.add_or_replace(copy_ent);
                }
            }
        }

        if notes_ref && count_notes_in_index(&index) > 0 {
            let n = count_notes_in_index(&index);
            rewrite_notes_fanout_in_index(&self.repo.odb, &mut index, notes_fanout_for_count(n))?;
        }

        let tree_oid = write_tree_from_index(&self.repo.odb, &index, "")?;

        let message_str = String::from_utf8_lossy(&message).into_owned();
        let raw_message = (!message.is_empty() && std::str::from_utf8(&message).is_err())
            .then_some(message.clone());
        let reflog_identity = committer.clone();

        let commit = CommitData {
            tree: tree_oid,
            parents: parents_for_commit,
            author,
            committer,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: message_str,
            raw_message,
        };
        let bytes = serialize_commit(&commit);
        let commit_oid = self.repo.odb.write(ObjectKind::Commit, &bytes)?;

        if let Some(m) = mark {
            self.marks.insert(m, commit_oid);
        }
        self.branch_tips.insert(refname.to_string(), commit_oid);
        if !self.force {
            if let Ok(old) = crate::refs::resolve_ref(&self.repo.git_dir, refname) {
                if old != commit_oid {
                    let is_ancestor =
                        crate::merge_base::is_ancestor(self.repo, old, commit_oid).unwrap_or(false);
                    if !is_ancestor {
                        return Err(Error::IndexError(format!(
                            "fast-import: refusing non-fast-forward update of {refname} (use feature force or --force)"
                        )));
                    }
                }
            }
        }
        self.update_ref_for_fast_import(refname, &commit_oid, &reflog_identity, "fast-import")?;
        Ok(())
    }

    fn resolve_commit_ish(&self, spec: &str) -> Result<ObjectId> {
        if let Some(rest) = spec.strip_prefix(':') {
            let id: u32 = rest
                .parse()
                .map_err(|_| Error::IndexError(format!("fast-import: bad mark ref: {spec}")))?;
            return self
                .marks
                .get(&id)
                .copied()
                .ok_or_else(|| Error::IndexError(format!("fast-import: unknown mark :{id}")));
        }
        if ObjectId::is_full_hex(spec) {
            return spec.parse();
        }
        resolve_revision(self.repo, spec)
    }

    fn resolve_blob_ref(&self, spec: &str) -> Result<ObjectId> {
        if let Some(rest) = spec.strip_prefix(':') {
            let id: u32 = rest
                .parse()
                .map_err(|_| Error::IndexError(format!("fast-import: bad mark ref: {spec}")))?;
            return self
                .marks
                .get(&id)
                .copied()
                .ok_or_else(|| Error::IndexError(format!("fast-import: unknown mark :{id}")));
        }
        if ObjectId::is_full_hex(spec) {
            return spec.parse();
        }
        Err(Error::IndexError(format!(
            "fast-import: unsupported blob ref: {spec}"
        )))
    }

    fn read_tag(&mut self, short_name: &str) -> Result<()> {
        let mut mark: Option<u32> = None;
        let mut from_oid: Option<ObjectId> = None;
        let mut tagger: Option<String> = None;

        loop {
            let line = self.read_line_nonempty()?.ok_or_else(|| {
                Error::IndexError("fast-import: unexpected EOF in tag".to_owned())
            })?;
            let t = line.trim_end();
            if let Some(id) = t.strip_prefix("mark :") {
                mark = Some(
                    id.parse()
                        .map_err(|_| Error::IndexError(format!("fast-import: bad mark: {t}")))?,
                );
                continue;
            }
            if t.starts_with("original-oid ") {
                continue;
            }
            if let Some(rest) = t.strip_prefix("from ") {
                let spec = rest.trim();
                from_oid = Some(self.resolve_commit_ish(spec)?);
                continue;
            }
            if let Some(rest) = t.strip_prefix("tagger ") {
                tagger = Some(rest.to_owned());
                continue;
            }
            if t.starts_with("data ") {
                let message = self.read_data_payload(t)?;

                let target = from_oid
                    .ok_or_else(|| Error::IndexError("fast-import: tag missing from".to_owned()))?;
                let target_obj = self.repo.odb.read(&target)?;
                let object_type = target_obj.kind.as_str().to_owned();
                let msg_str = String::from_utf8_lossy(&message).into_owned();

                let reflog_ident = tagger
                    .clone()
                    .unwrap_or_else(Self::fast_import_reflog_identity_from_env);
                let tag_data = TagData {
                    object: target,
                    object_type,
                    tag: short_name.to_owned(),
                    tagger,
                    message: msg_str,
                };
                let bytes = serialize_tag(&tag_data);
                let tag_oid = self.repo.odb.write(ObjectKind::Tag, &bytes)?;

                if let Some(m) = mark {
                    self.marks.insert(m, tag_oid);
                }

                let full_ref = format!("refs/tags/{short_name}");
                self.update_ref_with_reflog(&full_ref, &tag_oid, &reflog_ident, "fast-import")?;
                return Ok(());
            }
            return Err(Error::IndexError(format!(
                "fast-import: unexpected in tag: {t}"
            )));
        }
    }

    fn read_reset(&mut self, refname: &str) -> Result<()> {
        let Some(line) = self.read_line_any()? else {
            return Ok(());
        };
        let t = line.trim_end();
        if t.is_empty() {
            return Ok(());
        }
        if let Some(spec) = t.strip_prefix("from ") {
            let oid = self.resolve_commit_ish(spec.trim())?;
            self.branch_tips.insert(refname.to_string(), oid);
            if !self.force {
                if let Ok(old) = crate::refs::resolve_ref(&self.repo.git_dir, refname) {
                    if old != oid {
                        let is_ancestor =
                            crate::merge_base::is_ancestor(self.repo, old, oid).unwrap_or(false);
                        if !is_ancestor {
                            return Err(Error::IndexError(format!(
                                "fast-import: refusing non-fast-forward reset of {refname}"
                            )));
                        }
                    }
                }
            }
            let ident = Self::fast_import_reflog_identity_from_env();
            self.update_ref_for_fast_import(refname, &oid, &ident, "fast-import")?;
            return Ok(());
        }
        self.stashed_line = Some(line);
        Ok(())
    }

    /// Resolve the commit a `notemodify` annotates (branch tip, mark, rev, or full hex).
    fn resolve_note_target_commit(&self, spec: &str) -> Result<ObjectId> {
        let oid = if let Some(tip) = self.branch_tips.get(spec) {
            *tip
        } else {
            self.resolve_commit_ish(spec)?
        };
        let obj = self.repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::IndexError(format!(
                "fast-import: notemodify target {spec} is not a commit"
            )));
        }
        Ok(oid)
    }
}

fn validate_fast_import_refname(refname: &str) -> Result<()> {
    if refname == "HEAD" {
        return Ok(());
    }
    let full = if refname.starts_with("refs/") {
        refname.to_owned()
    } else {
        format!("refs/heads/{refname}")
    };
    check_refname_format(&full, &RefNameOptions::default())
        .map(|_| ())
        .map_err(|e| Error::IndexError(format!("fast-import: invalid ref name '{refname}': {e}")))
}

/// Blob payload source in a `N` (notemodify) command.
enum NoteBlobSpec {
    Inline,
    Mark(u32),
    Oid(ObjectId),
}

fn parse_notemodify_operands(rest: &str) -> Result<(NoteBlobSpec, &str)> {
    let s = rest.trim();
    if let Some(commit_spec) = s.strip_prefix("inline ") {
        return Ok((NoteBlobSpec::Inline, commit_spec.trim()));
    }
    if let Some(after_colon) = s.strip_prefix(':') {
        let space = after_colon
            .find(' ')
            .ok_or_else(|| Error::IndexError("fast-import: bad N line (mark)".to_owned()))?;
        let id: u32 = after_colon[..space]
            .parse()
            .map_err(|_| Error::IndexError(format!("fast-import: bad mark in N line: {s}")))?;
        return Ok((NoteBlobSpec::Mark(id), after_colon[space + 1..].trim()));
    }
    if s.len() < 41 {
        return Err(Error::IndexError(format!(
            "fast-import: bad N line (expected oid + commit-ish): {s}"
        )));
    }
    let head = &s[..40];
    if !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::IndexError(format!(
            "fast-import: bad N line (invalid blob oid): {s}"
        )));
    }
    if s.as_bytes().get(40) != Some(&b' ') {
        return Err(Error::IndexError(format!(
            "fast-import: bad N line (missing space after blob oid): {s}"
        )));
    }
    let oid: ObjectId = head
        .parse()
        .map_err(|_| Error::IndexError(format!("fast-import: bad blob oid in N line: {s}")))?;
    Ok((NoteBlobSpec::Oid(oid), s[41..].trim()))
}

fn is_note_index_path(path: &[u8]) -> bool {
    let compact: Vec<u8> = path.iter().copied().filter(|b| *b != b'/').collect();
    compact.len() == 40 && compact.iter().all(u8::is_ascii_hexdigit)
}

fn compact_hex_from_note_path(path: &[u8]) -> Option<String> {
    if !is_note_index_path(path) {
        return None;
    }
    let s: String = path
        .iter()
        .copied()
        .filter(|b| *b != b'/')
        .map(|b| char::from(b).to_ascii_lowercase())
        .collect();
    Some(s)
}

fn count_notes_in_index(index: &crate::index::Index) -> usize {
    index
        .entries
        .iter()
        .filter(|e| is_note_index_path(&e.path))
        .count()
}

fn notes_fanout_for_count(mut n: usize) -> usize {
    let mut fanout = 0usize;
    while n > 0xff {
        n >>= 8;
        fanout += 1;
    }
    fanout
}

fn construct_note_path_with_fanout(commit: &ObjectId, fanout: usize) -> Vec<u8> {
    let hex = commit.to_hex();
    let bytes = hex.as_bytes();
    let split = fanout.min(bytes.len() / 2);
    let mut out = Vec::with_capacity(hex.len() + split);
    for i in 0..split {
        let start = i * 2;
        out.extend_from_slice(&bytes[start..start + 2]);
        out.push(b'/');
    }
    out.extend_from_slice(&bytes[split * 2..]);
    out
}

fn remove_note_entries_for_target(index: &mut crate::index::Index, target: &ObjectId) {
    let want = target.to_hex();
    index.entries.retain(|e| {
        if !is_note_index_path(&e.path) {
            return true;
        }
        compact_hex_from_note_path(&e.path).as_deref() != Some(want.as_str())
    });
}

fn rewrite_notes_fanout_in_index(
    odb: &crate::odb::Odb,
    index: &mut crate::index::Index,
    fanout: usize,
) -> Result<()> {
    let mut notes: Vec<(ObjectId, ObjectId, u32)> = Vec::new();
    let mut kept = Vec::new();
    for e in index.entries.drain(..) {
        if is_note_index_path(&e.path) {
            let Some(compact) = compact_hex_from_note_path(&e.path) else {
                continue;
            };
            let commit_oid = compact
                .parse()
                .map_err(|_| Error::IndexError("fast-import: bad note path in index".to_owned()))?;
            notes.push((commit_oid, e.oid, e.mode));
        } else {
            kept.push(e);
        }
    }
    index.entries = kept;
    let mut by_commit: std::collections::BTreeMap<ObjectId, (ObjectId, u32)> =
        std::collections::BTreeMap::new();
    for (commit_oid, blob_oid, mode) in notes {
        if let Some((existing_oid, existing_mode)) = by_commit.get_mut(&commit_oid) {
            if *existing_oid != blob_oid {
                let existing = odb.read(existing_oid)?;
                let incoming = odb.read(&blob_oid)?;
                let mut existing_len = existing.data.len();
                if existing_len > 0 && existing.data[existing_len - 1] == b'\n' {
                    existing_len -= 1;
                }
                let mut data = Vec::with_capacity(existing_len + 2 + incoming.data.len());
                data.extend_from_slice(&existing.data[..existing_len]);
                data.push(b'\n');
                data.push(b'\n');
                data.extend_from_slice(&incoming.data);
                *existing_oid = odb.write(ObjectKind::Blob, &data)?;
            }
            *existing_mode = mode;
        } else {
            by_commit.insert(commit_oid, (blob_oid, mode));
        }
    }
    for (commit_oid, (blob_oid, mode)) in by_commit {
        let path = construct_note_path_with_fanout(&commit_oid, fanout);
        index.add_or_replace(index_entry(path, mode, blob_oid));
    }
    Ok(())
}

fn normalize_mode(mode: u32) -> Result<u32> {
    match mode {
        0o100644 | 0o644 => Ok(MODE_REGULAR),
        0o100755 | 0o755 => Ok(crate::index::MODE_EXECUTABLE),
        0o120000 => Ok(crate::index::MODE_SYMLINK),
        0o160000 => Ok(MODE_GITLINK),
        0o040000 => Ok(MODE_TREE),
        _ => Err(Error::IndexError(format!(
            "fast-import: unsupported mode {mode:o}"
        ))),
    }
}

fn index_entry(path: Vec<u8>, mode: u32, oid: ObjectId) -> IndexEntry {
    let path_len = path.len().min(0xFFF) as u16;
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path_len,
        flags_extended: Some(0),
        path,
        base_index_pos: 0,
    }
}

fn tree_to_index(odb: &crate::odb::Odb, tree_oid: &ObjectId) -> Result<Index> {
    let obj = odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::IndexError(format!("expected tree at {tree_oid}")));
    }
    let entries = crate::objects::parse_tree(&obj.data)?;
    let mut index = Index::new();
    for te in entries {
        let path = te.name;
        if te.mode == MODE_TREE {
            let sub = tree_to_index(odb, &te.oid)?;
            for mut e in sub.entries {
                let mut full = path.clone();
                full.push(b'/');
                full.extend_from_slice(&e.path);
                e.path = full;
                let pl = e.path.len().min(0xFFF) as u16;
                e.flags = pl;
                index.add_or_replace(e);
            }
        } else {
            index.add_or_replace(index_entry(path, te.mode, te.oid));
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::resolve_ref;
    use crate::repo::init_repository;
    use std::io::Cursor;
    use tempfile::tempdir;

    #[test]
    fn fast_import_delimited_data_m_inline_and_note() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = init_repository(dir.path(), false, "main", None, "files")?;

        let setup = r#"commit refs/heads/main
committer T <t@e> 1000000000 +0000
data <<COMMIT
m1
COMMIT

M 644 inline f
data <<EOF
a
EOF

commit refs/heads/main
committer T <t@e> 1000000001 +0000
data <<COMMIT
m2
COMMIT

M 644 inline f
data <<EOF
b
EOF

"#;
        import_stream(&repo, Cursor::new(setup.as_bytes()))?;

        let c2 = resolve_ref(&repo.git_dir, "refs/heads/main")?;
        let c2_obj = repo.odb.read(&c2)?;
        let c2_parsed = parse_commit(&c2_obj.data)?;
        let c1 = c2_parsed
            .parents
            .first()
            .copied()
            .ok_or_else(|| Error::IndexError("test: expected parent commit".to_owned()))?;

        let notes = format!(
            r#"commit refs/notes/commits
committer T <t@e> 1000000002 +0000
data <<COMMIT
n1
COMMIT

N inline {c1}
data <<EOF
note1
EOF

N inline {c2}
data <<EOF
note2
EOF

commit refs/notes/commits
committer T <t@e> 1000000003 +0000
data <<COMMIT
n2
COMMIT

M 644 inline foobar/x.txt
data <<EOF
non-note
EOF

N inline {c2}
data <<EOF
edited
EOF

"#
        );
        import_stream(&repo, Cursor::new(notes.as_bytes()))?;

        let notes_tip = resolve_ref(&repo.git_dir, "refs/notes/commits")?;
        let commit_obj = repo.odb.read(&notes_tip)?;
        let parsed = parse_commit(&commit_obj.data)?;
        let tree = tree_to_index(&repo.odb, &parsed.tree)?;
        assert!(
            tree.entries.iter().any(|e| e.path == b"foobar/x.txt"),
            "expected non-note path preserved"
        );
        let mut found_edit = false;
        for e in &tree.entries {
            if is_note_index_path(&e.path) {
                let compact = compact_hex_from_note_path(&e.path).expect("note path");
                if compact == c2.to_hex() {
                    let blob = repo.odb.read(&e.oid)?;
                    assert_eq!(blob.data, b"edited\n");
                    found_edit = true;
                }
            }
        }
        assert!(found_edit, "expected edited note for second commit");
        Ok(())
    }
}
