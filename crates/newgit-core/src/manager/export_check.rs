//! What an export would newly publish (#444): the commits of a composed
//! view that its target does not have yet. Only those are checked for a file
//! a tracker outside Git owns, since what the target holds already was
//! published already: refusing it again blocks the export forever (one old
//! commit that added a file before a tracker outside Git claimed it) and
//! protects nothing. What counts as the target's is asked of the target at
//! that moment, never taken from a record here: a target force-pushed to
//! purge a secret no longer has it, so a stale record would publish it again.
//!
//! The same commits are checked for a commit a reset dropped (#442): a view
//! commit the target lacks that stands for a root commit pinning one would
//! publish its files there.

use super::{BranchManager, short};
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;

impl BranchManager {
    /// The commits `target` has now at `branches` (those it has of them),
    /// as `(branch, commit)`, fetched here when this store lacks one: one
    /// `ls-remote`, and a fetch only for what is new.
    pub(super) fn target_has(
        &self,
        target: &ExportTarget,
        branches: &[&str],
    ) -> Result<Vec<(String, String)>> {
        let root = self.store.paths().project_root.clone();
        let url = target.push_url();
        let refs: Vec<String> = branches
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .collect();
        let mut args = vec!["-C", root.as_str(), "ls-remote", "--", url.as_str()];
        args.extend(refs.iter().map(String::as_str));
        let listed = crate::source::run_git_env(&args, &[])?;
        let mut has = Vec::new();
        let mut fetch = Vec::new();
        for line in listed.lines() {
            let Some((id, name)) = line.split_once('\t') else {
                continue;
            };
            if !refs.iter().any(|wanted| wanted == name) {
                continue;
            }
            let here = crate::source::run_git_env(
                &[
                    "-C",
                    root.as_str(),
                    "cat-file",
                    "-e",
                    &format!("{id}^{{commit}}"),
                ],
                &[],
            )
            .is_ok();
            let branch = name.trim_start_matches("refs/heads/");
            if !here {
                fetch.push(format!(
                    "+{name}:refs/newgit/exports/{}/has/{branch}",
                    target.name
                ));
            }
            has.push((branch.to_owned(), id.to_owned()));
        }
        if !fetch.is_empty() {
            let mut args = vec![
                "-C",
                root.as_str(),
                "fetch",
                "--quiet",
                "--no-tags",
                "--",
                url.as_str(),
            ];
            args.extend(fetch.iter().map(String::as_str));
            crate::source::run_git_env(&args, &[])?;
        }
        Ok(has)
    }

    /// Refuse `tip`, about to be pushed to `target`, when a commit of it the
    /// target does not have adds a file a tracker outside Git owns, or pins
    /// a commit a reset dropped. `has` is what [`Self::target_has`] says the
    /// target has at its own branch and the pushed one: published already,
    /// so not judged again (#444). Only [`Self::clear_push`] asks this.
    pub(super) fn refuse_new_outside_git(
        &self,
        target: &ExportTarget,
        tip: &str,
        has: &[(String, String)],
    ) -> Result<()> {
        let has: Vec<String> = has.iter().map(|(_, id)| id.clone()).collect();
        self.refuse_dropped_pins(target, tip, &has)?;
        let mut rev_list = vec![tip];
        if !has.is_empty() {
            rev_list.push("--not");
            rev_list.extend(has.iter().map(String::as_str));
        }
        // A rooted view holds the tracker's files at its own root: each is
        // judged where the project's view has it, under the mount.
        let rooted = target.root.as_ref().and_then(|root| {
            self.carved
                .iter()
                .find(|definition| &definition.name == root)
                .map(|definition| (root.clone(), definition.mount.clone()))
        });
        let root = self.store.paths().project_root.clone();
        let all = self.all_definitions();
        let Some((commit, path, owner)) = crate::compose::find_outside_git_unpublished(
            &root,
            &rev_list,
            &all,
            rooted.as_ref().map(|(_, mount)| mount.as_path()),
        )?
        else {
            return Ok(());
        };
        let way_out = match &rooted {
            Some((tracker, mount)) => {
                let inside = path
                    .strip_prefix(&format!("{mount}/"))
                    .unwrap_or(path.as_str());
                format!(
                    "To export it, take `{path}` out of that history: rewrite the history `{}` \
                     should have without it (in a clone, `git filter-repo --invert-paths --path \
                     {inside}`), push that to `{}` yourself, and `newgit pull --remote {} \
                     --force` resets `{}`'s line to it, saying which commits that drops",
                    target.name,
                    target.name,
                    target.name,
                    crate::tracker::tracker_path(tracker)
                )
            }
            None => "To export it, the project's history has to be rewritten without it, which \
                     newgit cannot follow yet: `newgit pull --remote --force` follows a rewrite \
                     only on a target rooted at a tracker"
                .to_owned(),
        };
        Err(NewgitError::SplitRefused(format!(
            "the export to `{}` would carry commit {}, which adds `{path}`, and `{path}` belongs \
             to `{owner}`, a tracker outside Git, whose files (secrets among them) never leave \
             this machine through Git. `{}` does not have that commit, so this would publish \
             `{path}` there for the first time; nothing was pushed. (What `{}` has already is \
             published, so it is not checked again.) {way_out}. If `{path}` is not a secret, \
             take it out of `{owner}`'s paths instead",
            target.name,
            short(&commit),
            target.name,
            target.name,
        )))
    }
}
