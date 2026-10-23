//! Rename a configured remote: tracking refs, then config (see [`RepositoryRemotes::rename`]).
use std::collections::BTreeSet;
use std::ops::ControlFlow;

use crate::config::{canonical_key, ConfigFile, ConfigScope, ConfigSet};
use crate::porcelain::checkout_reflog::checkout_reflog_identity;
use crate::refs::commit_ref_batch_on_store;
use crate::refs::store::{
    Expected, RawRef, RefEntry, RefReflogRename, RefUpdate, RefUpdateFlags, ReflogUpdate,
};
use crate::remote::RemoteError;
use crate::repo::Repository;

use super::remote::{has_remote_section, is_valid_remote_name, RepositoryRemotes};

/// Result of [`RepositoryRemotes::rename`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRename {
    /// Previous remote name.
    pub old: String,
    /// New remote name.
    pub new: String,
    /// Remote-tracking refs renamed under `refs/remotes/<new>/`.
    pub refs_renamed: usize,
    /// Fetch refspec values rewritten to the new remote namespace.
    pub refspecs_updated: Vec<String>,
    /// Local branch short names whose `branch.<name>.remote` or `.pushRemote` was updated.
    pub branches_updated: Vec<String>,
    /// Whether the repository was left unchanged.
    pub dry_run: bool,
}

impl<'repo> RepositoryRemotes<'repo> {
    /// Rename remote `old` to `new`.
    ///
    /// When any `remote.<old>.fetch` refspec maps into `refs/remotes/<old>/`, every
    /// `refs/remotes/<old>/*` ref is renamed in one ref transaction (reflogs preserved),
    /// then the config section is renamed and branch upstream keys are retargeted. Custom
    /// fetch refspecs that do not use the default remote namespace are left unchanged.
    ///
    /// Ref updates commit before config writes so a ref failure leaves config untouched.
    ///
    /// # Errors
    ///
    /// - [`RemoteError::UnknownRemote`] when `old` has no `remote.<old>.*` section.
    /// - [`RemoteError::RemoteExists`] when `new` is already configured.
    /// - [`RemoteError::InvalidRemoteName`] when `new` is not a valid remote name.
    /// - [`RemoteError::Library`] for config, ref, or I/O failures.
    pub fn rename(
        &self,
        old: &str,
        new: &str,
        dry_run: bool,
    ) -> std::result::Result<RemoteRename, RemoteError> {
        if !is_valid_remote_name(new) {
            return Err(RemoteError::InvalidRemoteName(new.to_owned()));
        }
        let repo = self.repository();
        let config = repo.config().map_err(RemoteError::Library)?;
        if !has_remote_section(config.as_ref(), old) {
            return Err(RemoteError::UnknownRemote(old.to_owned()));
        }
        if has_remote_section(config.as_ref(), new) {
            return Err(RemoteError::RemoteExists(new.to_owned()));
        }
        if old == new {
            return Ok(RemoteRename {
                old: old.to_owned(),
                new: new.to_owned(),
                refs_renamed: 0,
                refspecs_updated: Vec::new(),
                branches_updated: Vec::new(),
                dry_run,
            });
        }

        let refspec_marker = format!(":refs/remotes/{old}/");
        let refspecs_need_update = config
            .get_all(&format!("remote.{old}.fetch"))
            .iter()
            .any(|spec| spec.contains(&refspec_marker));

        let refs_renamed = if refspecs_need_update {
            count_remote_tracking_refs(repo, old)?
        } else {
            0
        };

        let (refspecs_updated, branches_updated) =
            plan_config_updates(config.as_ref(), old, new, refspecs_need_update)?;

        let outcome = RemoteRename {
            old: old.to_owned(),
            new: new.to_owned(),
            refs_renamed,
            refspecs_updated,
            branches_updated,
            dry_run,
        };

        if dry_run {
            return Ok(outcome);
        }

        if refspecs_need_update {
            rename_remote_tracking_refs(repo, old, new)?;
        }
        apply_remote_rename_config(repo, old, new, refspecs_need_update)?;
        Ok(outcome)
    }
}

fn remote_ref_prefix(name: &str) -> String {
    format!("refs/remotes/{name}/")
}

fn rename_remote_ref(old: &str, new: &str, refname: &str) -> Option<String> {
    refname
        .strip_prefix(&remote_ref_prefix(old))
        .map(|tail| format!("{}{tail}", remote_ref_prefix(new)))
}

fn count_remote_tracking_refs(repo: &Repository, old: &str) -> Result<usize, RemoteError> {
    let prefix = remote_ref_prefix(old);
    let mut count = 0usize;
    repo.refs()
        .for_each_ref(&prefix, &mut |_: &RefEntry| {
            count += 1;
            ControlFlow::Continue(())
        })
        .map_err(RemoteError::Library)?;
    Ok(count)
}

fn rename_remote_tracking_refs(repo: &Repository, old: &str, new: &str) -> Result<(), RemoteError> {
    let prefix = remote_ref_prefix(old);
    let mut entries = Vec::new();
    repo.refs()
        .for_each_ref(&prefix, &mut |entry: &RefEntry| {
            entries.push(entry.clone());
            ControlFlow::Continue(())
        })
        .map_err(RemoteError::Library)?;

    let identity = checkout_reflog_identity(repo);
    let time = repo.identity_now();
    let mut updates = Vec::with_capacity(entries.len() * 2);
    let mut reflog_renames = Vec::with_capacity(entries.len());

    for entry in entries {
        let Some(new_name) = rename_remote_ref(old, new, &entry.name) else {
            continue;
        };
        let (new_value, delete_expected, symref) = match &entry.value {
            RawRef::Direct(oid) => (Some(RawRef::Direct(*oid)), Expected::Oid(*oid), false),
            RawRef::Symbolic(target) => {
                let new_target =
                    rename_remote_ref(old, new, target).unwrap_or_else(|| target.clone());
                (
                    Some(RawRef::Symbolic(new_target)),
                    Expected::Symref(target.clone()),
                    true,
                )
            }
        };
        let tip = entry.peeled.or(match &entry.value {
            RawRef::Direct(oid) => Some(*oid),
            RawRef::Symbolic(_) => None,
        });

        updates.push(RefUpdate {
            name: new_name.clone(),
            new_value,
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref: symref,
                ..RefUpdateFlags::default()
            },
        });
        updates.push(RefUpdate {
            name: entry.name.clone(),
            new_value: None,
            expected: delete_expected,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref: symref,
                ..RefUpdateFlags::default()
            },
        });

        reflog_renames.push(RefReflogRename {
            from: entry.name.clone(),
            to: new_name.clone(),
            append: ReflogUpdate {
                identity: identity.clone(),
                message: format!("remote: renamed {} to {new_name}", entry.name),
                time,
                old_oid: tip,
                new_oid: tip,
            },
        });
    }

    commit_ref_batch_on_store(repo.refs(), &updates, &reflog_renames, &[])
        .map_err(RemoteError::Library)
}

fn plan_config_updates(
    config: &ConfigSet,
    old: &str,
    new: &str,
    refspecs_need_update: bool,
) -> Result<(Vec<String>, Vec<String>), RemoteError> {
    let refspec_marker = format!(":refs/remotes/{old}/");
    let mut refspecs_updated = Vec::new();
    if refspecs_need_update {
        for spec in config.get_all(&format!("remote.{old}.fetch")) {
            if spec.contains(&refspec_marker) {
                refspecs_updated.push(update_fetch_refspec(&spec, old, new));
            }
        }
    }

    let mut branches_updated = BTreeSet::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("branch.") else {
            continue;
        };
        let Some((branch, field)) = rest.rsplit_once('.') else {
            continue;
        };
        if (field.eq_ignore_ascii_case("remote") || field.eq_ignore_ascii_case("pushremote"))
            && entry.value.as_deref() == Some(old)
        {
            branches_updated.insert(branch.to_owned());
        }
    }

    Ok((refspecs_updated, branches_updated.into_iter().collect()))
}

fn update_fetch_refspec(spec: &str, old: &str, new: &str) -> String {
    let marker = format!(":refs/remotes/{old}/");
    let replacement = format!(":refs/remotes/{new}/");
    spec.replace(&marker, &replacement)
}

fn apply_remote_rename_config(
    repo: &Repository,
    old: &str,
    new: &str,
    refspecs_need_update: bool,
) -> Result<(), RemoteError> {
    let path = crate::repo::common_git_dir_for_config(repo.git_dir()).join("config");
    let content = std::fs::read_to_string(&path).map_err(crate::error::Error::Io)?;
    let mut file =
        ConfigFile::parse(&path, &content, ConfigScope::Local).map_err(RemoteError::Library)?;
    let old_section = format!("remote.{old}");
    let new_section = format!("remote.{new}");
    if !file.rename_section(&old_section, &new_section)? {
        return Err(RemoteError::Library(crate::error::Error::Message(format!(
            "could not rename config section {old_section} to {new_section}"
        ))));
    }

    if refspecs_need_update {
        let fetch_key = format!("remote.{new}.fetch");
        let specs = config_file_get_all(&file, &fetch_key);
        file.unset(&fetch_key)?;
        let refspec_marker = format!(":refs/remotes/{old}/");
        for spec in specs {
            let updated = if spec.contains(&refspec_marker) {
                update_fetch_refspec(&spec, old, new)
            } else {
                spec
            };
            file.add_value(&fetch_key, &updated)?;
        }
    }

    let keys: Vec<String> = file.entries.iter().map(|e| e.key.clone()).collect();
    for key in keys {
        let Some(rest) = key.strip_prefix("branch.") else {
            continue;
        };
        let Some((branch, field)) = rest.rsplit_once('.') else {
            continue;
        };
        if (field.eq_ignore_ascii_case("remote") || field.eq_ignore_ascii_case("pushremote"))
            && file.get(&key).as_deref() == Some(old)
        {
            file.set(&format!("branch.{branch}.{field}"), new)?;
        }
    }

    if file
        .get("remote.pushDefault")
        .as_deref()
        .is_some_and(|v| v == old)
    {
        file.set("remote.pushDefault", new)?;
    }

    file.write().map_err(RemoteError::Library)?;
    repo.reload_config().map_err(RemoteError::Library)?;
    Ok(())
}

fn config_file_get_all(file: &ConfigFile, key: &str) -> Vec<String> {
    let Ok(canon) = canonical_key(key) else {
        return Vec::new();
    };
    file.entries
        .iter()
        .filter(|e| e.key == canon)
        .map(|e| e.value.clone().unwrap_or_default())
        .collect()
}
