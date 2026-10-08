//! Git [`GIT_NAMESPACE`](https://git-scm.com/docs/git#Documentation/git.txt-codeGITNAMESPACEcode)
//! handling: map logical ref names to storage under `refs/namespaces/.../`.
//!
//! Matches `get_git_namespace()` in Git's `environment.c`.

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::environment::Environment;

/// Raw value of `GIT_NAMESPACE` (may contain `/`-separated components).
#[must_use]
pub fn raw_git_namespace(env: &Environment) -> Option<String> {
    let t = env.git_namespace.as_deref()?.trim();
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
    let raw = raw_git_namespace(env)?;
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
pub fn storage_ref_name(env: &Environment, logical: &str) -> String {
    match ref_storage_prefix(env) {
        Some(p) if logical.starts_with(&p) => logical.to_owned(),
        Some(p) => format!("{p}{logical}"),
        None => logical.to_owned(),
    }
}

/// If `storage` lives under the active namespace, return the logical ref name.
#[must_use]
pub fn logical_ref_name_from_storage(env: &Environment, storage: &str) -> Option<String> {
    let p = ref_storage_prefix(env)?;
    storage.strip_prefix(&p).map(str::to_owned)
}

/// Strip the active namespace prefix from `refname` when present (for advertisements / display).
#[must_use]
pub fn strip_namespace_prefix<'a>(
    env: &Environment,
    refname: &'a str,
) -> std::borrow::Cow<'a, str> {
    match ref_storage_prefix(env) {
        Some(p) if refname.starts_with(&p) => {
            std::borrow::Cow::Owned(refname[p.len()..].to_owned())
        }
        _ => std::borrow::Cow::Borrowed(refname),
    }
}
