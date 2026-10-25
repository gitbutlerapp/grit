//! Keeping pushes off a served repository's target branch ([`TargetGuard`]).
//!
//! Once a repository has snapshot checkouts, its target branch changes only
//! through the server's own operations, [`super::integrate`] and
//! [`super::edit_slices`], so its history stays merges of checked-in work. A
//! plain push could replace that history, fast-forward it past review, or
//! delete the branch every checkout refreshes from, so a server runs
//! [`TargetGuard`] on every push to such a repository. A repository without
//! checkouts gets no guard and keeps Git's rules.

use crate::odb::Odb;
use crate::repo::Repository;
use crate::serve::{ReceiveHook, RefCommand, RefUpdateRefusal};

use super::{same_branch_name, target_branch, CheckoutStore};

/// A [`ReceiveHook`] that refuses every pushed create, update or delete of
/// one branch, the target, with [`RefUpdateRefusal::TargetBranch`].
///
/// Branches whose names differ from the target's only in case are refused
/// too: a case-insensitive file system keeps them in the target's file.
/// Other refs pass untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGuard {
    target: String,
}

impl TargetGuard {
    /// A guard for branch `target`.
    ///
    /// # Parameters
    ///
    /// - `target`: the branch name, without `refs/heads/`.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }

    /// The guard pushes to `repo` need: one for its [`target_branch`] once it
    /// has snapshot checkouts ([`CheckoutStore::has_checkouts`]), `None` for
    /// a repository without them.
    ///
    /// # Parameters
    ///
    /// - `repo`: the repository receiving the push.
    #[must_use]
    pub fn for_repository(repo: &Repository) -> Option<Self> {
        CheckoutStore::for_repository(repo)
            .has_checkouts()
            .then(|| Self::new(target_branch(repo)))
    }

    /// The guarded branch, without `refs/heads/`.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Whether a push to `refname` would change the guarded branch.
    ///
    /// # Parameters
    ///
    /// - `refname`: a full ref name, such as `refs/heads/main`.
    #[must_use]
    pub fn guards(&self, refname: &str) -> bool {
        refname
            .strip_prefix("refs/heads/")
            .is_some_and(|branch| same_branch_name(branch, &self.target))
    }
}

impl ReceiveHook for TargetGuard {
    fn update(&self, command: &RefCommand, _odb: &Odb) -> Result<(), RefUpdateRefusal> {
        if self.guards(&command.refname) {
            return Err(RefUpdateRefusal::TargetBranch {
                target: self.target.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::InitOptions;
    use crate::serve::{receive_pack_with_hook, ProtocolVersion, ReceivePolicy, ServeOptions};

    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn pkt(payload: &str) -> Vec<u8> {
        format!("{:04x}{payload}", payload.len() + 4).into_bytes()
    }

    /// A push of `commands` (`old new ref`) with no pack, so only deletes
    /// and moves to objects the repository has.
    fn push(
        repo: &Repository,
        commands: &[(Option<ObjectId>, Option<ObjectId>, &str)],
        caps: &str,
        hook: &dyn ReceiveHook,
    ) -> (Vec<Option<RefUpdateRefusal>>, String) {
        let hex = |o: Option<ObjectId>| o.map_or_else(|| ZERO.to_owned(), |o| o.to_hex());
        let mut body = Vec::new();
        for (i, (old, new, name)) in commands.iter().enumerate() {
            let caps = if i == 0 {
                format!("\0{caps}")
            } else {
                String::new()
            };
            body.extend(pkt(&format!("{} {} {name}{caps}\n", hex(*old), hex(*new))));
        }
        body.extend_from_slice(b"0000");
        if commands.iter().any(|(_, new, _)| new.is_some()) {
            body.extend_from_slice(&empty_pack());
        }
        let opts = ServeOptions {
            protocol: ProtocolVersion::V0,
            stateless_rpc: true,
            ..ServeOptions::default()
        };
        let mut output = Vec::new();
        let outcome = receive_pack_with_hook(
            repo,
            &mut std::io::Cursor::new(body),
            &mut output,
            &opts,
            &ReceivePolicy::default(),
            hook,
        )
        .expect("receive-pack");
        (
            outcome.updates.into_iter().map(|u| u.error).collect(),
            String::from_utf8_lossy(&output).into_owned(),
        )
    }

    /// A pack with no objects.
    fn empty_pack() -> Vec<u8> {
        use grit_test_support::objects::{HashAlgo as FixtureAlgo, PackBuilder};
        PackBuilder::new(FixtureAlgo::Sha1).build().bytes
    }

    fn bare_with_main() -> (tempfile::TempDir, Repository, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &InitOptions {
                bare: true,
                initial_branch: Some("main".to_owned()),
                ..InitOptions::default()
            },
        )
        .expect("init");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let commit = repo
            .odb()
            .write(
                ObjectKind::Commit,
                format!(
                    "tree {}\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\n\nbase\n",
                    tree.to_hex()
                )
                .as_bytes(),
            )
            .expect("commit");
        for name in ["refs/heads/main", "refs/heads/topic"] {
            crate::refs::write_ref(repo.git_dir(), name, &commit).expect("ref");
        }
        (dir, repo, commit)
    }

    #[test]
    fn only_repositories_with_checkouts_are_guarded() {
        let (_dir, repo, _) = bare_with_main();
        assert_eq!(TargetGuard::for_repository(&repo), None);
        std::fs::create_dir_all(CheckoutStore::for_repository(&repo).dir()).expect("mkdir");
        let guard = TargetGuard::for_repository(&repo).expect("guarded");
        assert_eq!(guard.target(), "main");
        for (refname, guarded) in [
            ("refs/heads/main", true),
            ("refs/heads/MAIN", true),
            ("refs/heads/Main", true),
            ("refs/heads/main2", false),
            ("refs/heads/topic/main", false),
            ("refs/tags/main", false),
            ("refs/checkouts/x/out/heads/main", false),
        ] {
            assert_eq!(guard.guards(refname), guarded, "{refname}");
        }
    }

    #[test]
    fn creating_updating_and_deleting_the_target_are_refused() {
        let (_dir, repo, commit) = bare_with_main();
        let guard = TargetGuard::new("main");
        let refused = Some(RefUpdateRefusal::TargetBranch {
            target: "main".to_owned(),
        });
        let tip = |name: &str| crate::refs::resolve_ref(repo.git_dir(), name).ok();

        // An update to the target and a new branch in one push: only the target is refused.
        let (errors, report) = push(
            &repo,
            &[
                (Some(commit), Some(commit), "refs/heads/main"),
                (None, Some(commit), "refs/heads/feature"),
            ],
            "report-status",
            &guard,
        );
        assert_eq!(errors, vec![refused.clone(), None]);
        assert!(
            report.contains(
                "ng refs/heads/main main is the target branch; it changes only through integrate"
            ),
            "{report}"
        );
        assert_eq!(tip("refs/heads/feature"), Some(commit));

        // A case variant would write the target's file on a case-insensitive
        // file system (where it already reads as the target).
        let (errors, _) = push(
            &repo,
            &[(tip("refs/heads/MAIN"), Some(commit), "refs/heads/MAIN")],
            "report-status",
            &guard,
        );
        assert_eq!(errors, vec![refused.clone()]);

        // Deleting it, even with HEAD elsewhere so Git's own rule doesn't apply.
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/topic").expect("head");
        let (errors, _) = push(
            &repo,
            &[(Some(commit), None, "refs/heads/main")],
            "report-status",
            &guard,
        );
        assert_eq!(errors, vec![refused.clone()]);
        assert_eq!(tip("refs/heads/main"), Some(commit));

        // In an atomic push, the refusal refuses everything.
        let (errors, _) = push(
            &repo,
            &[
                (None, Some(commit), "refs/heads/other"),
                (Some(commit), None, "refs/heads/main"),
            ],
            "report-status atomic",
            &guard,
        );
        assert_eq!(
            errors,
            vec![Some(RefUpdateRefusal::AtomicTransactionFailed), refused]
        );
        assert_eq!(tip("refs/heads/other"), None);
        assert_eq!(tip("refs/heads/main"), Some(commit));
    }

    #[test]
    fn deleting_a_bare_repositorys_current_branch_is_refused_as_in_git() {
        let (_dir, repo, commit) = bare_with_main();
        let (errors, _) = push(
            &repo,
            &[(Some(commit), None, "refs/heads/main")],
            "report-status",
            &(),
        );
        assert_eq!(errors, vec![Some(RefUpdateRefusal::CurrentBranchDeletion)]);
        let (errors, _) = push(
            &repo,
            &[(Some(commit), None, "refs/heads/topic")],
            "report-status",
            &(),
        );
        assert_eq!(errors, vec![None]);
    }

    #[test]
    fn a_hooks_message_reaches_side_band_clients() {
        struct Explains;
        impl ReceiveHook for Explains {
            fn update(&self, command: &RefCommand, _odb: &Odb) -> Result<(), RefUpdateRefusal> {
                TargetGuard::new("main").update(command, _odb)
            }
            fn message(&self, updates: &[crate::serve::RefUpdateResult]) -> Option<String> {
                updates
                    .iter()
                    .any(|u| u.error.is_some())
                    .then(|| "first line\nsecond line\n".to_owned())
            }
        }
        let (_dir, repo, commit) = bare_with_main();
        let push_main = [(Some(commit), Some(commit), "refs/heads/main")];
        let (_, plain) = push(&repo, &push_main, "report-status", &Explains);
        assert!(!plain.contains("first line"), "{plain}");
        let (_, banded) = push(&repo, &push_main, "report-status side-band-64k", &Explains);
        assert!(banded.contains("\u{2}first line\n"), "{banded:?}");
        assert!(banded.contains("\u{2}second line\n"), "{banded:?}");
        let message = banded.find("first line").unwrap_or(usize::MAX);
        let report = banded.find("ng refs/heads/main").unwrap_or(0);
        assert!(message < report, "the message comes before the report");
    }
}
