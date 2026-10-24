//! Read `core.precomposeunicode` without opening a full [`Repository`](crate::repo::Repository).
//!
//! Used for pathspec matching when argv may use NFD spellings while the index stores NFC.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::parse_config_parameters;
use crate::environment::Environment;
use crate::unicode_normalization::probe_filesystem_normalizes_nfd_to_nfc;

fn parse_ceiling_directories_paths(env: &Environment) -> Vec<PathBuf> {
    let Some(raw) = env.git_ceiling_directories.as_deref() else {
        return Vec::new();
    };
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(':')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return None;
            }
            Some(
                p.canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(s.trim_end_matches('/'))),
            )
        })
        .collect()
}

fn path_for_ceiling_compare(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn offset_1st_component(path: &str) -> usize {
    if path.starts_with('/') {
        1
    } else {
        0
    }
}

fn longest_ancestor_length(path: &str, ceilings: &[String]) -> Option<usize> {
    if path == "/" {
        return None;
    }
    let mut max_len: Option<usize> = None;
    for ceil in ceilings {
        let mut len = ceil.len();
        while len > 0 && ceil.as_bytes().get(len - 1) == Some(&b'/') {
            len -= 1;
        }
        if len == 0 {
            continue;
        }
        if path.len() <= len + 1 {
            continue;
        }
        if !path.starts_with(&ceil[..len]) {
            continue;
        }
        if path.as_bytes().get(len) != Some(&b'/') {
            continue;
        }
        if path.as_bytes().get(len + 1).is_none() {
            continue;
        }
        max_len = Some(max_len.map_or(len, |m| m.max(len)));
    }
    max_len
}

fn probe_git_dir_at(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.canonicalize().unwrap_or(dot_git));
    }
    if dot_git.is_file() {
        let content = fs::read_to_string(&dot_git).ok()?;
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("gitdir:") {
                let p = Path::new(rest.trim());
                let resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    dir.join(p)
                };
                return Some(resolved.canonicalize().unwrap_or(resolved));
            }
        }
    }
    None
}

/// Walk parents of the discovery directory in `environment` for `.git`, honouring
/// `GIT_DIR`, `GIT_CEILING_DIRECTORIES` and the other discovery variables like Git.
pub fn locate_git_dir_from_environment(environment: &Environment) -> Option<PathBuf> {
    if let Some(dir) = environment.git_dir.as_deref() {
        let cwd = environment.discovery_cwd();
        let mut git_dir = PathBuf::from(dir);
        if git_dir.is_relative() {
            git_dir = cwd.join(git_dir);
        }
        return crate::repo::resolve_git_directory_arg(&git_dir).ok();
    }

    let start_canon = environment.discovery_cwd();
    let ceilings: Vec<String> = parse_ceiling_directories_paths(environment)
        .into_iter()
        .map(|p| path_for_ceiling_compare(&p))
        .collect();
    let mut dir_buf = path_for_ceiling_compare(&start_canon);
    let min_offset = offset_1st_component(&dir_buf);
    let mut ceil_offset: isize = longest_ancestor_length(&dir_buf, &ceilings)
        .map(|n| n as isize)
        .unwrap_or(-1);
    if ceil_offset < 0 {
        ceil_offset = min_offset as isize - 2;
    }

    loop {
        if let Some(gd) = probe_git_dir_at(Path::new(&dir_buf)) {
            return Some(gd);
        }

        let mut offset: isize = dir_buf.len() as isize;
        if offset <= min_offset as isize {
            break;
        }
        loop {
            offset -= 1;
            if offset <= ceil_offset {
                break;
            }
            if dir_buf
                .as_bytes()
                .get(offset as usize)
                .is_some_and(|b| *b == b'/')
            {
                break;
            }
        }
        if offset <= ceil_offset {
            break;
        }
        let off_u = offset as usize;
        let new_len = if off_u > min_offset {
            off_u
        } else {
            min_offset
        };
        dir_buf.truncate(new_len);
    }
    None
}

/// `git -c core.precomposeunicode=…` overrides local config (last token wins).
fn precompose_from_git_config_parameters(env: &Environment) -> Option<bool> {
    let raw = env.git_config_parameters.as_deref()?;
    let mut last: Option<bool> = None;
    for entry in parse_config_parameters(raw) {
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("core.precomposeunicode") {
            continue;
        }
        let v = v.trim();
        last = Some(matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        ));
    }
    last
}

/// Effective `core.precomposeunicode` after the normal config cascade (system, global, local,
/// `GIT_CONFIG_PARAMETERS`), matching [`crate::config::ConfigSet::load`].
///
/// Does not imply argv should be rewritten: Git only runs `precompose_argv_prefix` when the
/// filesystem aliases NFD/NFC (or the test harness forces that probe for `git init`).
#[must_use]
pub fn effective_core_precomposeunicode(git_dir: Option<&Path>) -> bool {
    effective_core_precomposeunicode_with_config(git_dir, None)
}

/// Like [`effective_core_precomposeunicode`], but uses an already-loaded config snapshot when provided.
#[must_use]
pub fn effective_core_precomposeunicode_with_config(
    git_dir: Option<&Path>,
    config: Option<&crate::config::ConfigSet>,
) -> bool {
    if let Some(v) =
        precompose_from_git_config_parameters(&crate::environment::Environment::empty())
    {
        return v;
    }
    if let Some(cfg) = config {
        return cfg
            .get_bool("core.precomposeunicode")
            .and_then(|r| r.ok())
            .unwrap_or(false);
    }
    let Some(gd) = git_dir else {
        return false;
    };
    crate::config::ConfigSet::load(&crate::environment::Environment::empty(), Some(gd), true)
        .ok()
        .and_then(|cfg| cfg.get_bool("core.precomposeunicode").and_then(|r| r.ok()))
        .unwrap_or(false)
}

/// True when the filesystem aliases NFD and NFC spellings for the same path (macOS / HFS+ style).
///
/// `GIT_TEST_UTF8_NFD_TO_NFC` does **not** count here: it only makes `git init` write
/// `core.precomposeunicode` on Linux; argv and directory walks must still use the bytes the shell
/// passed when the FS does not alias.
#[must_use]
pub fn filesystem_nfd_nfc_aliases(git_dir: &Path) -> bool {
    probe_filesystem_normalizes_nfd_to_nfc(git_dir).unwrap_or(false)
}

/// NFC-normalize command-line path arguments (Git's `precompose_argv_prefix`).
#[must_use]
pub fn argv_precompose_enabled(git_dir: Option<&Path>) -> bool {
    if !effective_core_precomposeunicode(git_dir) {
        return false;
    }
    let Some(gd) = git_dir else {
        return false;
    };
    filesystem_nfd_nfc_aliases(gd)
}

/// NFC-normalize for pathspec comparisons when the repo opts into precomposed Unicode storage.
///
/// Prefer [`PathspecMatchContext::precompose_paths`](crate::pathspec::PathspecMatchContext::precompose_paths)
/// on hot paths after opening a [`Repository`](crate::repo::Repository).
#[must_use]
pub fn pathspec_precompose_enabled() -> bool {
    false
}
