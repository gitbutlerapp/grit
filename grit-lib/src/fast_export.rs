//! [`git fast-export`](https://git-scm.com/docs/git-fast-export) stream generation.
//!
//! Supports the subset needed by upstream tests: `--all`, `--anonymize`,
//! `--anonymize-map`, topological commit order with `reverse` (oldest first),
//! blob/commit marks, per-commit tree diffs, and annotated tags on commits.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::diff::{diff_trees, DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, CommitData, ObjectId, ObjectKind};
use crate::pathspec::matches_pathspec_list;
use crate::refs;
use crate::repo::Repository;
use crate::rev_list::{rev_list, OrderingMode, RevListOptions};

use crate::index::{MODE_GITLINK, MODE_TREE};

/// Options for [`export_stream`].
#[derive(Debug, Clone, Default)]
pub struct FastExportOptions {
    /// Export all heads under `refs/heads/` (and reachable history).
    pub all: bool,
    /// Replace paths, idents, messages, and non-mark OIDs with stable placeholders.
    pub anonymize: bool,
    /// `from:to` or bare `token` mappings (last duplicate key wins, matching Git).
    pub anonymize_maps: Vec<String>,
    /// Emit `feature done` / trailing `done` (matches `git fast-import` when the feature is negotiated).
    pub use_done_feature: bool,
    /// Omit `blob` commands and emit `M` lines with full object ids (matches `git fast-export --no-data`).
    pub no_data: bool,
    /// Positive revision arguments to export when `all` is false.
    pub revisions: Vec<String>,
    /// Pathspecs limiting exported commits and file commands.
    pub paths: Vec<String>,
}

struct AnonState<'a> {
    seeds: &'a HashMap<String, String>,
    paths: HashMap<String, String>,
    refs: HashMap<String, String>,
    objs: HashMap<String, String>,
    idents: HashMap<String, String>,
    tag_msgs: HashMap<String, String>,
    path_n: u32,
    ref_n: u32,
    oid_n: u32,
    ident_n: u32,
    subject_n: u32,
    tag_msg_n: u32,
    blob_n: u32,
}

impl<'a> AnonState<'a> {
    fn new(seeds: &'a HashMap<String, String>) -> Self {
        Self {
            seeds,
            paths: HashMap::new(),
            refs: HashMap::new(),
            objs: HashMap::new(),
            idents: HashMap::new(),
            tag_msgs: HashMap::new(),
            path_n: 0,
            ref_n: 0,
            oid_n: 0,
            ident_n: 0,
            subject_n: 0,
            tag_msg_n: 0,
            blob_n: 0,
        }
    }

    fn map_token(
        map: &mut HashMap<String, String>,
        seeds: &HashMap<String, String>,
        key: &str,
        gen: impl FnOnce() -> String,
    ) -> String {
        if let Some(v) = seeds.get(key) {
            return v.clone();
        }
        if let Some(v) = map.get(key) {
            return v.clone();
        }
        let v = gen();
        map.insert(key.to_string(), v.clone());
        v
    }

    fn path_seed_lookup(comp: &str, seeds: &HashMap<String, String>) -> Option<String> {
        if let Some(v) = seeds.get(comp) {
            return Some(v.clone());
        }
        if let Some(dot) = comp.find('.') {
            let stem = &comp[..dot];
            if let Some(v) = seeds.get(stem) {
                let ext = &comp[dot..];
                return Some(format!("{v}{ext}"));
            }
        }
        None
    }

    fn anonymize_path_component(&mut self, comp: &str) -> String {
        if let Some(mapped) = Self::path_seed_lookup(comp, self.seeds) {
            return Self::map_token(&mut self.paths, &HashMap::new(), comp, || mapped);
        }
        Self::map_token(&mut self.paths, self.seeds, comp, || {
            let n = self.path_n;
            self.path_n += 1;
            format!("path{n}")
        })
    }

    fn anonymize_path(&mut self, path: &str) -> String {
        if !path.is_empty() && self.seeds.contains_key(path) {
            return self.seeds[path].clone();
        }
        let mut out = String::new();
        for (i, part) in path.split('/').enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(&self.anonymize_path_component(part));
        }
        out
    }

    fn anonymize_refname(&mut self, refname: &str) -> String {
        const PREFIXES: &[&str] = &["refs/heads/", "refs/tags/", "refs/remotes/", "refs/"];
        let mut rest = refname;
        let mut prefix = "";
        for p in PREFIXES {
            if let Some(stripped) = refname.strip_prefix(p) {
                prefix = p;
                rest = stripped;
                break;
            }
        }
        let mut out = prefix.to_string();
        if rest.is_empty() {
            return out;
        }
        for (i, comp) in rest.split('/').enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(&Self::map_token(&mut self.refs, self.seeds, comp, || {
                let n = self.ref_n;
                self.ref_n += 1;
                format!("ref{n}")
            }));
        }
        out
    }

    fn anonymize_oid_hex(&mut self, hex: &str) -> String {
        Self::map_token(&mut self.objs, self.seeds, hex, || {
            self.oid_n += 1;
            format!("{:040x}", self.oid_n as u128)
        })
    }

    fn anonymize_ident_line(&mut self, line: &str) -> String {
        // "author NAME <EMAIL> DATE TZ" — preserve header word and date tail.
        let Some(space) = line.find(' ') else {
            return line.to_owned();
        };
        let header = &line[..space + 1];
        let rest = line[space + 1..].trim_end();
        let Some(gt) = rest.rfind('>') else {
            return format!("{header}Malformed Ident <malformed@example.com> 0 -0000");
        };
        let name_email = &rest[..gt + 1];
        let after = rest[gt + 1..].trim_start();
        let key = name_email.to_string();
        let ident = Self::map_token(&mut self.idents, self.seeds, &key, || {
            let n = self.ident_n;
            self.ident_n += 1;
            format!("User {n} <user{n}@example.com>")
        });
        format!("{header}{ident} {after}")
    }

    fn anonymize_commit_message(&mut self) -> String {
        let n = self.subject_n;
        self.subject_n += 1;
        format!("subject {n}\n\nbody\n")
    }

    fn anonymize_tag_message(&mut self, msg: &str) -> String {
        Self::map_token(&mut self.tag_msgs, self.seeds, msg, || {
            let n = self.tag_msg_n;
            self.tag_msg_n += 1;
            format!("tag message {n}")
        })
    }

    fn anonymize_blob_payload(&mut self) -> Vec<u8> {
        let n = self.blob_n;
        self.blob_n += 1;
        format!("anonymous blob {n}").into_bytes()
    }
}

fn parse_anonymize_maps(entries: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for raw in entries {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(Error::InvalidRef(
                "--anonymize-map token cannot be empty".to_owned(),
            ));
        }
        if let Some((k, v)) = raw.split_once(':') {
            if k.is_empty() || v.is_empty() {
                return Err(Error::InvalidRef(
                    "--anonymize-map token cannot be empty".to_owned(),
                ));
            }
            out.insert(k.to_string(), v.to_string());
        } else {
            out.insert(raw.to_string(), raw.to_string());
        }
    }
    Ok(out)
}

/// Ref tips used to assign each exported commit a `commit <ref>` line (Git `revision_sources`).
///
/// Includes `refs/heads/*` and peeled `refs/tags/*` so tagged-only commits (e.g. `git tag E` with no
/// branch) still get a valid source ref. Without tags, `fast-export --all` can fail with
/// `no ref source for commit` when the walk reaches a commit reachable only via tags.
fn revision_source_tips(repo: &Repository) -> Result<Vec<(String, ObjectId)>> {
    let mut tips = refs::list_refs(&repo.git_dir, "refs/heads/")?;
    for (name, oid) in refs::list_refs(&repo.git_dir, "refs/tags/")? {
        let tip = match peel_tag_to_commit_oid(repo, oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        tips.push((name, tip));
    }
    Ok(tips)
}

fn ref_source_for_commit(
    repo: &Repository,
    oid: ObjectId,
    head_branches: &[(String, ObjectId)],
) -> Result<String> {
    let mut best: Option<(&str, (u8, usize))> = None;
    for (name, tip) in head_branches {
        if *tip != oid {
            continue;
        }
        let score = (
            if name.starts_with("refs/heads/") {
                0
            } else {
                1
            },
            name.len(),
        );
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((name.as_str(), score));
        }
    }
    if let Some((n, _)) = best {
        return Ok(n.to_string());
    }
    // Propagate first-seen ref name along parents (matches Git `revision_sources`).
    let mut source: HashMap<ObjectId, String> = HashMap::new();
    let mut queue: std::collections::VecDeque<ObjectId> = std::collections::VecDeque::new();
    for (name, tip) in head_branches {
        if source.insert(*tip, name.clone()).is_none() {
            queue.push_back(*tip);
        }
    }
    while let Some(c) = queue.pop_front() {
        let pname = source.get(&c).cloned().unwrap_or_default();
        let commit = load_commit(repo, c)?;
        for p in commit.parents {
            if source.contains_key(&p) {
                continue;
            }
            source.insert(p, pname.clone());
            queue.push_back(p);
        }
    }
    source
        .get(&oid)
        .cloned()
        .ok_or_else(|| Error::InvalidRef(format!("no ref source for commit {oid}")))
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData> {
    let obj = repo.odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    parse_commit(&obj.data)
}

fn peel_tag_to_commit_oid(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = repo.odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => {
                let t = parse_tag(&obj.data)?;
                oid = t.object;
            }
            _ => {
                return Err(Error::CorruptObject(
                    "tag does not point to a commit".to_owned(),
                ));
            }
        }
    }
}

fn depth_first_diff_sort(entries: &mut [DiffEntry]) {
    entries.sort_by(|a, b| {
        let pa = a.path();
        let pb = b.path();
        let la = pa.len();
        let lb = pb.len();
        let minlen = la.min(lb);
        let cmp = pa.as_bytes()[..minlen].cmp(&pb.as_bytes()[..minlen]);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        let len_cmp = lb.cmp(&la);
        if len_cmp != std::cmp::Ordering::Equal {
            return len_cmp;
        }
        let ar = matches!(a.status, DiffStatus::Renamed);
        let br = matches!(b.status, DiffStatus::Renamed);
        ar.cmp(&br)
    });
}

fn diff_entry_matches_paths(entry: &DiffEntry, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    matches_pathspec_list(&entry.path().to_string_lossy(), paths)
        || entry
            .old_path
            .as_deref()
            .is_some_and(|path| matches_pathspec_list(&path.to_string_lossy(), paths))
}

fn export_ref_for_non_all(repo: &Repository) -> Result<String> {
    refs::read_head(&repo.git_dir)?.ok_or_else(|| {
        Error::InvalidRef("fast-export: detached HEAD export not implemented".to_owned())
    })
}

/// Write a fast-import stream for the repository to `writer`.
///
/// # Errors
///
/// Propagates object database, ref, and revision walk errors.
pub fn export_stream(
    repo: &Repository,
    mut writer: impl Write,
    options: &FastExportOptions,
) -> Result<()> {
    let seeds = if options.anonymize {
        parse_anonymize_maps(&options.anonymize_maps)?
    } else {
        HashMap::new()
    };

    if !options.anonymize && !options.anonymize_maps.is_empty() {
        return Err(Error::InvalidRef(
            "the option '--anonymize-map' requires '--anonymize'".to_owned(),
        ));
    }

    let head_branches = revision_source_tips(repo)?;
    let non_all_export_ref = if options.all {
        None
    } else {
        Some(export_ref_for_non_all(repo)?)
    };

    let opts = RevListOptions {
        all_refs: options.all,
        ordering: OrderingMode::Topo,
        reverse: true,
        paths: options.paths.clone(),
        ..RevListOptions::default()
    };
    let positive_specs = if options.all {
        &[] as &[String]
    } else {
        options.revisions.as_slice()
    };
    let rev_result = rev_list(repo, positive_specs, &[] as &[String], &opts)?;
    let commits: Vec<ObjectId> = rev_result.commits;

    let commit_set: HashSet<ObjectId> = commits.iter().copied().collect();

    let mut marks: HashMap<ObjectId, u32> = HashMap::new();
    let mut next_mark: u32 = 0;

    let mut anon = if options.anonymize {
        Some(AnonState::new(&seeds))
    } else {
        None
    };

    if options.use_done_feature {
        writeln!(writer, "feature done")?;
    }

    for oid in &commits {
        let raw_commit = load_commit(repo, *oid)?;
        let parent_tree = if let Some(p) = raw_commit.parents.first() {
            let pc = load_commit(repo, *p)?;
            Some(pc.tree)
        } else {
            None
        };
        let diffs = diff_trees(&repo.odb, parent_tree.as_ref(), Some(&raw_commit.tree), "")?;
        let mut diff_vec: Vec<DiffEntry> = diffs
            .into_iter()
            .filter(|e| {
                matches!(
                    e.status,
                    DiffStatus::Added
                        | DiffStatus::Deleted
                        | DiffStatus::Modified
                        | DiffStatus::Renamed
                        | DiffStatus::Copied
                        | DiffStatus::TypeChanged
                ) && diff_entry_matches_paths(e, &options.paths)
            })
            .collect();
        depth_first_diff_sort(&mut diff_vec);

        if !options.no_data {
            for e in &diff_vec {
                if e.status == DiffStatus::Deleted {
                    continue;
                }
                let mode = u32::from_str_radix(e.new_mode.trim(), 8).unwrap_or(0);
                if mode == MODE_TREE || mode == MODE_GITLINK {
                    continue;
                }
                let blob_oid = e.new_oid;
                if marks.contains_key(&blob_oid) {
                    continue;
                }
                next_mark += 1;
                marks.insert(blob_oid, next_mark);
                writeln!(writer, "blob")?;
                writeln!(writer, "mark :{next_mark}")?;
                let payload = if let Some(a) = anon.as_mut() {
                    a.anonymize_blob_payload()
                } else {
                    let o = repo.odb.read(&blob_oid)?;
                    if o.kind != ObjectKind::Blob {
                        return Err(Error::CorruptObject("expected blob".to_owned()));
                    }
                    o.data
                };
                writeln!(writer, "data {}", payload.len())?;
                writer.write_all(&payload)?;
                writeln!(writer)?;
            }
        }

        let refname = if let Some(export_ref) = non_all_export_ref.as_deref() {
            export_ref.to_owned()
        } else {
            ref_source_for_commit(repo, *oid, &head_branches)?
        };
        let export_ref = if let Some(a) = anon.as_mut() {
            a.anonymize_refname(&refname)
        } else {
            refname.clone()
        };

        if raw_commit.parents.is_empty() {
            writeln!(writer, "reset {export_ref}")?;
        }

        next_mark += 1;
        let commit_mark = next_mark;
        marks.insert(*oid, commit_mark);

        writeln!(writer, "commit {export_ref}")?;
        writeln!(writer, "mark :{commit_mark}")?;

        let author_line = if let Some(a) = anon.as_mut() {
            a.anonymize_ident_line(&format!("author {}", raw_commit.author))
        } else {
            format!("author {}", raw_commit.author)
        };
        let committer_line = if let Some(a) = anon.as_mut() {
            a.anonymize_ident_line(&format!("committer {}", raw_commit.committer))
        } else {
            format!("committer {}", raw_commit.committer)
        };
        writeln!(writer, "{author_line}")?;
        writeln!(writer, "{committer_line}")?;

        let message = if let Some(a) = anon.as_mut() {
            a.anonymize_commit_message()
        } else {
            raw_commit.message.clone()
        };
        let msg_bytes = message.as_bytes();
        writeln!(writer, "data {}", msg_bytes.len())?;
        writer.write_all(msg_bytes)?;
        writeln!(writer)?;

        let exported_parents = raw_commit
            .parents
            .iter()
            .filter_map(|p| marks.get(p).copied())
            .collect::<Vec<_>>();
        for (i, m) in exported_parents.iter().enumerate() {
            let label = if i == 0 { "from" } else { "merge" };
            write!(writer, "{label} ")?;
            writeln!(writer, ":{m}")?;
        }
        if !options.paths.is_empty() && exported_parents.is_empty() {
            writeln!(writer, "deleteall")?;
        }

        let mut changed: HashSet<String> = HashSet::new();
        for e in &diff_vec {
            match e.status {
                DiffStatus::Deleted => {
                    let path = if let Some(a) = anon.as_mut() {
                        a.anonymize_path(&e.path().to_string_lossy())
                    } else {
                        e.path().to_string_lossy().into_owned()
                    };
                    writeln!(writer, "D {path}")?;
                    changed.insert(e.path().to_string_lossy().into_owned());
                }
                DiffStatus::Renamed | DiffStatus::Copied => {
                    let old_p = e
                        .old_path
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let skip_modify = e.old_oid == e.new_oid
                        && e.old_mode == e.new_mode
                        && !changed.contains(&old_p);
                    if !changed.contains(&old_p) {
                        let op = if let Some(a) = anon.as_mut() {
                            a.anonymize_path(&old_p)
                        } else {
                            old_p.clone()
                        };
                        let np = if let Some(a) = anon.as_mut() {
                            a.anonymize_path(&e.path().to_string_lossy())
                        } else {
                            e.path().to_string_lossy().into_owned()
                        };
                        writeln!(writer, "{} {op} {np}", e.status.letter())?;
                    }
                    if !skip_modify {
                        fallthrough_modify(
                            repo,
                            &mut writer,
                            e,
                            &marks,
                            anon.as_mut(),
                            options.anonymize,
                            options.no_data,
                        )?;
                    }
                    changed.insert(old_p);
                    changed.insert(e.path().to_string_lossy().into_owned());
                }
                DiffStatus::Added | DiffStatus::Modified | DiffStatus::TypeChanged => {
                    fallthrough_modify(
                        repo,
                        &mut writer,
                        e,
                        &marks,
                        anon.as_mut(),
                        options.anonymize,
                        options.no_data,
                    )?;
                    changed.insert(e.path().to_string_lossy().into_owned());
                }
                _ => {}
            }
        }
        writeln!(writer)?;
    }

    // Annotated tags that point at exported commits
    let tag_refs = refs::list_refs(&repo.git_dir, "refs/tags/")?;
    for (full_name, tag_oid) in tag_refs {
        let tag_obj = repo.odb.read(&tag_oid)?;
        if tag_obj.kind != ObjectKind::Tag {
            continue;
        }
        let tag_data = parse_tag(&tag_obj.data)?;
        let Ok(target_commit) = peel_tag_to_commit_oid(repo, tag_data.object) else {
            continue;
        };
        if !commit_set.contains(&target_commit) {
            continue;
        }
        let Some(&tip_mark) = marks.get(&target_commit) else {
            continue;
        };

        let export_name = if let Some(a) = anon.as_mut() {
            a.anonymize_refname(&full_name)
        } else {
            full_name.clone()
        };
        let short_name = export_name
            .strip_prefix("refs/tags/")
            .unwrap_or(&export_name)
            .to_string();

        let tagger_line = if let Some(t) = tag_data.tagger.as_deref() {
            if let Some(a) = anon.as_mut() {
                a.anonymize_ident_line(&format!("tagger {t}"))
            } else {
                format!("tagger {t}")
            }
        } else {
            String::new()
        };

        let msg = if options.anonymize {
            anon.as_mut()
                .map(|a| a.anonymize_tag_message(&tag_data.message))
                .unwrap_or_default()
        } else {
            tag_data.message.clone()
        };

        writeln!(writer, "tag {short_name}")?;
        writeln!(writer, "from :{tip_mark}")?;
        if !tagger_line.is_empty() {
            writeln!(writer, "{tagger_line}")?;
        }
        let msg_bytes = msg.as_bytes();
        writeln!(writer, "data {}", msg_bytes.len())?;
        writer.write_all(msg_bytes)?;
        writeln!(writer)?;
    }

    if options.use_done_feature {
        writeln!(writer, "done")?;
    }

    Ok(())
}

fn fallthrough_modify(
    _repo: &Repository,
    writer: &mut impl Write,
    e: &DiffEntry,
    marks: &HashMap<ObjectId, u32>,
    mut anon: Option<&mut AnonState>,
    _anonymize: bool,
    no_data: bool,
) -> Result<()> {
    let mode = u32::from_str_radix(e.new_mode.trim(), 8).unwrap_or(0);
    let path = if let Some(a) = anon.as_mut() {
        a.anonymize_path(&e.path().to_string_lossy())
    } else {
        e.path().to_string_lossy().into_owned()
    };
    if mode == MODE_GITLINK {
        let hex = e.new_oid.to_hex();
        let oid_out = if let Some(a) = anon {
            a.anonymize_oid_hex(&hex)
        } else {
            hex
        };
        writeln!(writer, "M {:06o} {oid_out} {path}", mode)?;
        return Ok(());
    }
    if no_data {
        let hex = e.new_oid.to_hex();
        let oid_out = if let Some(a) = anon.as_mut() {
            a.anonymize_oid_hex(&hex)
        } else {
            hex
        };
        writeln!(writer, "M {:06o} {oid_out} {path}", mode)?;
        return Ok(());
    }
    let Some(&bm) = marks.get(&e.new_oid) else {
        return Err(Error::IndexError(format!(
            "fast-export: missing mark for blob {}",
            e.new_oid
        )));
    };
    writeln!(writer, "M {:06o} :{bm} {path}", mode)?;
    Ok(())
}
