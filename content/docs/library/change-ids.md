---
title: Change ids
summary: Stable change-id commit headers, derivation, and grit.changeIds config.
---

Every commit Grit creates can carry a **`change-id`** header: an opaque token that stays with the same logical change across rewrites (amend, rebase, history edits). Commits without the header still have a **derived** id (Jujutsu-compatible) computed from the commit object id.

## Header format

- Primary header: `change-id <value>`
- Legacy name: `gitbutler-change-id <value>` (read for compatibility; new writes use `change-id` only)
- Random ids use **reverse-hex**: 16 bytes encoded as 32 characters where nibble `n` maps to `'z' - n` (alphabet `k`–`z`).

[`ChangeId::from_bytes`](rustdoc:grit_lib::change_id::ChangeId::from_bytes) and [`ChangeId::parse_reverse_hex`](rustdoc:grit_lib::change_id::ChangeId::parse_reverse_hex) handle that encoding. Legacy values may be arbitrary strings (for example UUIDs).

## Reading and writing

On [`CommitData`](rustdoc:grit_lib::objects::CommitData):

- [`change_id`](rustdoc:grit_lib::change_id::change_id) — stored header if present
- [`set_change_id`](rustdoc:grit_lib::change_id::set_change_id) — set or replace the header before any `gpgsig` / `gpgsig-sha256` block
- [`resolved_change_id`](rustdoc:grit_lib::change_id::resolved_change_id) — stored id or [`ChangeId::derived_from_commit_id`](rustdoc:grit_lib::change_id::ChangeId::derived_from_commit_id)

Derivation takes object-id bytes `4..20`, reverses their order, bit-reverses each byte, then reverse-hex encodes (same scheme as Jujutsu/GitButler).

## Policy and config

[`ChangeIdPolicy::from_config`](rustdoc:grit_lib::change_id::ChangeIdPolicy::from_config) reads:

| Key | Default | Effect |
| --- | --- | --- |
| `grit.changeIds` | `true` | When false, [`id_for_new_commit`](rustdoc:grit_lib::change_id::ChangeIdPolicy::id_for_new_commit) returns `None` |
| `grit.testing.changeId` | unset | `content-hash` derives ids from the hash of the serialized commit without change-id headers; any other non-empty value is used verbatim as the id |

Embedders inject randomness with [`ChangeIdGenerator`](rustdoc:grit_lib::change_id::ChangeIdGenerator) on [`RepositoryOptions::with_change_id_generator`](rustdoc:grit_lib::environment::RepositoryOptions::with_change_id_generator); [`Repository::change_id_generator`](rustdoc:grit_lib::repo::Repository::change_id_generator) exposes the active generator.

## Where ids are assigned

[`create_commit`](rustdoc:grit_lib::porcelain::commit::create_commit) (and the CLI `grit commit` built on it) applies [`ChangeIdPolicy`](rustdoc:grit_lib::change_id::ChangeIdPolicy) before serializing: new commits, the initial commit, merge conclusions, and amends that need a stored id. Amends keep an existing `change-id` / `gitbutler-change-id` header; when the replaced tip had none, the derived id from that tip is stored.

[`prepare_commit_bytes`](rustdoc:grit_lib::porcelain::commit::prepare_commit_bytes) and [`write_commit_object`](rustdoc:grit_lib::porcelain::commit::write_commit_object) do **not** add change ids — stash, notes, and other auxiliary commits stay header-free unless the caller sets one.

## Git compatibility

The header is an ordinary commit header line. System `git` preserves it on read/write; `git fsck --strict` accepts commits that carry `change-id`.
