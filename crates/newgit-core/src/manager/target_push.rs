//! The one way anything reaches an export target (#442).
//!
//! Every `git push` to a target — `main` (`newgit export --remote`, its
//! `--dry-run`, and the export a push from the project root makes), an
//! instance's branch (`branches = true`, from a workspace's `git push` or
//! `newgit export <instance> --remote`), and a tag pushed from the project
//! root — is a [`Cleared`], and only [`BranchManager::clear_push`] makes
//! one. Its fields are private to this module, and
//! [`BranchManager::push_cleared`] takes nothing else, so a push that skips
//! the checks does not compile.
//!
//! That is every push from a client. The remote's own export of a project
//! to its GitHub mirrors (`newgit-serve`'s `mirror/export.rs`, #414) is
//! another process, with no such record, and does not check yet: see
//! *Not yet* in `docs/design/serve.md`.
//!
//! The checks, each asked of the target at that moment:
//!
//! - **What a rewrite removed.** When the target's own branch no longer
//!   descends from what this machine last exported there (a force-push,
//!   perhaps to purge a secret), or is gone, nothing that holds a commit the
//!   rewrite removed is pushed there, under any ref. Only `newgit export
//!   --remote <target> --force --republish` may put them back. The record is
//!   this machine's (`refs/newgit/exports/<target>/pushed`): with none, or
//!   one older than another machine's export, a rewrite cannot be told from
//!   history of the target's own (#473).
//! - **What is new there** ([`BranchManager::refuse_new_outside_git`]): a
//!   commit the target lacks that adds a file a tracker outside Git owns, or
//!   pins a commit a reset dropped.
//!
//! A push of the target's own branch is leased on what the check saw there,
//! forced or not (onto it only once it is known to be behind), so a purge
//! landing between the check and the push is refused by the target rather
//! than fast-forwarded over. An instance's branch or a tag pushed onto what
//! is there leases nothing: the purge rewrites `main`, which neither push
//! moves, so a purge landing in that window is not seen. That gap is open.
//!
//! A workspace's `git push` is not an export: it is forwarded to the
//! store's `origin` as it is (`receive_push`), so an export target may not
//! be `origin` itself (`export_targets` refuses that, and the forward checks
//! again), and the forward refuses a commit a reset dropped.

use std::collections::BTreeSet;

use super::{BranchManager, PushExport, export_ref, plural_commits, short};
use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;

/// What a push to a target sends, which says the ref it moves and how a
/// refusal of it reads.
#[derive(Debug, Clone, Copy)]
pub(super) enum Pushing<'a> {
    /// The target's own branch. `from_push`: the export a `git push` from
    /// the project root makes.
    Main { from_push: bool },
    /// Instance `branch`'s branch, on a `branches = true` target.
    Branch(&'a BranchInstance),
    /// Tag `name`: `object` (the tag object, or the tagged commit itself
    /// when it is lightweight) at `refs/tags/<name>`.
    Tag { name: &'a str, object: &'a str },
}

/// Whether a push may put back commits a rewrite of the target removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PutBack {
    /// Never: the push is refused.
    Never,
    /// `newgit export --remote <target> --force --republish`, the one push
    /// that may, asked for by name.
    Republish,
    /// A `--force --dry-run`: counted and said, never pushed.
    Count,
}

/// A push to an export target that passed [`BranchManager::clear_push`].
/// Nothing outside this module can make one.
#[derive(Debug)]
pub(super) struct Cleared {
    url: String,
    refspec: String,
    /// `--force-with-lease` on the pushed ref at this (empty: it must not
    /// exist).
    lease: Option<(String, String)>,
    /// False for a dry run's check, which may count what it would put back.
    pushable: bool,
    /// What the target has at the pushed branch now (none: no such branch;
    /// always none for a tag).
    pub(super) theirs: Option<String>,
    /// How many commits a rewrite of the target removed this puts back.
    pub(super) restored: usize,
}

impl BranchManager {
    /// Check a push of `tip` to `target` and say how it goes: `how` it
    /// moves the ref, and whether it may put back what a rewrite there
    /// removed. A forced push is leased on what the target held when it was
    /// checked, so one rewritten again in between is refused rather than
    /// overwritten unseen.
    pub(super) fn clear_push(
        &self,
        target: &ExportTarget,
        tip: &str,
        pushing: Pushing<'_>,
        how: PushExport<'_>,
        put_back: PutBack,
    ) -> Result<Cleared> {
        let (branch, refname, object) = match pushing {
            Pushing::Main { .. } => (
                target.branch.clone(),
                format!("refs/heads/{}", target.branch),
                tip.to_owned(),
            ),
            Pushing::Branch(instance) => (
                instance.source_ref.clone(),
                format!("refs/heads/{}", instance.source_ref),
                tip.to_owned(),
            ),
            Pushing::Tag { name, object } => (
                target.branch.clone(),
                format!("refs/tags/{name}"),
                object.to_owned(),
            ),
        };
        let mut branches = vec![target.branch.as_str()];
        if branch != target.branch {
            branches.push(branch.as_str());
        }
        let has = self.target_has(target, &branches)?;
        let at = |wanted: &str| {
            has.iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, id)| id.clone())
        };
        let main_there = at(&target.branch);
        let theirs = match pushing {
            Pushing::Tag { .. } => None,
            _ => at(&branch),
        };

        // A dry run says what would be refused, too. First what is new
        // there, whose refusal names the file or the reset exactly.
        self.refuse_new_outside_git(target, tip, &has)?;

        let mut restored = 0;
        if let Some(pushed) = self.target_rewritten(target, main_there.as_deref())? {
            let held = self.removed_held(&pushed, main_there.as_deref(), tip)?;
            let replacing = matches!(pushing, Pushing::Main { .. })
                && matches!(how, PushExport::Force)
                && put_back != PutBack::Never;
            if held > 0 && !replacing {
                return Err(self.rewrite_refused(
                    target,
                    &pushed,
                    main_there.as_deref(),
                    held,
                    pushing,
                    how,
                )?);
            }
            restored = held;
        }

        // Onto the target's own branch, the push is leased on what it was
        // checked at, once that is known to be behind: a plain push would
        // also go through onto an ancestor it was reset to in between (a
        // purge), putting back what that removed unchecked. A dry run is
        // never pushed, and says what would be refused as it did.
        let behind = match (pushing, how, &theirs) {
            (Pushing::Main { .. }, PushExport::Onto, Some(theirs)) => {
                self.source.is_ancestor(theirs, tip)?
            }
            _ => true,
        };
        if !behind && put_back != PutBack::Count {
            return Err(NewgitError::Unsupported(format!(
                "{}; nothing was pushed",
                self.push_rejected(target, theirs.as_deref(), tip)?
            )));
        }
        let lease = behind
            .then(|| lease_for(pushing, how, &refname, theirs.as_deref()))
            .flatten();
        Ok(Cleared {
            url: target.push_url(),
            refspec: format!("{object}:{refname}"),
            lease,
            pushable: put_back != PutBack::Count,
            theirs,
            restored,
        })
    }

    /// Push what [`Self::clear_push`] cleared, with plain `git push` and the
    /// user's own credentials. `--no-verify`: the project's own pre-push
    /// hooks are for its work, not for a mirror of it.
    pub(super) fn push_cleared(&self, cleared: Cleared) -> Result<()> {
        if !cleared.pushable {
            return Err(NewgitError::Unsupported(
                "a dry run's check of an export is not a push".to_owned(),
            ));
        }
        let root = self.store.paths().project_root.clone();
        let args = push_args(root.as_str(), &cleared);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::source::run_git_env(&args, &[]).map(|_| ())
    }

    /// The commit this machine last exported to `target`'s branch, when the
    /// branch, at `theirs` now (none: the target no longer has it), does
    /// not descend from it: rewritten there since (a force-push, perhaps to
    /// purge a secret), or deleted. None without a record, which this
    /// machine cannot tell a rewrite without (#473).
    pub(super) fn target_rewritten(
        &self,
        target: &ExportTarget,
        theirs: Option<&str>,
    ) -> Result<Option<String>> {
        let Ok(pushed) = self.source.rev_parse(&export_ref(&target.name, "pushed")) else {
            return Ok(None);
        };
        match theirs {
            Some(theirs) if pushed == theirs || self.source.is_ancestor(&pushed, theirs)? => {
                Ok(None)
            }
            _ => Ok(Some(pushed)),
        }
    }

    /// How many of the commits `pushed` has that `theirs` lacks (what a
    /// rewrite from `pushed` to `theirs` removed; all of them when the
    /// branch is gone) `tip` holds.
    pub(super) fn removed_held(
        &self,
        pushed: &str,
        theirs: Option<&str>,
        tip: &str,
    ) -> Result<usize> {
        let root = self.store.paths().project_root.clone();
        let list = |from: &str| -> Result<BTreeSet<String>> {
            let mut args = vec!["-C", root.as_str(), "rev-list", from];
            if let Some(theirs) = theirs {
                args.extend(["--not", theirs]);
            }
            Ok(crate::source::run_git_env(&args, &[])?
                .lines()
                .map(|line| line.trim().to_owned())
                .filter(|line| !line.is_empty())
                .collect())
        };
        let removed = list(pushed)?;
        Ok(list(tip)?.intersection(&removed).count())
    }

    /// Why [`Self::clear_push`] refused a push holding `held` of the
    /// commits a rewrite of `target` (from `pushed` to `theirs`) removed.
    fn rewrite_refused(
        &self,
        target: &ExportTarget,
        pushed: &str,
        theirs: Option<&str>,
        held: usize,
        pushing: Pushing<'_>,
        how: PushExport<'_>,
    ) -> Result<NewgitError> {
        let name = &target.name;
        let refused = match (pushing, how) {
            (Pushing::Main { .. }, PushExport::Force) => {
                let what = match theirs {
                    Some(_) => "was rewritten since this machine last exported to it",
                    None => "is gone (deleted there) since this machine last exported to it",
                };
                return Ok(NewgitError::Unsupported(format!(
                    "`{branch}` on `{name}` {what}, and replacing it would put back {} that the \
                     rewrite removed (a force-push there, perhaps to purge a secret); nothing \
                     was pushed. If they are not a secret, `newgit export --remote {name} \
                     --force --republish` puts them back. Otherwise follow the rewrite instead: \
                     {}",
                    plural_commits(held),
                    self.follow_way(target),
                    branch = target.branch,
                )));
            }
            (Pushing::Main { from_push: true }, _) => "This push would put them back".to_owned(),
            (Pushing::Main { from_push: false }, _) => format!(
                "Exporting would put them back, and merging the target in with plain `newgit \
                 pull --remote {name}` would keep them for the next export to put back"
            ),
            (Pushing::Branch(instance), _) => format!(
                "`{}` holds {} of them, and exporting it there would put them back",
                instance.source_ref,
                plural_commits(held)
            ),
            (Pushing::Tag { name: tag, .. }, _) => format!(
                "Tag `{tag}` names a commit holding {} of them, and pushing it there would put \
                 them back",
                plural_commits(held)
            ),
        };
        let held = matches!(pushing, Pushing::Main { .. }).then_some(held);
        self.rewritten_error(target, pushed, theirs, held, &refused)
    }

    /// What a refusal says when `target`'s branch, at `theirs` (none: gone),
    /// was rewritten since this machine pushed `pushed` there. `held` is how
    /// many of the removed commits the export holds, which `export --force
    /// --republish` would put back (unknown when it was not composed).
    pub(super) fn rewritten_error(
        &self,
        target: &ExportTarget,
        pushed: &str,
        theirs: Option<&str>,
        held: Option<usize>,
        refused: &str,
    ) -> Result<NewgitError> {
        let removed = match theirs {
            Some(theirs) => self.source.commit_count(&format!("{theirs}..{pushed}"))?,
            None => self.source.commit_count(pushed)?,
        };
        let commits = if removed == 1 {
            "1 commit that export put there is".to_owned()
        } else {
            format!("{removed} commits that export put there are")
        };
        let put_back = match held {
            Some(held) => format!("putting back {}", plural_commits(held)),
            None => "putting them back".to_owned(),
        };
        let name = &target.name;
        let branch = &target.branch;
        let Some(_) = theirs else {
            // Nothing there to follow: a history without them is pushed
            // there first, or they are put back.
            let follow = match &target.root {
                Some(_) => format!(
                    "To publish a history without them, push it to `{branch}` on `{name}` \
                     yourself; `newgit pull --remote {name} --force` then follows it"
                ),
                None => self.follow_way(target),
            };
            return Ok(NewgitError::Unsupported(format!(
                "`{branch}` on `{name}` is gone: this machine last exported {} there, and the \
                 target no longer has that branch, so {commits} gone from it (deleted, perhaps \
                 to purge a secret). {refused}, so nothing moved. {follow}. If they are not a \
                 secret, `newgit export --remote {name} --force --republish` makes the branch \
                 again, {put_back}",
                short(pushed),
            )));
        };
        // Following is the way on for a rooted target; the project's own
        // view cannot be followed yet, so putting them back is named too.
        let republish = match &target.root {
            Some(_) => String::new(),
            None => format!(
                " If they are not a secret, `newgit export --remote {name} --force --republish` \
                 replaces the branch instead, {put_back}"
            ),
        };
        Ok(NewgitError::Unsupported(format!(
            "`{branch}` on `{name}` was rewritten since this machine last exported to it: it \
             no longer has {}, the commit pushed there, so {commits} gone from it (a \
             force-push there, perhaps to purge a secret). {refused}, so nothing moved. \
             {}.{republish}",
            short(pushed),
            self.follow_way(target),
        )))
    }
}

/// The lease a push of `refname` takes, at `theirs` (what the target had
/// there when it was checked; none: nothing). `Onto` a branch or tag has
/// none: neither can lease on the target's `main`, which a purge rewrites,
/// so a purge landing between the check and the push is not seen there.
fn lease_for(
    pushing: Pushing<'_>,
    how: PushExport<'_>,
    refname: &str,
    theirs: Option<&str>,
) -> Option<(String, String)> {
    match (pushing, how) {
        (Pushing::Main { .. }, PushExport::Onto) | (_, PushExport::Force) => {
            Some((refname.to_owned(), theirs.unwrap_or_default().to_owned()))
        }
        (_, PushExport::Replacing(held)) => Some((refname.to_owned(), held.to_owned())),
        (_, PushExport::Onto) => None,
    }
}

/// `git push`'s arguments for `cleared`, run in `root`.
fn push_args(root: &str, cleared: &Cleared) -> Vec<String> {
    let mut args: Vec<String> = ["-C", root, "push", "--quiet", "--no-verify"]
        .map(ToOwned::to_owned)
        .to_vec();
    args.extend(
        cleared
            .lease
            .as_ref()
            .map(|(refname, expected)| format!("--force-with-lease={refname}:{expected}")),
    );
    args.extend([cleared.url.clone(), cleared.refspec.clone()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleared(pushing: Pushing<'_>, how: PushExport<'_>, theirs: Option<&str>) -> Cleared {
        let refname = "refs/heads/main";
        Cleared {
            url: "https://example.com/a.git".to_owned(),
            refspec: format!("tip:{refname}"),
            lease: lease_for(pushing, how, refname, theirs),
            pushable: true,
            theirs: theirs.map(ToOwned::to_owned),
            restored: 0,
        }
    }

    #[test]
    fn an_export_onto_main_is_leased_on_what_it_was_checked_at() {
        let main = Pushing::Main { from_push: false };
        let args = push_args("/p", &cleared(main, PushExport::Onto, Some("abc")));
        assert!(
            args.contains(&"--force-with-lease=refs/heads/main:abc".to_owned()),
            "{args:?}"
        );
        // A branch the target lacks must still not exist when it lands.
        let args = push_args("/p", &cleared(main, PushExport::Onto, None));
        assert!(
            args.contains(&"--force-with-lease=refs/heads/main:".to_owned()),
            "{args:?}"
        );
        let tag = Pushing::Tag {
            name: "v1",
            object: "tip",
        };
        let args = push_args("/p", &cleared(tag, PushExport::Onto, None));
        assert!(
            !args.iter().any(|arg| arg.starts_with("--force-with-lease")),
            "{args:?}"
        );
    }
}
