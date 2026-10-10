//! Git [`GIT_NAMESPACE`](https://git-scm.com/docs/git#Documentation/git.txt-codeGITNAMESPACEcode)
//! handling: map logical ref names to storage under `refs/namespaces/.../`.
//!
//! Matches `get_git_namespace()` in Git's `environment.c`.

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::environment::Environment;

/// Raw value of `GIT_NAMESPACE` (may contain `/`-separated components).
#[must_use]
pub fn raw_git_namespace_from_env(env: &Environment) -> Option<String> {
    let v = env.git_namespace.as_deref()?;
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Storage prefix for refs when a namespace is active, e.g. `refs/namespaces/foo/`.
/// Returns `None` when `GIT_NAMESPACE` is unset or empty.
#[must_use]
pub fn ref_storage_prefix(env: &Environment) -> Option<String> {
    let raw = raw_git_namespace_from_env(env)?;
    let mut buf = String::new();
    for part in raw.split('/') {
        if part.is_empty() {
            continue;
        }
        buf.push_str("refs/namespaces/");
        buf.push_str(part);
        buf.push('/');
    }
    while buf.ends_with('/') && buf.len() > 1 {
        buf.pop();
    }
    if !buf.is_empty() {
        let opts = RefNameOptions::default();
        if check_refname_format(&buf, &opts).is_err() {
            return None;
        }
        buf.push('/');
    }
    if buf.is_empty() {
        None
    } else {
        Some(buf)
    }
}

/// Map a logical ref name to its on-disk ref name inside the namespace.
#[must_use]
pub fn storage_ref_name(logical: &str) -> String {
    storage_ref_name_env(&Environment::empty(), logical)
}

/// Whether `logical` is stored outside the namespace (e.g. global `HEAD` in `$GIT_DIR`).
#[must_use]
pub fn ref_skips_namespace_prefix(logical: &str) -> bool {
    logical == "HEAD"
        || (!logical.is_empty()
            && !logical.contains('/')
            && logical
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '-' || c == '_'))
}

/// Map a logical ref name using an explicit storage prefix (not from environment).
#[must_use]
pub fn storage_ref_name_with_prefix(prefix: Option<&str>, logical: &str) -> String {
    if prefix.is_some() && ref_skips_namespace_prefix(logical) {
        return logical.to_owned();
    }
    match prefix {
        Some(p) if logical.starts_with(p) => logical.to_owned(),
        Some(p) => format!("{p}{logical}"),
        None => logical.to_owned(),
    }
}

/// Strip an explicit storage prefix to recover the logical ref name.
#[must_use]
pub fn logical_ref_name_with_prefix(prefix: Option<&str>, storage: &str) -> String {
    match prefix {
        Some(p) if storage.starts_with(p) => storage[p.len()..].to_owned(),
        _ => storage.to_owned(),
    }
}

/// Map a logical ref name using an explicit [`Environment`].
#[must_use]
pub fn storage_ref_name_env(env: &Environment, logical: &str) -> String {
    match ref_storage_prefix(env) {
        Some(p) if logical.starts_with(&p) => logical.to_owned(),
        Some(p) => format!("{p}{logical}"),
        None => logical.to_owned(),
    }
}

/// Active namespace storage prefix using an empty environment (no `GIT_NAMESPACE`).
#[must_use]
pub fn ref_storage_prefix_default() -> Option<String> {
    ref_storage_prefix(&Environment::empty())
}

/// If `storage` lives under the active namespace, return the logical ref name.
#[must_use]
pub fn logical_ref_name_from_storage(storage: &str) -> Option<String> {
    logical_ref_name_from_storage_env(&Environment::empty(), storage)
}

/// Like [`logical_ref_name_from_storage`] with an explicit environment.
#[must_use]
pub fn logical_ref_name_from_storage_env(env: &Environment, storage: &str) -> Option<String> {
    let p = ref_storage_prefix(env)?;
    storage.strip_prefix(&p).map(str::to_owned)
}

/// Strip the active namespace prefix from `refname` when present (for advertisements / display).
#[must_use]
pub fn strip_namespace_prefix(refname: &str) -> String {
    strip_namespace_prefix_env(&Environment::empty(), refname)
}

/// Like [`strip_namespace_prefix`] with an explicit environment.
#[must_use]
pub fn strip_namespace_prefix_env(env: &Environment, refname: &str) -> String {
    match ref_storage_prefix(env) {
        Some(p) if refname.starts_with(&p) => refname[p.len()..].to_owned(),
        _ => refname.to_owned(),
    }
}
