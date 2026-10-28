---
title: grit upload-pack
summary: Serve a fetch or clone of a repository over stdin and stdout.
group: Plumbing
order: 1
---

## Synopsis

```text
grit upload-pack [--stateless-rpc] [--advertise-refs] <directory>
```

## Description

The server side of fetch and clone. It speaks the Git wire protocol on stdin and stdout, so any Git client, `git` or `grit`, can fetch from a repository it serves. You don't normally run it yourself: an SSH server runs it when a client connects, and `grit-http-server` answers smart HTTP requests by calling the same logic in `grit-lib` in-process (no separate `grit` binary next to the server). It doesn't appear in `grit --help`.

`<directory>` is the repository to serve: a bare repository, a working tree, or a path that names one once `.git` is added. Unlike other commands, `grit upload-pack` doesn't search parent directories, so it serves exactly the repository it was given.

It supports Git protocol versions 0, 1 and 2, chosen by the client through the `GIT_PROTOCOL` environment variable. Refs matching `uploadpack.hideRefs` or `transfer.hideRefs` are not shown to clients.

The pack is built the way Git builds one when serving: objects already stored in the repository's packs are copied as they are, whole objects and deltas alike, and only objects that are not yet stored as deltas (loose or newly written blobs, trees and commits) go through the delta search, which reads each object only while it is in the search window, so serving a repository full of loose objects costs about as much memory as serving a packed one. `pack.window` and `pack.depth` set that search's window and maximum chain length; as with Git, `pack.depth = 0` stops new deltas but still sends the deltas already on disk. Each delta's base is written before it, so deltas use the compact offset form, and partial-clone (filtered) packs stream the same way. A thin pack is sent only when the client asked for one and already has objects to delta against. The pack is streamed to the client while it is written, so a full clone of an already-packed repository costs about as much time and memory as it does with `git upload-pack`.

In protocol v2 negotiation, each round of `have` lines is answered with an `acknowledgments` section that lists the haves the server also has (or `NAK` when there are none). When those haves already cover everything the client wants, the server adds `ready` and sends the pack in the same response, so a client that is a few commits behind finishes after one round instead of walking its whole history back to the server. A want counts as covered when its history reaches an acknowledged commit or one of that commit's parents, without going past the oldest acknowledged commit's date. Wants that are trees or blobs never hold the negotiation open. A client that sends `wait-for-done` (as `git fetch --negotiate-only` does) never gets `ready`: the server keeps answering with acknowledgments until the client sends `done`.

Shallow and deepen requests (`deepen`, `deepen-since`, `deepen-not`, `deepen-relative`, client `shallow` lines) are honored in protocol v0/v1 and in v2 `fetch` (including the `shallow-info` section). `deepen-not`, which `git clone --shallow-exclude=<ref>` sends, takes a ref name as Git does: a full name such as `refs/tags/v1.0`, or a short name such as `v1.0` that must match exactly one ref. A full object id also works. Annotated tags are peeled to their commit. Wanted annotated tags count depth from the commit they name, and `shallow` lines always name commits. With `include-tag`, the pack gets every annotated tag (and the tags it names in turn) whose final object, whether a commit, tree or blob, is among the objects being sent. The check runs against the pack's object list, not the history, so its cost follows the size of the fetch. A shallow fetch therefore only adds the tags on commits it actually sends, and `git clone --depth 1` of a tagged repository receives one commit, as with `git upload-pack`. Partial-clone filters (`filter blob:none`, `blob:limit=<n>`, `tree:<depth>`, `object:type=<type>`, `sparse:oid=<blob>`, and `combine:` of them) follow `uploadpack.allowFilter` and `uploadpackfilter.*` policy.

A filtered pack holds the objects `git upload-pack` would send for the same request: the wanted objects and every annotated tag the client asked for or that `include-tag` adds, then the commits, trees and blobs the filter keeps. It is built and streamed like an unfiltered one, reusing the objects and deltas already stored, and the pack bitmap selects the objects when it can apply the filter (`blob:none`, `blob:limit`, `tree:0`). A blob-less clone therefore costs about what a full clone does without the blobs. Trees a filter rules out are not read, and blob sizes are only looked up for `blob:limit`. Under `combine:` with `tree:<depth>`, `git` can leave out a tree it first met too deep even when it meets the tree again within the depth; grit sends such a tree.

Protocol v2 `want-ref` is accepted only when `uploadpack.allowRefInWant` is set, and the response then includes a `wanted-refs` section with the object each requested ref resolved to. Asking for the same ref twice is a protocol error. Protocol v2 `sideband-all` is advertised and accepted only when `uploadpack.allowSidebandAll` is set. Optional `want` rules follow `uploadpack.allowTipSha1InWant`, `uploadpack.allowReachableSha1InWant` and `uploadpack.allowAnySHA1InWant`. The last lets a client want any object the repository has, reachable or not, and also turns on the other two capabilities in protocol v0. A large-object promisor needs it: it holds blobs that no ref points at, and clients fetch them by object id.

In `--stateless-rpc` mode (smart HTTP POST), any commit reachable from the server's current ref tips may appear in a `want` line without those config flags. That matches Git when ref discovery and the fetch run as separate requests and a push can move refs in between. Stateful sessions (SSH and the negotiation half of a long-lived connection) still require each `want` to match the ref advertisement from that session unless `uploadpack.allowReachableSha1InWant` or `uploadpack.allowTipSha1InWant` is set.

### Promisor remotes

A repository that keeps some of its objects on other remotes (a large-object promisor holding the big blobs, for example) can tell clients about them. With `promisor.advertise = true`, the protocol v2 capabilities end with a `promisor-remote` line that names each of the repository's promisor remotes (`remote.<name>.promisor = true`, a `remote.<name>.partialCloneFilter`, or the `extensions.partialClone` remote, listed last) that has a URL. `promisor.sendFields` adds each remote's `partialCloneFilter` and `token` when they are set. Protocol v0 and v1 never carry the capability.

```text
promisor-remote=name=lop,url=https://lop.example.com/project.git,partialCloneFilter=blob:limit=1m
```

A client that accepts some of these remotes says so with a `promisor-remote=<names>` line in the capabilities of its command requests. The server keeps only the names it advertised. In a stateful session (SSH, `git://`, a local pipe) the acceptance lasts until the connection closes, so a reply sent with `ls-refs` covers the `fetch` that follows. Over smart HTTP each request stands alone: only a reply in the `fetch` request itself counts. `grit` clients send it with both commands; `git` clients send it only with `ls-refs`, so over smart HTTP their acceptance never reaches the fetch. A `promisor-remote` line with no names is a protocol error.

### Serving a repository whose large blobs live elsewhere

A repository with promisor remotes of its own may lack some objects: after [`grit gc offload`](../gc/#grit-gc-offload) its large blobs live only on a large-object promisor (LOP), and its other objects sit in a `.promisor` pack. Such a repository is *partial*, and upload-pack serves it by Git's rules:

- When the client accepted at least one advertised promisor remote, a promised blob the repository does not have is left out of the pack. The client fetches it from the LOP. A blob is promised when an object in a promisor pack refers to it.
- Otherwise the request fails. The server first fetches the missing blobs from its own promisor remotes only if lazy fetch is on. That takes `uploadpack.lazyFetch = true` (a grit setting) or `GIT_NO_LAZY_FETCH=0` in the environment of `grit upload-pack` or `grit serve`. Git's upload-pack has the same `GIT_NO_LAZY_FETCH=0` switch. The fetched blobs go into a new `.promisor` pack in the served repository, and the client's filter still applies to them: a `blob:limit` client is not sent the large blobs the server fetched.
- A missing object that nothing promises is an error in every case.

The pack is planned before anything is sent, so a refusal reaches the client as a clean error, not a broken pack. Protocol v2 clients get an `ERR` line, which Git prints as `fatal: remote error: …`. The message names the blob and the advertised promisor remotes, and suggests `promisor.acceptFromServer` on the client or `uploadpack.lazyFetch` on the server. Protocol v0 clients get the same text on side band 3, or as an `ERR` line when they want the promised blob itself. Wants for a promised blob itself (a client's lazy fetch) follow the same rule; an accepting client gets an empty pack and asks the LOP next.

This applies only to repositories with a promisor remote (`remote.<name>.promisor = true` or `extensions.partialClone`). For those, the pack is chosen with the object walk instead of bitmaps or verbatim pack reuse; other repositories keep the fast path. Only blobs can be left out. A missing commit, tree or tag still fails the request; `grit gc offload` only ever removes blobs.

Over smart HTTP, each request carries its own acceptance (see above). Git clients accept only on `ls-refs`, so their `fetch` arrives without it. A large-object setup served over HTTP to Git clients therefore needs lazy fetch on the server, or a stateful transport (SSH, `git://`).

## Options

| Option | Description |
| --- | --- |
| `<directory>` | The repository to serve. |
| `--stateless-rpc` | Answer a single request and exit, without advertising refs first. Used for smart HTTP. |
| `--advertise-refs` | Print the ref advertisement and exit. Used for the first request of smart HTTP. Also accepted as `--http-backend-info-refs`. |

## Examples

Clone over SSH from a server that has `grit` but not `git` installed:

```console
$ git clone --upload-pack='grit upload-pack' ssh://example.com/srv/git/project.git
```

Set it for a remote you already have:

```console
$ git config remote.origin.uploadpack 'grit upload-pack'
```

## JSON output

None. `--json` has no effect, since stdout carries the Git protocol.


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit receive-pack](../receive-pack/), [grit clone](../clone/), [grit fetch](../fetch/)