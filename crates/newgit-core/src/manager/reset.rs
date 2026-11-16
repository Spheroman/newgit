//! `newgit pull --remote <target> --force` (#442): reset a tracker's line
//! to its export target, so the tracker follows a force-push there instead
//! of merging with it. Plain `pull --remote` merges, as `git pull` would,
//! which keeps what the force-push removed in the tracker's history for the
//! next export to publish again; when the force-push purged a secret, that
//! publishes it again. A reset replaces the line, as `git fetch` then `git
//! reset --hard <remote>/<branch>` would, and says what it drops.
//!
//! What it drops is written down here (`.newgit/local/resets.json`), and
//! this machine then refuses to bring any of it back: a merge, a send to
//! the remote, or an export holding a dropped commit fails, naming it. A
//! pin on the old line with nothing of its own on it follows the reset on
//! `newgit pull`. What it cannot undo: copies others already fetched, and
//! another machine, which has no record, merging them back.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{BranchManager, PROJECT_ROOT_INSTANCE, local_tracker_ref, recorded_source, short};
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;

/// One reset, written down: `tracker`'s line went from `from` to `to`, and
/// `dropped` are the commits only the old line had.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResetRecord {
    tracker: String,
    target: String,
    from: String,
    to: String,
    dropped: Vec<String>,
    at: String,
}

/// What `newgit pull --remote <target> --force` did, or would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    pub target: String,
    pub url: String,
    pub branch: String,
    /// The tracker the target is rooted at.
    pub tracker: String,
    /// Each tracker whose line moved: the one the target is rooted at, and
    /// any mounted inside it whose commits the target's history split to.
    pub lines: Vec<ResetLine>,
    /// Instances whose branches (as of their last checkpoint) still hold
    /// dropped commits.
    pub instances: Vec<HeldBack>,
    /// The project root's commit moving the pin; none when it had it.
    pub view: Option<String>,
    pub dry_run: bool,
}

/// One tracker's line, reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetLine {
    pub tracker: String,
    /// Its `main` before; none when it had none.
    pub from: Option<String>,
    pub to: String,
    /// The commits only the old line had: `(id, subject)`, newest first.
    pub dropped: Vec<(String, String)>,
    /// How many commits the new line has that the old one did not.
    pub added: usize,
}

/// An instance whose branch of `tracker` holds `dropped` of the commits a
/// reset dropped, and `own` commits of its own besides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldBack {
    pub instance: String,
    pub tracker: String,
    pub dropped: usize,
    pub own: usize,
}

impl BranchManager {
    /// Reset the line of the tracker export target `name` is rooted at to
    /// what the target has: its `main` becomes the target's history (split,
    /// when it mounts trackers in Git), and the project root's pin moves to
    /// it in one commit, `newgit merge main` publishing both. Nothing is
    /// merged, so the commits only the old line had are dropped: listed,
    /// with every instance still holding them, and written down so this
    /// machine refuses to bring them back. `dry_run` moves nothing.
    pub fn reset_to_remote(&self, name: &str, dry_run: bool) -> Result<ResetOutcome> {
        self.require_migrated()?;
        let target = self.export_target(name)?;
        let Some(tracker) = target.root.clone() else {
            return Err(NewgitError::Unsupported(format!(
                "`{}` is the project's own view (root declares it), and following a force-push \
                 there means rewriting the project's history, every tracker's: newgit cannot do \
                 that yet. `--force` resets the line of a tracker a target is rooted at",
                target.name
            )));
        };
        self.prepare_projection()?;
        self.sync_project_root()?;
        self.need_history(&tracker)?;
        let theirs = self.fetch_target(&target)?.ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "`{}` has no branch `{}` to reset to",
                target.name, target.branch
            ))
        })?;
        let definition = self
            .carved
            .iter()
            .find(|definition| definition.name == tracker)
            .cloned()
            .ok_or_else(|| {
                NewgitError::Unsupported(format!("`{tracker}` is not a tracker in Git here"))
            })?;
        let ours = self.rooted_ours(&tracker)?;
        // What comes in is judged as a pull judges it: a history adding a
        // file a tracker outside Git owns is not taken in.
        let project = self.store.paths().project_root.clone();
        let has_children = self
            .carved
            .iter()
            .any(|child| child.parent.as_deref() == Some(tracker.as_str()));
        let seen = match (&ours, has_children) {
            (Some(ours), true) => Some(self.compose_rooted(&target, &tracker, ours)?),
            (Some(ours), false) => Some(ours.clone()),
            (None, _) => None,
        };
        let mut new_args = vec![theirs.as_str()];
        if let Some(seen) = seen.as_deref() {
            new_args.extend(["--not", seen]);
        }
        let all = self.all_definitions();
        if let Some((commit, path, owner)) = crate::compose::find_outside_git_under(
            &project,
            &new_args,
            &[],
            &all,
            Some(&definition.mount),
        )? {
            return Err(super::outside_git_on_target(
                &commit,
                &target.name,
                &path,
                &owner,
            ));
        }

        // The new line: the target's history, split when it holds children.
        let mut moves: Vec<(String, Option<String>, String)> = Vec::new();
        let new_main = if has_children {
            let outcome = self.split_rooted(&tracker, &theirs)?;
            for child in self.carved.iter().filter(|child| child.name != tracker) {
                if let Some(commit) = outcome.index.get(&child.name) {
                    let before = self
                        .source
                        .rev_parse(&local_tracker_ref(&child.name, "main"))
                        .ok();
                    if before.as_deref() != Some(commit.as_str()) {
                        moves.push((child.name.clone(), before, commit.clone()));
                    }
                }
            }
            outcome.root
        } else {
            theirs.clone()
        };
        moves.insert(0, (tracker.clone(), ours.clone(), new_main.clone()));

        // Each instance's branches as its workspace has them now, not as
        // of whenever they were last split.
        self.split_live_instances();
        let mut lines = Vec::new();
        let mut instances = Vec::new();
        for (name, before, after) in &moves {
            let dropped = match before {
                Some(before) => self.only_in(before, after)?,
                None => Vec::new(),
            };
            let added = match before {
                Some(before) => self.commits_between(before, after)?,
                None => crate::source::run_git_env(
                    &["-C", project.as_str(), "rev-list", "--count", after],
                    &[],
                )?
                .trim()
                .parse()
                .unwrap_or(0),
            };
            let ids: BTreeSet<&str> = dropped.iter().map(|(id, _)| id.as_str()).collect();
            if !ids.is_empty() {
                instances.extend(self.held_back(name, after, &ids)?);
            }
            lines.push(ResetLine {
                tracker: name.clone(),
                from: before.clone(),
                to: after.clone(),
                dropped,
                added,
            });
        }
        let mut outcome = ResetOutcome {
            target: target.name.clone(),
            url: target.push_url(),
            branch: target.branch.clone(),
            tracker: tracker.clone(),
            lines,
            instances,
            view: None,
            dry_run,
        };
        if dry_run {
            return Ok(outcome);
        }

        // Written down first: if anything after this fails, this machine
        // still refuses to bring the dropped commits back.
        self.record_resets(&target, &outcome.lines)?;
        let mains: Vec<(String, String)> = moves
            .iter()
            .map(|(name, _, after)| (name.clone(), after.clone()))
            .collect();
        let (slug, _, base) = self.pull_base(None)?;
        let (pins, _) = self.base_pins(&base)?;
        let dropped: usize = outcome.lines.iter().map(|line| line.dropped.len()).sum();
        if pins.get(&tracker) == Some(&new_main) {
            self.land_tracker_branches(
                mains
                    .iter()
                    .map(|(tracker, main)| (tracker.as_str(), main.as_str())),
                "main",
            )?;
        } else {
            let message = format!(
                "Reset {tracker} to {} {}: drops {dropped} commit{}",
                recorded_source(&target.url),
                target.branch,
                if dropped == 1 { "" } else { "s" },
            );
            let moved = [(tracker.clone(), new_main.clone())];
            outcome.view = Some(self.commit_pins(
                &slug,
                None,
                &base,
                &moved,
                &message,
                definition.mount.as_str(),
                &mains,
                &moved,
            )?);
        }
        Ok(outcome)
    }

    /// Split every live instance's workspace `HEAD` onto its tracker
    /// branches, as a pull or a merge from it would. One that cannot be read
    /// keeps the branches it had.
    fn split_live_instances(&self) {
        let project = self.store.paths().project_root.clone();
        for branch in self.store.load_branches().unwrap_or_default() {
            let read = (|| -> Result<()> {
                let head = crate::source::run_git_env(
                    &["-C", branch.workspace_path.as_str(), "rev-parse", "HEAD"],
                    &[],
                )?
                .trim()
                .to_owned();
                crate::source::GitSource::fetch_refs(
                    &["-C", project.as_str()],
                    branch.workspace_path.as_str(),
                    &[format!("+HEAD:refs/newgit/pull/{}", branch.slug)],
                    None,
                )?;
                self.split_instance(&branch.slug, &head)?;
                Ok(())
            })();
            if let Err(error) = read {
                eprintln!(
                    "newgit: could not read `{}`'s workspace, so what it holds is as of its \
                     last split: {error}",
                    branch.name
                );
            }
        }
    }

    /// The commits `old` has that `new` does not, `(id, subject)`.
    fn only_in(&self, old: &str, new: &str) -> Result<Vec<(String, String)>> {
        let project = self.store.paths().project_root.clone();
        let listed = crate::source::run_git_env(
            &[
                "-C",
                project.as_str(),
                "log",
                "--format=%H%x00%s",
                old,
                "--not",
                new,
            ],
            &[],
        )?;
        Ok(listed
            .lines()
            .filter_map(|line| line.split_once('\0'))
            .map(|(id, subject)| (id.to_owned(), subject.to_owned()))
            .collect())
    }

    /// Every instance (but the project root, whose branch the reset moves)
    /// whose branch of `tracker` holds any of `dropped`.
    fn held_back(
        &self,
        tracker: &str,
        new: &str,
        dropped: &BTreeSet<&str>,
    ) -> Result<Vec<HeldBack>> {
        let project = self.store.paths().project_root.clone();
        let prefix = local_tracker_ref(tracker, "instances/");
        let refs = crate::source::run_git_env(
            &[
                "-C",
                project.as_str(),
                "for-each-ref",
                "--format=%(refname)",
                &prefix,
            ],
            &[],
        )?;
        let names: BTreeMap<String, String> = self
            .store
            .load_branches()
            .unwrap_or_default()
            .into_iter()
            .map(|branch| (branch.slug, branch.name))
            .collect();
        let mut held = Vec::new();
        for reference in refs.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let slug = reference.trim_start_matches(&prefix);
            if slug == PROJECT_ROOT_INSTANCE {
                continue;
            }
            let theirs = crate::source::run_git_env(
                &["-C", project.as_str(), "rev-list", reference, "--not", new],
                &[],
            )?;
            let (mut dropped_here, mut own) = (0, 0);
            for commit in theirs
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                if dropped.contains(commit) {
                    dropped_here += 1;
                } else {
                    own += 1;
                }
            }
            if dropped_here > 0 {
                held.push(HeldBack {
                    instance: names.get(slug).cloned().unwrap_or_else(|| slug.to_owned()),
                    tracker: tracker.to_owned(),
                    dropped: dropped_here,
                    own,
                });
            }
        }
        Ok(held)
    }

    fn resets_file(&self) -> camino::Utf8PathBuf {
        self.store.paths().local.join("resets.json")
    }

    /// Every reset written down on this machine.
    fn resets(&self) -> Result<Vec<ResetRecord>> {
        let path = self.resets_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                NewgitError::Unsupported(format!(
                    "{path} is not a list of resets newgit can read ({error}); it records the \
                     commits `newgit pull --remote --force` dropped, so nothing is sent until \
                     it is fixed"
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(NewgitError::Io {
                path: path.clone(),
                source: error,
            }),
        }
    }

    fn record_resets(&self, target: &ExportTarget, lines: &[ResetLine]) -> Result<()> {
        let mut resets = self.resets()?;
        let at = chrono::Utc::now().to_rfc3339();
        for line in lines.iter().filter(|line| !line.dropped.is_empty()) {
            let Some(from) = &line.from else { continue };
            resets.push(ResetRecord {
                tracker: line.tracker.clone(),
                target: target.name.clone(),
                from: from.clone(),
                to: line.to.clone(),
                dropped: line.dropped.iter().map(|(id, _)| id.clone()).collect(),
                at: at.clone(),
            });
        }
        let path = self.resets_file();
        let text = serde_json::to_string_pretty(&resets).map_err(|error| {
            NewgitError::Unsupported(format!("could not write the resets: {error}"))
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| NewgitError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text).map_err(|source| NewgitError::Io {
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, &path).map_err(|source| NewgitError::Io {
            path: path.clone(),
            source,
        })
    }

    /// The commits a reset of `tracker` dropped that `commit` holds.
    fn dropped_in(&self, tracker: &str, commit: &str) -> Result<Vec<(String, String)>> {
        let mut found = Vec::new();
        for reset in self
            .resets()?
            .iter()
            .filter(|reset| reset.tracker == tracker)
        {
            if commit == reset.to {
                continue;
            }
            let dropped: BTreeSet<&str> = reset.dropped.iter().map(String::as_str).collect();
            let project = self.store.paths().project_root.clone();
            let new = crate::source::run_git_env(
                &[
                    "-C",
                    project.as_str(),
                    "rev-list",
                    commit,
                    "--not",
                    &reset.to,
                ],
                &[],
            )?;
            found.extend(
                new.lines()
                    .map(str::trim)
                    .filter(|id| dropped.contains(id))
                    .map(|id| (id.to_owned(), reset.target.clone())),
            );
        }
        Ok(found)
    }

    /// Refuse `commits` (`(tracker, commit)`) when one holds a commit a
    /// reset dropped (#442): `doing` would publish it again. What is
    /// refused is said, with the way on.
    pub(super) fn refuse_dropped(&self, commits: &[(&str, &str)], doing: &str) -> Result<()> {
        if !self.resets_file().exists() {
            return Ok(());
        }
        for (tracker, commit) in commits {
            let found = self.dropped_in(tracker, commit)?;
            let Some((_, target)) = found.first() else {
                continue;
            };
            let shown: Vec<&str> = found.iter().take(5).map(|(id, _)| short(id)).collect();
            let path = crate::tracker::tracker_path(tracker);
            return Err(NewgitError::Unsupported(format!(
                "`{path}` at {} holds {} that resetting its line to `{target}` dropped ({}{}), \
                 so {doing} would publish {} again; nothing moved. An instance with no work \
                 of its own on `{path}` follows the reset with `newgit pull {path}`; one with \
                 work of its own has it on the old line, so redo that work in a new instance \
                 (`newgit spawn`) and remove this one",
                short(commit),
                if found.len() == 1 {
                    "a commit".to_owned()
                } else {
                    format!("{} commits", found.len())
                },
                shown.join(", "),
                if found.len() > shown.len() {
                    ", …"
                } else {
                    ""
                },
                if found.len() == 1 { "it" } else { "them" },
            )));
        }
        Ok(())
    }

    /// Whether a reset moved `tracker`'s line off `current` onto a line
    /// `commit` is on: then `current` is the old line, to be replaced, not
    /// merged with.
    pub(super) fn reset_replaces(
        &self,
        tracker: &str,
        current: &str,
        commit: &str,
    ) -> Result<bool> {
        if !self.resets_file().exists() {
            return Ok(false);
        }
        for reset in self
            .resets()?
            .iter()
            .rev()
            .filter(|reset| reset.tracker == tracker)
        {
            let old = current == reset.from || self.source.is_ancestor(current, &reset.from)?;
            let new = commit == reset.to || self.source.is_ancestor(&reset.to, commit)?;
            if old && new {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether every commit `pin` has that `new` does not is one a reset of
    /// `tracker` dropped: a pin on the old line with nothing of its own on
    /// it, which may follow the reset.
    pub(super) fn only_dropped(&self, tracker: &str, pin: &str, new: &str) -> Result<bool> {
        if !self.resets_file().exists() || pin == new {
            return Ok(false);
        }
        let dropped: BTreeSet<String> = self
            .resets()?
            .into_iter()
            .filter(|reset| reset.tracker == tracker)
            .flat_map(|reset| reset.dropped)
            .collect();
        if dropped.is_empty() {
            return Ok(false);
        }
        let project = self.store.paths().project_root.clone();
        let theirs = crate::source::run_git_env(
            &["-C", project.as_str(), "rev-list", pin, "--not", new],
            &[],
        )?;
        let mut any = false;
        for commit in theirs
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if !dropped.contains(commit) {
                return Ok(false);
            }
            any = true;
        }
        Ok(any)
    }
}
