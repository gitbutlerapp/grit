---
title: Objects
summary: ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.
---

Git stores four object kinds grit-lib exposes as [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Every object is named by an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId) (SHA-1 by default). Prefer [`Repository::objects`](rustdoc:grit_lib::repo::Repository) ([`Objects`](rustdoc:grit_lib::objects::handle::Objects)) for read/write/contains; it delegates to the pluggable [`Odb`](rustdoc:grit_lib::objects::odb::Odb) on the same [`Repository`](rustdoc:grit_lib::repo::Repository).

## Writing

[`Objects::write`](rustdoc:grit_lib::objects::handle::Objects::write) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree, commit and tag payloads are checked first with the same rules as `git hash-object -t <kind>`: an unsorted tree, a duplicate entry, or an identity line without a timestamp fails with `InvalidObject` on [`Error`](rustdoc:grit_lib::error::Error), which carries Git's fsck message id (`treeNotSorted`, `missingSpaceBeforeDate`, …), and nothing is written. [`Objects::write_literally`](rustdoc:grit_lib::objects::handle::Objects::write_literally) skips the checks, like `git hash-object --literally`; it exists to reproduce corrupt objects in tests. [`Odb::write`](rustdoc:grit_lib::objects::odb::Odb::write) is the unchecked internal path the engine uses for objects it has built itself. Use [`parse_tree`](rustdoc:grit_lib::objects::parse_tree) and [`parse_commit`](rustdoc:grit_lib::objects::parse_commit) when reading objects back.

## Big objects

[`Odb::write_stream`](rustdoc:grit_lib::objects::odb::Odb::write_stream) stores an object from any [`Read`](https://doc.rust-lang.org/std/io/trait.Read.html) of known size without holding it in memory. Objects of at least [`Odb::big_file_threshold`](rustdoc:grit_lib::objects::odb::Odb::big_file_threshold) bytes (`core.bigFileThreshold`, 512 MiB unless configured) are hashed and deflated in fixed-size chunks straight into a new single-object pack at the pack compression level (`pack.compression` / `core.compression`), which is what Git does with big files; smaller objects are written loose as usual. If the object already exists, the new pack is discarded. [`Odb::hash_stream`](rustdoc:grit_lib::objects::odb::Odb::hash_stream) computes the id the same way without storing anything. A reader that ends early fails with [`Error::ObjectStreamTruncated`](rustdoc:grit_lib::error::Error::ObjectStreamTruncated).

Staging uses the same path: `add`, `commit` auto-staging and `status` stream every worktree file at or above the threshold whose content needs no clean conversion (no clean filter, `working-tree-encoding` or line-ending conversion), so their memory use does not grow with the file size. Files that need a conversion are read whole, as in Git.

Staging never stores content twice. Like Git, it freshens an existing copy instead of writing a new one: a blob already in a local pack (promisor packs included) or an alternate object directory gets its pack's mtime refreshed, and no loose object is written, so re-adding packed content (for example after `rm --cached -r .`) adds nothing to `objects/`. Only an object stored solely in a cruft pack, which cannot be freshened, is written loose again. Bulk writers get the same behavior from [`Odb::bulk_stage_write_options`](rustdoc:grit_lib::objects::odb::Odb::bulk_stage_write_options), which sets [`WriteOptions::freshen_existing`](rustdoc:grit_lib::objects::odb::WriteOptions::freshen_existing) only when the repository has a pack or an alternate, so writing into a repository without either skips the probe entirely.

```rust
use std::fs::File;
use grit_lib::objects::ObjectKind;
use grit_lib::objects::odb::WriteOptions;

let mut file = File::open("dataset.bin")?;
let size = file.metadata()?.len();
let oid = repo.odb().write_stream(ObjectKind::Blob, size, &mut file, WriteOptions::default())?;
```

## Building trees and commits

A program that creates history without an index or work tree (a bare-repository tool, a changelog bot) should not assemble object bytes by hand. [`Objects::write_tree`](rustdoc:grit_lib::objects::handle::Objects::write_tree) takes [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry) values in any order, sorts them into Git's tree order, and refuses entries `git fsck` would reject with a typed [`TreeEntryError`](rustdoc:grit_lib::objects::TreeEntryError): an empty, `.`, `..` or `.git` name, a `/` or NUL in a name, a mode other than `100644`, `100755`, `120000`, `40000` or `160000`, the null id, an id of the other hash algorithm, a name over 4096 bytes, a name used twice (a file and a directory with the same name count), or a symlink to a reserved metadata file name (`.gitmodules`, `.gitattributes`, and related spellings). Each entry's object must exist in the repository with the kind implied by its mode (tree or blob; gitlink `160000` entries may name a commit that is not stored locally), matching `git mktree` without `--missing`; missing or wrong-kind oids return [`Error::ObjectNotFound`](rustdoc:grit_lib::error::Error::ObjectNotFound) or [`Error::UnexpectedObjectKind`](rustdoc:grit_lib::error::Error::UnexpectedObjectKind). Pass [`WriteTreeOptions::allow_missing`](rustdoc:grit_lib::objects::WriteTreeOptions::allow_missing) to [`Objects::write_tree_with`](rustdoc:grit_lib::objects::handle::Objects::write_tree_with) when building trees bottom-up or referencing objects a partial clone has not fetched (wrong kinds are still rejected).

[`Objects::write_commit`](rustdoc:grit_lib::objects::handle::Objects::write_commit) takes a [`NewCommit`](rustdoc:grit_lib::objects::NewCommit): a tree, parents, message, and author and committer as typed [`Signature`](rustdoc:grit_lib::ident::Signature) values. [`Signature::new`](rustdoc:grit_lib::ident::Signature::new) takes a name, an email and an `OffsetDateTime` and rejects `<`, `>` and control characters, times before 1970 and offsets with a seconds part ([`SignatureError`](rustdoc:grit_lib::ident::SignatureError)), so the header line always has Git's `Name <email> <unix> <±HHMM>` shape. `write_commit` checks that the tree is a tree and each parent is a commit, as `git commit-tree` does. It adds a trailing newline to the message when missing. When `grit.changeIds` is on (the default) the commit gets a fresh `change-id` header; call [`NewCommit::with_change_id`](rustdoc:grit_lib::objects::NewCommit::with_change_id) to keep the id of a change you are rewriting. The commit is not signed and no ref moves. To commit the index onto the current branch, use `create_commit` in [`porcelain::commit`](rustdoc:grit_lib::porcelain::commit) instead.

For hot paths that only need headers (tree, parents, identities) or a borrowed message body, prefer [`CommitRef::parse`](rustdoc:grit_lib::objects::CommitRef::parse). It borrows from the object bytes and decodes author, committer, and message on demand via [`CommitRef::author`](rustdoc:grit_lib::objects::CommitRef::author), [`CommitRef::committer`](rustdoc:grit_lib::objects::CommitRef::committer), and [`CommitRef::message`](rustdoc:grit_lib::objects::CommitRef::message). [`parse_commit`](rustdoc:grit_lib::objects::parse_commit) is `CommitRef::parse(data)?.to_owned()` for callers that need an owning [`CommitData`](rustdoc:grit_lib::objects::CommitData).

[`Odb::hash`](rustdoc:grit_lib::objects::odb::Odb) (or [`HashAlgo::hash_object`](rustdoc:grit_lib::objects::HashAlgo)) computes an id without writing—useful for dry runs and tests. The algorithm follows the repository’s configured object format (SHA-1 or SHA-256).

Object ids and file checksums share the same digest algorithms but serve different roles. [`ObjectHasher`](rustdoc:grit_lib::objects::hash::ObjectHasher) and [`HashAlgo::digest`](rustdoc:grit_lib::objects::HashAlgo) name Git objects (canonical `"<kind> <len>\\0<payload>"` bytes or raw payloads that become an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId)). [`ChecksumHasher`](rustdoc:grit_lib::objects::hash::ChecksumHasher), [`HashAlgo::checksum_digest`](rustdoc:grit_lib::objects::HashAlgo), [`HashingWriter`](rustdoc:grit_lib::objects::hash::HashingWriter), and [`verify_trailer`](rustdoc:grit_lib::objects::hash::verify_trailer) cover pack, index, MIDX, and commit-graph trailers—integrity checks, not object identities.

## Collision detection

SHA-1 object ids are computed with collision detection (sha1dc). [`hash_object`](rustdoc:grit_lib::objects::hash::hash_object), [`HashAlgo::hash_object`](rustdoc:grit_lib::objects::HashAlgo::hash_object), [`HashAlgo::digest`](rustdoc:grit_lib::objects::HashAlgo::digest), [`ObjectHasher::finalize`](rustdoc:grit_lib::objects::hash::ObjectHasher::finalize), and [`Odb::hash`](rustdoc:grit_lib::objects::odb::Odb::hash) return [`Result`](rustdoc:grit_lib::error::Result) and surface [`Error::HashCollision`](rustdoc:grit_lib::error::Error::HashCollision) when a chosen-prefix collision is detected. Ingestion paths (`Odb::write`, pack indexing, worktree staging) propagate the same error instead of storing a colliding object.

```rust
use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::objects::hash::hash_object;

let payload = b"hello\n";
match hash_object(HashAlgo::Sha1, ObjectKind::Blob, payload) {
    Ok(oid) => println!("blob id {oid}"),
    Err(Error::HashCollision { oid }) => {
        eprintln!("refusing colliding digest {}", oid.to_hex());
    }
    Err(e) => return Err(e.into()),
}
```

Patch ids, rerere ids, and pack/index trailers use [`ChecksumHasher`](rustdoc:grit_lib::objects::hash::ChecksumHasher)—same digests, not object identities, and no collision guard on those paths.

## Revision labels

[`Repository::revision_label`](rustdoc:grit_lib::repo::Repository::revision_label) returns an optional [`RevisionLabel`](rustdoc:grit_lib::revwalk::object_inspect::RevisionLabel) for a short local branch or tag name. `HEAD` labels its symbolic local branch, including an unborn branch; detached `HEAD` has no label. Short branch names take precedence over equally named tags. It uses the repository's ref store, including injected backends. This is the classification used by `grit show`; resolve the revision separately when you need its object id.

`RevisionLabel::Tag` carries the tag ref's short name and an optional parsed annotation. Lightweight tags have no annotation. Annotated tags retain their original tag header, tagger, target, extra headers and untrimmed message; the target is not peeled. Missing labels return `None`, while unreadable tag objects and malformed annotations return library errors.

## Reading

[`Odb::read`](rustdoc:grit_lib::objects::odb::Odb) returns an [`Object`](rustdoc:grit_lib::objects::Object) with `kind` and uncompressed `data`. Packed and loose objects share the same API.

For hot loops (tree walks, batch cat-file), [`Odb::read_into`](rustdoc:grit_lib::objects::odb::Odb::read_into) inflates into a caller-owned [`Vec`](https://doc.rust-lang.org/std/vec/struct.Vec.html) and returns the [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Payload bytes stay in the buffer; wrap them with [`ObjectRef::from_buf`](rustdoc:grit_lib::objects::ObjectRef::from_buf) when you need a borrowed view. The same method exists on [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) backends (default: copy from `read`; native for loose files and packs).

When you only need type and size (for example listing objects without loading blob bodies), use [`Odb::read_info`](rustdoc:grit_lib::objects::odb::Odb) (see `read_info` on [`Odb`](rustdoc:grit_lib::objects::odb::Odb)). It returns [`ObjectInfo`](rustdoc:grit_lib::objects::ObjectInfo) and avoids inflating full payloads for loose objects and non-delta pack entries; delta chains are resolved from headers and delta size varints only.

## Streaming large objects

For user-facing object inspection, [`Repository::read_object_info_for_display`](rustdoc:grit_lib::repo::Repository::read_object_info_for_display) returns kind and size, and [`Repository::with_object_stream_for_display`](rustdoc:grit_lib::repo::Repository::with_object_stream_for_display) passes the payload reader to a callback. Both follow replace refs unless the repository environment disables them. `grit cat` uses these APIs to stream large blobs in human output; JSON and Markdown keep their documented content fields.

[`Odb::with_stream`](rustdoc:grit_lib::objects::odb::Odb::with_stream) hands a callback an [`ObjectStream`](rustdoc:grit_lib::objects::odb::ObjectStream) (kind, size and a reader over the payload) and returns the callback's result, or `None` when the object is missing. Loose objects and objects stored whole in a pack are inflated as you read, in fixed-size chunks, so copying a multi-gigabyte blob to a file takes a few hundred KiB of memory. Pack deltas are rebuilt in memory first, which Git also has to do. A truncated or corrupt object, including one whose zlib checksum fails, comes back as an `io::Error` from the reader. [`Odb::open_stream`](rustdoc:grit_lib::objects::odb::Odb::open_stream) returns a stream that owns its payload, so it reads the whole object first. Use it only for small objects.

```rust
use std::io::{Read, Write};

let copied = odb.with_stream(&blob_id, |stream| {
    let mut reader = stream.reader;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    Ok(stream.size)
})?;
```

Checkout uses this path. `restore`, `switch`, `reset --hard` and clone stream any regular file larger than 1 MiB straight into the work tree when no content conversion applies to its path (no `ident`, `working-tree-encoding`, smudge filter or LF-to-CRLF setting; see [`worktree_conversion_is_identity`](rustdoc:grit_lib::worktree::crlf::worktree_conversion_is_identity)). Files at or under 1 MiB, which is nearly all of them, are still read with one lookup: a loose object's header, or a whole pack entry's header, gives the size before the body is inflated, so the size check is part of the read. A file that does stream is looked up twice, once to size it and once to open the stream, and a whole pack entry's bytes are checked against the pack index CRC before streaming, so a corrupt copy is never streamed. Streamed data goes to a temporary file next to the destination, which is renamed into place only after the object's size and zlib checksum have been verified. A corrupt object fails the checkout and leaves the old file, or no file, in place. The check that a restored file matches its blob hashes large files in chunks too, when nothing would change the file on its way into Git ([`git_conversion_is_identity`](rustdoc:grit_lib::worktree::crlf::git_conversion_is_identity)).

## Reading commits

[`Odb::read_commit`](rustdoc:grit_lib::objects::odb::Odb::read_commit) (also on [`Objects`](rustdoc:grit_lib::objects::handle::Objects)) reads an id, checks it names a commit and returns the parsed [`CommitData`](rustdoc:grit_lib::objects::CommitData). When you only need the root tree, [`Odb::commit_tree`](rustdoc:grit_lib::objects::odb::Odb::commit_tree) decodes just the commit headers. Neither follows annotated tags; peel first (see below) when the id may name one. An id that names a tree, blob or tag fails with `UnexpectedObjectKind` on [`Error`](rustdoc:grit_lib::error::Error), which carries the id and both kinds, so callers can tell "wrong kind of object" from a missing or corrupt one without matching message text. Stash, merge, pull, the history engine and commit-graph Bloom filters read commits through these two methods.

## Looking up a path in a tree

[`Odb::tree_entry_at_path`](rustdoc:grit_lib::objects::odb::Odb::tree_entry_at_path) returns the [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry) at a `/`-separated path such as `b"src/lib.rs"` under a root tree, or `None` when a component is missing or a directory on the way is really a file. The last entry comes back whatever its mode (file, symlink, gitlink or subtree), so filter on `mode` when only files will do. Trees on the way go through the handle's bounded tree cache and are scanned without allocating an entry per name, so looking up many paths under the same directories stays cheap. Diff, stash, blame, the history engine and snapshot checkout all look up single paths this way.

## Tree iteration without copying names

[`parse_tree`](rustdoc:grit_lib::objects::parse_tree) allocates an owning [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry) for every name. For hot paths that only walk a tree once (object walks, pack enumeration, fsck), use [`TreeRefIter`](rustdoc:grit_lib::objects::TreeRefIter): it yields [`TreeEntryRef`](rustdoc:grit_lib::objects::TreeEntryRef) values that borrow name bytes from the tree blob. Pass the repository hash width with [`TreeRefIter::with_oid_len`](rustdoc:grit_lib::objects::TreeRefIter::with_oid_len), or call [`TreeRefIter::infer_oid_len`](rustdoc:grit_lib::objects::TreeRefIter::infer_oid_len) when the algorithm is not threaded through (same SHA-1-then-SHA-256 detection as [`parse_tree`](rustdoc:grit_lib::objects::parse_tree)). Call [`TreeEntryRef::to_owned`](rustdoc:grit_lib::objects::TreeEntryRef::to_owned) when you need a [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry). The compiled example below reads a tree with both APIs.

## Peeling tags

A ref tip can name an annotated tag, or a tag of a tag. `Odb::peel_tags` (on [`Odb`](rustdoc:grit_lib::objects::odb::Odb)) follows the chain and returns the id and kind of the first non-tag object; `Odb::peel_to_commit` returns `Some(commit)` when that object is a commit and `None` for a tree or blob. Only tag objects are read in full, and only their `object` header is parsed, so tags with non-UTF-8 messages peel fine; the final object's kind comes from `read_info`. Chains of any length are followed. A missing object, a tag without a valid `object` header, or a store that serves a tag cycle is an error, not `None`, so you can tell "not a commit" apart from a broken repository. Revision walks, fetch negotiation, bundles, commit-graph tips, `packed-refs` peel lines, tag listing and log decorations all peel through these two methods.

## Pack read caching

[`Odb`](rustdoc:grit_lib::objects::odb::Odb) owns a repository-scoped [`PackStore`](rustdoc:grit_lib::objects::pack_store::PackStore): pack directory listings, parsed `.idx` files, pack bytes, MIDX layers, and the delta-base LRU. Cloned [`Odb`](rustdoc:grit_lib::objects::odb::Odb) handles share the same store; alternate object directories get separate stores on the parent [`Odb`](rustdoc:grit_lib::objects::odb::Odb).

After repack, garbage collection, or installing a pack with [`install_pack_bytes`](rustdoc:grit_lib::objects::index_pack::install_pack_bytes), call [`Odb::invalidate_packs`](rustdoc:grit_lib::objects::odb::Odb) so the next read rescans `objects/pack/`. If another [`Odb`](rustdoc:grit_lib::objects::odb::Odb) in the same process still holds a stale listing, a lookup miss retriggers directory reprepare when the pack folder’s mtime changes.

For batch reads (`cat-file --batch`, `--batch-all-objects`), wrap the loop in [`Odb::with_pack_read_context`](rustdoc:grit_lib::objects::odb::Odb) so pack indexes, mmap-backed pack bytes, and the delta-base LRU stay on one thread-local context instead of reinstalling it per object. [`Odb::read`](rustdoc:grit_lib::objects::odb::Odb) detects an active matching context and skips nested setup.

When iterating a pack in offset order (unordered `--batch-all-objects`), prefer [`read_object_from_pack_at_offset`](rustdoc:grit_lib::objects::pack) with the entry offset from [`PackIndex`](rustdoc:grit_lib::objects::pack::PackIndex) so the read path does not repeat index lookup by OID.

The reverse direction, from a pack offset to the object stored there (resolving an `OFS_DELTA` base, for example), is [`PackIndex::find_position_by_offset_sorted`](rustdoc:grit_lib::objects::pack::PackIndex::find_position_by_offset_sorted). Each index decodes its offset table once, from Git's `.rev` reverse index when the pack has a valid one and by sorting the index rows otherwise, and clones of the index share it, so every later lookup is a binary search.

## Pack deltas (encoding)

When building new pack deltas (as opposed to reusing on-disk zlib), grit-lib indexes the base blob with [`DeltaIndex`](rustdoc:grit_lib::objects::delta_encode::DeltaIndex) and encodes targets with a rolling fingerprint matcher. Reuse one index across many targets in a pack-objects window; `encode` on that index accepts an optional `max_size` cap (zero means no limit). An empty target yields a valid delta (header varints only). [`encode_delta`](rustdoc:grit_lib::objects::delta_encode::encode_delta) is a convenience wrapper for one-off base/target pairs.

## Example

This example initializes a repository, writes a blob, then a tree and commit through the typed API, verifies structure in memory, and prints the commit id:

<!-- include: grit-examples/src/bin/guide_objects.rs -->

Objects land on disk in standard loose format. System Git can read them:

```bash
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).
