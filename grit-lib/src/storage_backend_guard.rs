//! Source-scan guard for direct access to the files object store.
//!
//! A repository's objects are meant to be reached through its configured
//! object store. Code that builds its own [`crate::odb::Odb`] from a path,
//! joins `objects` onto a git dir, or reads pack files directly bypasses a
//! custom store. This guard walks the production module tree of `grit-lib`
//! (everything reachable from `lib.rs`, minus test-only modules and items) and
//! reports every such access that is not on one of two allow-lists:
//!
//! - `FILES_BY_DESIGN`: code that implements or maintains the on-disk files
//!   backend, or opens a directory that is files-only by nature (a peer
//!   repository on a local path, a submodule's git dir), so it must touch
//!   `objects/` directly. The areas are listed for embedders under
//!   "Filesystem-only by design" in `content/docs/library/object-stores.md`.
//! - `KNOWN_LEAKS`: places that should go through the repository's store but
//!   do not yet, each tagged with the plan step that removes it.
//!
//! Entries are `(file, pattern)` pairs. A pair whose hits include any leak is
//! a known leak, even when the same file also has by-design hits for that
//! pattern, so a leak never hides behind a by-design entry; the step that
//! fixes it moves the pair to `FILES_BY_DESIGN` if by-design hits remain.
//! An entry that no longer matches any access fails the guard, so both lists
//! can only shrink.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A direct-access shape the guard looks for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Access {
        /// `Odb::new(`: an object database opened from a bare path.
        OdbNew,
        /// `Odb::with_work_tree(`: same, with a work tree.
        OdbWithWorkTree,
        /// `.objects_dir()` outside `odb/`: reaching for the files directory.
        ObjectsDir,
        /// `join("objects")`: deriving the files directory from a git dir.
        JoinObjects,
        /// `SingleLocalPackReader::open*`: reading local packs directly.
        SingleLocalPackReader,
        /// `read_local_pack_indexes*`: listing local pack indexes directly.
        ReadLocalPackIndexes,
        /// `local_object_materialized*`: probing the files store for an object.
        LocalObjectMaterialized,
        /// `local_object_stored*`: probing the files store for an object.
        LocalObjectStored,
    }

    impl Access {
        const ALL: [Access; 8] = [
            Access::OdbNew,
            Access::OdbWithWorkTree,
            Access::ObjectsDir,
            Access::JoinObjects,
            Access::SingleLocalPackReader,
            Access::ReadLocalPackIndexes,
            Access::LocalObjectMaterialized,
            Access::LocalObjectStored,
        ];

        fn needle(self) -> &'static str {
            match self {
                Access::OdbNew => "Odb::new(",
                Access::OdbWithWorkTree => "Odb::with_work_tree(",
                Access::ObjectsDir => ".objects_dir()",
                Access::JoinObjects => "join(\"objects\")",
                Access::SingleLocalPackReader => "SingleLocalPackReader::open",
                Access::ReadLocalPackIndexes => "read_local_pack_indexes",
                Access::LocalObjectMaterialized => "local_object_materialized",
                Access::LocalObjectStored => "local_object_stored",
            }
        }

        /// Whether `rel` (a path under `src/`) is exempt from this access by rule.
        fn exempt(self, rel: &str) -> bool {
            self == Access::ObjectsDir && rel.starts_with("odb/")
        }
    }

    /// One reported access.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Hit {
        file: String,
        line: usize,
        access: Access,
    }

    /// Source text with comments blanked (`code`) and, additionally, string and
    /// char literal contents blanked (`shape`). Both keep every byte offset and
    /// newline of the original.
    struct Masked {
        code: Vec<u8>,
        shape: Vec<u8>,
    }

    fn is_ident(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn blank(buf: &mut [u8], range: std::ops::Range<usize>) {
        for b in &mut buf[range] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }

    fn mask(src: &str) -> Masked {
        let s = src.as_bytes();
        let mut code = s.to_vec();
        let mut shape = s.to_vec();
        let mut i = 0;
        while i < s.len() {
            let prev_ident = i > 0 && is_ident(s[i - 1]);
            match s[i] {
                b'/' if s.get(i + 1) == Some(&b'/') => {
                    let end = s[i..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(s.len(), |p| i + p);
                    blank(&mut code, i..end);
                    blank(&mut shape, i..end);
                    i = end;
                }
                b'/' if s.get(i + 1) == Some(&b'*') => {
                    let mut depth = 0usize;
                    let mut j = i;
                    while j < s.len() {
                        if s[j..].starts_with(b"/*") {
                            depth += 1;
                            j += 2;
                        } else if s[j..].starts_with(b"*/") {
                            depth -= 1;
                            j += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            j += 1;
                        }
                    }
                    blank(&mut code, i..j);
                    blank(&mut shape, i..j);
                    i = j;
                }
                b'r' | b'b' if !prev_ident && raw_string_start(s, i).is_some() => {
                    let (open_end, hashes) = raw_string_start(s, i).unwrap_or((i + 1, 0));
                    let mut close = Vec::with_capacity(hashes + 1);
                    close.push(b'"');
                    close.extend(std::iter::repeat_n(b'#', hashes));
                    let end = s[open_end..]
                        .windows(close.len())
                        .position(|w| w == close.as_slice())
                        .map_or(s.len(), |p| open_end + p);
                    blank(&mut shape, open_end..end);
                    i = (end + close.len()).min(s.len());
                }
                b'"' => {
                    let mut j = i + 1;
                    while j < s.len() && s[j] != b'"' {
                        j += if s[j] == b'\\' { 2 } else { 1 };
                    }
                    let end = j.min(s.len());
                    blank(&mut shape, i + 1..end);
                    i = end + 1;
                }
                b'\'' => {
                    let rest = &src[i + 1..];
                    let mut chars = rest.char_indices();
                    match chars.next() {
                        Some((_, '\\')) => {
                            let end = rest
                                .get(2..)
                                .and_then(|r| r.find('\''))
                                .map_or(s.len(), |p| i + 3 + p);
                            blank(&mut shape, i + 1..end);
                            i = end + 1;
                        }
                        Some((_, c)) if rest[c.len_utf8()..].starts_with('\'') => {
                            blank(&mut shape, i + 1..i + 1 + c.len_utf8());
                            i += c.len_utf8() + 2;
                        }
                        _ => i += 1,
                    }
                }
                _ => i += 1,
            }
        }
        Masked { code, shape }
    }

    /// If a raw string literal (`r"`, `r#"`, `br#"`, ...) starts at `i`, returns
    /// the offset just past its opening quote and its `#` count.
    fn raw_string_start(s: &[u8], i: usize) -> Option<(usize, usize)> {
        let mut j = i;
        if s.get(j) == Some(&b'b') {
            j += 1;
        }
        if s.get(j) != Some(&b'r') {
            return None;
        }
        j += 1;
        let hashes = s[j..].iter().take_while(|&&b| b == b'#').count();
        j += hashes;
        (s.get(j) == Some(&b'"')).then_some((j + 1, hashes))
    }

    /// Whether a `cfg(...)` predicate only holds in test or test-support builds.
    fn is_test_only_cfg(predicate: &str) -> bool {
        let p: String = predicate.chars().filter(|c| !c.is_whitespace()).collect();
        p == "test"
            || p.starts_with("all(test,")
            || p == "any(test,feature=\"test-support\")"
            || p == "feature=\"test-support\""
    }

    /// Finds the end of the bracketed group opened at `open` in `shape`.
    fn matching_close(shape: &[u8], open: usize) -> usize {
        let mut depth = 0usize;
        for (j, &b) in shape.iter().enumerate().skip(open) {
            match b {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return j;
                    }
                }
                _ => {}
            }
        }
        shape.len()
    }

    /// Byte ranges covered by test-only items, plus whether the whole file is
    /// test-only (`#![cfg(test)]`).
    fn test_only_ranges(src: &str, m: &Masked) -> (Vec<std::ops::Range<usize>>, bool) {
        let shape = &m.shape;
        let mut ranges = Vec::new();
        let mut i = 0;
        while let Some(p) = find(shape, i, b"#") {
            i = p + 1;
            let inner = shape.get(p + 1) == Some(&b'!');
            let bracket = if inner { p + 2 } else { p + 1 };
            if shape.get(bracket) != Some(&b'[') {
                continue;
            }
            let attr_end = matching_close(shape, bracket);
            let attr = src[bracket + 1..attr_end].trim();
            let Some(predicate) = attr
                .strip_prefix("cfg")
                .map(str::trim_start)
                .and_then(|a| a.strip_prefix('('))
                .and_then(|a| a.strip_suffix(')'))
            else {
                continue;
            };
            if !is_test_only_cfg(predicate) {
                continue;
            }
            if inner {
                return (Vec::new(), true);
            }
            let item_end = item_end(shape, attr_end + 1);
            ranges.push(p..item_end);
            i = item_end;
        }
        (ranges, false)
    }

    /// End of the item that starts after `from` (skipping further attributes):
    /// its `;`, its brace group, or the end of the enclosing group. A `,` ends a
    /// field, variant or match arm, but not an item whose generics may hold one.
    fn item_end(shape: &[u8], from: usize) -> usize {
        const KEYWORDS: &[&[u8]] = &[
            b"fn", b"impl", b"struct", b"enum", b"trait", b"type", b"use", b"static", b"const",
            b"let", b"mod",
        ];
        let mut keyword_seen = false;
        let mut j = from;
        while j < shape.len() {
            match shape[j] {
                b'#' if shape.get(j + 1) == Some(&b'[') => j = matching_close(shape, j + 1) + 1,
                b'(' | b'[' => j = matching_close(shape, j) + 1,
                b';' => return j + 1,
                b',' if !keyword_seen => return j + 1,
                b')' | b']' | b'}' => return j,
                b'{' => return matching_close(shape, j) + 1,
                b if is_ident(b) => {
                    let end = shape[j..]
                        .iter()
                        .position(|&b| !is_ident(b))
                        .map_or(shape.len(), |p| j + p);
                    keyword_seen |= KEYWORDS.contains(&&shape[j..end]);
                    j = end;
                }
                _ => j += 1,
            }
        }
        shape.len()
    }

    fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
        hay.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| from + p)
    }

    fn in_ranges(ranges: &[std::ops::Range<usize>], at: usize) -> bool {
        ranges.iter().any(|r| r.contains(&at))
    }

    /// Reports direct accesses in production code of one source file.
    fn scan_source(rel: &str, src: &str) -> Vec<Hit> {
        let m = mask(src);
        let (test_ranges, whole_file) = test_only_ranges(src, &m);
        if whole_file {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for access in Access::ALL {
            if access.exempt(rel) {
                continue;
            }
            let needle = access.needle().as_bytes();
            let mut from = 0;
            while let Some(at) = find(&m.code, from, needle) {
                from = at + 1;
                let starts_word = needle[0] == b'.' || at == 0 || !is_ident(m.code[at - 1]);
                if !starts_word || in_ranges(&test_ranges, at) {
                    continue;
                }
                let line = m.code[..at].iter().filter(|&&b| b == b'\n').count() + 1;
                hits.push(Hit {
                    file: rel.to_string(),
                    line,
                    access,
                });
            }
        }
        hits
    }

    /// Production `mod name;` declarations in `src`, with any `#[path]` value.
    fn module_decls(src: &str) -> Vec<(String, Option<String>)> {
        let m = mask(src);
        let (test_ranges, whole_file) = test_only_ranges(src, &m);
        if whole_file {
            return Vec::new();
        }
        let shape = &m.shape;
        let mut decls = Vec::new();
        let mut from = 0;
        while let Some(at) = find(shape, from, b"mod ") {
            from = at + 4;
            if (at > 0 && is_ident(shape[at - 1])) || in_ranges(&test_ranges, at) {
                continue;
            }
            let rest = &src[at + 4..];
            let name: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let after = rest.trim_start()[name.len()..].trim_start();
            if name.is_empty() || !after.starts_with(';') {
                continue;
            }
            decls.push((name, path_attr_before(src, at)));
        }
        decls
    }

    /// The `#[path = "..."]` value among the attributes directly above `at`.
    fn path_attr_before(src: &str, at: usize) -> Option<String> {
        let before = &src[..at];
        let start = before.rfind([';', '}', '{']).map_or(0, |p| p + 1);
        let attrs = &before[start..];
        let p = attrs.find("#[path")?;
        let q1 = attrs[p..].find('"')? + p + 1;
        let q2 = attrs[q1..].find('"')? + q1;
        Some(attrs[q1..q2].to_string())
    }

    /// All production source files reachable from `lib.rs`, relative to `src/`.
    fn production_files(src_dir: &Path) -> Vec<String> {
        let mut out = BTreeSet::new();
        let mut stack = vec![(src_dir.join("lib.rs"), src_dir.to_path_buf())];
        while let Some((file, dir)) = stack.pop() {
            let rel = file
                .strip_prefix(src_dir)
                .unwrap_or_else(|_| panic!("{} outside src", file.display()))
                .to_string_lossy()
                .replace('\\', "/");
            if !out.insert(rel) {
                continue;
            }
            let text = fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            let file_dir = file.parent().unwrap_or(src_dir).to_path_buf();
            for (name, path) in module_decls(&text) {
                let child = match path {
                    Some(p) => {
                        let child = file_dir.join(p);
                        let child_dir = child.parent().unwrap_or(&file_dir).to_path_buf();
                        (child, child_dir)
                    }
                    None => {
                        let flat = dir.join(format!("{name}.rs"));
                        let nested = dir.join(&name).join("mod.rs");
                        if flat.is_file() {
                            (flat, dir.join(&name))
                        } else if nested.is_file() {
                            (nested, dir.join(&name))
                        } else {
                            panic!("module `{name}` declared in {} not found", file.display())
                        }
                    }
                };
                stack.push(child);
            }
        }
        out.into_iter().collect()
    }

    fn scan_crate() -> Vec<Hit> {
        let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        for rel in production_files(&src_dir) {
            let text = fs::read_to_string(src_dir.join(&rel))
                .unwrap_or_else(|e| panic!("read {rel}: {e}"));
            hits.extend(scan_source(&rel, &text));
        }
        hits.sort();
        hits
    }

    /// Why a by-design entry needs the files layout.
    #[derive(Clone, Copy, Debug)]
    enum Area {
        /// The files backend itself: `Odb`'s files primary, loose/pack stores
        /// and the pack readers and writers they are built from.
        FilesBackend,
        /// Maintenance that rewrites the files layout: gc, repack, prune,
        /// offload, multi-pack-index and bitmap writing.
        Maintenance,
        /// Installing or spooling received packs under `objects/pack`,
        /// including the receive-pack quarantine.
        PackInstall,
        /// Opening, discovering and initializing an on-disk repository.
        RepositoryLayout,
        /// Submodule git dirs and alternates, which name directories on disk.
        SubmodulesAndAlternates,
        /// Peer repositories opened by path for local-path transport.
        LocalTransport,
        /// Fast paths taken only after checking that the primary is the files
        /// backend (or that a pack bitmap exists); other stores take the
        /// general path.
        GatedFastPath,
        /// Fallbacks for callers that have only a git directory path, such as the
        /// public git-dir free functions and a files ref store opened on its own:
        /// with no repository open, that directory's files database is the only
        /// store there is. Callers holding a repository pass its store instead.
        GitDirFallback,
        /// Throwaway, empty databases in a temporary directory that never hold
        /// the repository's objects.
        ScratchDatabase,
    }

    /// The step of the storage-backend plan that removes a known leak.
    #[derive(Clone, Copy, Debug)]
    enum FixStep {
        /// Step 4: the internal `PrimaryStore` enum in `Odb`, which gives fast
        /// paths and scratch stores a store handle instead of a directory.
        PrimaryStoreEnum,
        /// Step 5: change-id resolution and log grouping through the store.
        ChangeIdAndGroupBy,
    }

    struct ByDesign {
        file: &'static str,
        access: Access,
        area: Area,
    }

    struct Leak {
        file: &'static str,
        access: Access,
        step: FixStep,
    }

    const fn by_design(file: &'static str, access: Access, area: Area) -> ByDesign {
        ByDesign { file, access, area }
    }

    const fn leak(file: &'static str, access: Access, step: FixStep) -> Leak {
        Leak { file, access, step }
    }

    use Access::*;

    const FILES_BY_DESIGN: &[ByDesign] = &[
        by_design("odb/mod.rs", JoinObjects, Area::SubmodulesAndAlternates),
        by_design("odb/mod.rs", LocalObjectMaterialized, Area::FilesBackend),
        by_design("odb/mod.rs", LocalObjectStored, Area::FilesBackend),
        by_design("odb/mod.rs", ReadLocalPackIndexes, Area::FilesBackend),
        by_design("odb/mod.rs", SingleLocalPackReader, Area::FilesBackend),
        by_design(
            "odb/store/files.rs",
            LocalObjectMaterialized,
            Area::FilesBackend,
        ),
        by_design(
            "odb/store/files.rs",
            ReadLocalPackIndexes,
            Area::FilesBackend,
        ),
        by_design(
            "odb/store/mod.rs",
            LocalObjectMaterialized,
            Area::FilesBackend,
        ),
        by_design("odb/store/mod.rs", LocalObjectStored, Area::FilesBackend),
        by_design("odb/store/mod.rs", ReadLocalPackIndexes, Area::FilesBackend),
        by_design(
            "odb/store/packs.rs",
            ReadLocalPackIndexes,
            Area::FilesBackend,
        ),
        by_design("objects/mod.rs", ReadLocalPackIndexes, Area::FilesBackend),
        by_design("pack.rs", ObjectsDir, Area::FilesBackend),
        by_design("pack.rs", ReadLocalPackIndexes, Area::FilesBackend),
        by_design("pack.rs", SingleLocalPackReader, Area::FilesBackend),
        by_design("pack_delta.rs", ObjectsDir, Area::FilesBackend),
        by_design("pack_objects.rs", ObjectsDir, Area::FilesBackend),
        by_design("pack_reuse.rs", ObjectsDir, Area::FilesBackend),
        by_design("pack_reuse.rs", ReadLocalPackIndexes, Area::FilesBackend),
        by_design("pack_store.rs", JoinObjects, Area::FilesBackend),
        by_design("pack_store.rs", ObjectsDir, Area::FilesBackend),
        by_design("gc.rs", JoinObjects, Area::LocalTransport),
        by_design("gc.rs", OdbNew, Area::LocalTransport),
        by_design("maintenance.rs", ObjectsDir, Area::Maintenance),
        by_design("maintenance/drop_blobs.rs", ObjectsDir, Area::Maintenance),
        by_design("offload.rs", ObjectsDir, Area::Maintenance),
        by_design(
            "pack_bitmap/writer.rs",
            SingleLocalPackReader,
            Area::Maintenance,
        ),
        by_design("prune_packed.rs", OdbNew, Area::Maintenance),
        by_design("bundle/core.rs", ObjectsDir, Area::PackInstall),
        by_design("index_pack.rs", ObjectsDir, Area::PackInstall),
        by_design("receive_quarantine.rs", OdbNew, Area::PackInstall),
        by_design("serve/receive_pack.rs", ObjectsDir, Area::PackInstall),
        by_design("clone.rs", JoinObjects, Area::RepositoryLayout),
        by_design("repo.rs", JoinObjects, Area::RepositoryLayout),
        by_design("odb/builder.rs", OdbNew, Area::RepositoryLayout),
        by_design("diff/core.rs", JoinObjects, Area::SubmodulesAndAlternates),
        by_design("diff/core.rs", OdbNew, Area::SubmodulesAndAlternates),
        by_design(
            "submodule_gitdir.rs",
            JoinObjects,
            Area::SubmodulesAndAlternates,
        ),
        by_design("submodule_gitdir.rs", OdbNew, Area::SubmodulesAndAlternates),
        by_design("transfer.rs", JoinObjects, Area::LocalTransport),
        by_design("transfer.rs", ObjectsDir, Area::LocalTransport),
        by_design("transfer.rs", OdbNew, Area::LocalTransport),
        by_design("bitmap_walk.rs", ObjectsDir, Area::GatedFastPath),
        by_design("change_id.rs", SingleLocalPackReader, Area::GatedFastPath),
        by_design("fsck/mod.rs", SingleLocalPackReader, Area::GatedFastPath),
        by_design("merge_base.rs", ObjectsDir, Area::GatedFastPath),
        by_design("merge_base.rs", SingleLocalPackReader, Area::GatedFastPath),
        by_design("rev_list.rs", ObjectsDir, Area::GatedFastPath),
        by_design("rev_list.rs", SingleLocalPackReader, Area::GatedFastPath),
        // `open_checked_ref_store` serves only the git-dir ref writers.
        by_design("refs/store/checked.rs", JoinObjects, Area::GitDirFallback),
        by_design("refs/store/checked.rs", OdbNew, Area::GitDirFallback),
        // `pack_remote_tracking_refs_for_clone`; the other `join("objects")` hit reads
        // alternates.
        by_design("refs/mod.rs", JoinObjects, Area::GitDirFallback),
        by_design("refs/mod.rs", OdbNew, Area::GitDirFallback),
        // Peeling for initial `packed-refs` records when no repository installed a store.
        by_design("refs/store/files.rs", JoinObjects, Area::GitDirFallback),
        by_design("refs/store/files.rs", OdbNew, Area::GitDirFallback),
        by_design("promisor.rs", OdbNew, Area::GitDirFallback),
        // The thin-pack probe indexes a received pack against an empty database.
        by_design("unpack_objects.rs", JoinObjects, Area::ScratchDatabase),
        by_design("unpack_objects.rs", OdbNew, Area::ScratchDatabase),
    ];

    const KNOWN_LEAKS: &[Leak] = &[
        // Parallel diff and checkout workers fetch the primary's pack store by path.
        leak(
            "diff/merge_commit.rs",
            ObjectsDir,
            FixStep::PrimaryStoreEnum,
        ),
        leak("diff/patch_log.rs", ObjectsDir, FixStep::PrimaryStoreEnum),
        leak(
            "diff/tree_content.rs",
            ObjectsDir,
            FixStep::PrimaryStoreEnum,
        ),
        leak(
            "diff_rename_parallel.rs",
            ObjectsDir,
            FixStep::PrimaryStoreEnum,
        ),
        leak(
            "porcelain/checkout_parallel.rs",
            ObjectsDir,
            FixStep::PrimaryStoreEnum,
        ),
        // Abbreviation scans loose fanout directories without checking the primary.
        leak("oid_abbrev.rs", ObjectsDir, FixStep::PrimaryStoreEnum),
        // `reachable_commits` loads on-disk commit-graph files for any primary; the
        // pack spool hits in this file are files-only by design.
        leak("fetch.rs", ObjectsDir, FixStep::PrimaryStoreEnum),
        // A scratch database in a temp dir that could be an in-memory store.
        leak("diff/no_index.rs", OdbNew, FixStep::PrimaryStoreEnum),
        // `log --group-by` pack fast paths.
        leak(
            "revwalk/group_by.rs",
            ObjectsDir,
            FixStep::ChangeIdAndGroupBy,
        ),
        leak(
            "revwalk/group_by.rs",
            SingleLocalPackReader,
            FixStep::ChangeIdAndGroupBy,
        ),
    ];

    type Key = (&'static str, Access);

    fn allowed_keys() -> Vec<Key> {
        FILES_BY_DESIGN
            .iter()
            .map(|e| (e.file, e.access))
            .chain(KNOWN_LEAKS.iter().map(|e| (e.file, e.access)))
            .collect()
    }

    /// Hits that no allow-list entry covers.
    fn unallowed<'h>(hits: &'h [Hit], allowed: &[Key]) -> Vec<&'h Hit> {
        hits.iter()
            .filter(|h| !allowed.iter().any(|&(f, a)| f == h.file && a == h.access))
            .collect()
    }

    /// Entries that match no hit.
    fn stale(allowed: &[Key], hits: &[Hit]) -> Vec<Key> {
        allowed
            .iter()
            .copied()
            .filter(|&(f, a)| !hits.iter().any(|h| h.file == f && h.access == a))
            .collect()
    }

    fn render(hits: &[&Hit]) -> String {
        hits.iter()
            .map(|h| format!("  src/{}:{}: {}", h.file, h.line, h.access.needle()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn production_code_reaches_objects_only_where_allowed() {
        let hits = scan_crate();
        let bad = unallowed(&hits, &allowed_keys());
        assert!(
            bad.is_empty(),
            "direct access to the files object store outside the allow-lists; go through \
             the repository's object store, or add a FILES_BY_DESIGN / KNOWN_LEAKS entry \
             in src/storage_backend_guard.rs with its reason:\n{}",
            render(&bad)
        );
    }

    #[test]
    fn allow_lists_have_no_stale_entries() {
        let hits = scan_crate();
        let stale = stale(&allowed_keys(), &hits);
        let describe = |&(file, access): &Key| {
            let why = FILES_BY_DESIGN
                .iter()
                .find(|e| (e.file, e.access) == (file, access))
                .map(|e| format!("FILES_BY_DESIGN, {:?}", e.area))
                .or_else(|| {
                    KNOWN_LEAKS
                        .iter()
                        .find(|e| (e.file, e.access) == (file, access))
                        .map(|e| format!("KNOWN_LEAKS, fixed by {:?}", e.step))
                })
                .unwrap_or_default();
            format!("  src/{file}: {} ({why})", access.needle())
        };
        assert!(
            stale.is_empty(),
            "allow-list entries that no longer match any access; remove them:\n{}",
            stale.iter().map(describe).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn allow_lists_do_not_overlap() {
        let mut seen = BTreeSet::new();
        for key in allowed_keys() {
            assert!(seen.insert(key), "duplicate allow-list entry {key:?}");
        }
    }

    #[test]
    fn new_odb_in_an_unlisted_module_is_reported() {
        let src = "pub fn peel(git_dir: &Path) {\n    let odb = Odb::new(&git_dir.join(\"objects\"));\n}\n";
        let hits = scan_source("refs/peel_helper.rs", src);
        let bad = unallowed(&hits, &allowed_keys());
        let found: Vec<_> = bad.iter().map(|h| (h.line, h.access)).collect();
        assert_eq!(found, vec![(2, OdbNew), (2, JoinObjects)]);
    }

    #[test]
    fn listed_file_with_another_pattern_is_reported() {
        let src =
            "fn f(o: &Odb, id: &ObjectId) { let _ = local_object_materialized(o.dir(), id); }\n";
        let hits = scan_source("gc.rs", src);
        assert_eq!(unallowed(&hits, &allowed_keys()).len(), 1);
    }

    #[test]
    fn stale_entry_is_reported() {
        let hits = scan_source("a.rs", "fn f() { Odb::new(p); }\n");
        let allowed = [("a.rs", OdbNew), ("a.rs", JoinObjects), ("b.rs", OdbNew)];
        assert_eq!(
            stale(&allowed, &hits),
            vec![("a.rs", JoinObjects), ("b.rs", OdbNew)]
        );
    }

    #[test]
    fn test_only_code_comments_and_lookalikes_are_ignored() {
        let src = r##"
// Odb::new( in a line comment
/* block: join("objects") /* nested */ SingleLocalPackReader::open */
/// docs mention `Odb::with_work_tree(` too
fn f() {
    let brace = '{';
    let raw = r#"}}}"#;
    let s = "}";
    let x: &'static str = "";
    PackOdb::new(p);
    my_local_object_materialized(p);
}
#[cfg(test)]
mod tests {
    fn t() { Odb::new(p); let c = '}'; }
}
#[cfg(all(test, unix))]
fn helper() { Odb::new(p); }
#[cfg(any(test, feature = "test-support"))]
use crate::pack::read_local_pack_indexes;
#[cfg(test)]
fn generic<A, B>(a: A, b: B) { Odb::new(p); }
fn g() { let q = '\''; let r = "\"}"; local_object_stored(p, o); }
"##;
        let hits = scan_source("x.rs", src);
        let found: Vec<_> = hits.iter().map(|h| (h.line, h.access)).collect();
        assert_eq!(found, vec![(23, LocalObjectStored)]);
    }

    #[test]
    fn cfg_not_test_and_platform_cfgs_are_production() {
        let src = "#[cfg(not(test))]\nfn a() { Odb::new(p); }\n#[cfg(any(windows, test))]\nfn b() { Odb::new(p); }\n";
        let lines: Vec<_> = scan_source("x.rs", src).iter().map(|h| h.line).collect();
        assert_eq!(lines, vec![2, 4]);
    }

    #[test]
    fn objects_dir_is_exempt_only_inside_odb() {
        let src = "fn f(o: &Odb) { o.objects_dir(); }\n";
        assert!(scan_source("odb/commit.rs", src).is_empty());
        assert_eq!(scan_source("diff/core.rs", src).len(), 1);
    }

    #[test]
    fn module_walk_follows_path_attributes_and_skips_test_modules() {
        let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = production_files(&src_dir);
        let has = |f: &str| files.iter().any(|x| x == f);
        assert!(has("odb/mod.rs"));
        assert!(has("odb/store/files.rs"));
        assert!(has("merge_tree_cache.rs"), "#[path] module not followed");
        assert!(!has("storage_backend_guard.rs"));
        assert!(!has("rustdoc_placeholder_guard.rs"));
        assert!(!has("test_support/mod.rs"));
    }
}
