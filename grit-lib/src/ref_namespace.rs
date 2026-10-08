//! Git [`GIT_NAMESPACE`](https://git-scm.com/docs/git#Documentation/git.txt-codeGITNAMESPACEcode)
//! handling: map logical ref names to storage under `refs/namespaces/.../`.
//!
//! Matches `get_git_namespace()` in Git's `environment.c`.

use crate::check_ref_format::{check_refname_format, RefNameOptions};

/// Raw value of `GIT_NAMESPACE` (may contain `/`-separated components).
#[must_use]
pub fn raw_git_namespace_from_env() -> Option<String> {
    let v = crate::environment::Environment::capture_process().var("GIT_NAMESPACE")?;
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
pub fn ref_storage_prefix() -> Option<String> {
    let raw = raw_git_namespace_from_env()?;
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
    match ref_storage_prefix() {
        Some(p) if logical.starts_with(&p) => logical.to_owned(),
        Some(p) => format!("{p}{logical}"),
        None => logical.to_owned(),
    }
}

/// If `storage` lives under the active namespace, return the logical ref name.
#[must_use]
pub fn logical_ref_name_from_storage(storage: &str) -> Option<String> {
    let p = ref_storage_prefix()?;
    storage.strip_prefix(&p).map(str::to_owned)
}

/// Strip the active namespace prefix from `refname` when present (for advertisements / display).
#[must_use]
pub fn strip_namespace_prefix(refname: &str) -> std::borrow::Cow<'_, str> {
    match ref_storage_prefix() {
        Some(p) if refname.starts_with(&p) => {
            std::borrow::Cow::Owned(refname[p.len()..].to_owned())
        }
        _ => std::borrow::Cow::Borrowed(refname),
    }
}
