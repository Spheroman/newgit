//! `newgit pull --remote <target> --force` (#442): reset a tracker's line
//! to its export target, so the tracker follows a force-push there instead
//! of merging with it. Plain `pull --remote` merges, as `git pull` would,
//! which keeps what the force-push removed in the tracker's history for the
//! next export to publish again; when the force-push purged a secret, that
//! publishes it again. A reset replaces the line, as `git fetch` then `git
//! reset --hard <remote>/<branch>` would, and says what it drops.
//!
//! What it drops is written down here (`.newgit/local/resets.json`), and
//! this machine then refuses to carry a dropped commit onward from its
//! tracker's line: a merge whose result holds one, a checkpoint or merge
//! sending one to the remote, and a push to an export target of a commit
//! the target lacks that holds or pins one fail, naming it. A pin on the
//! old line with nothing of its own on it follows the reset on `newgit
//! pull`. A later reset onto a line holding commits an earlier one dropped
//! says so, and takes them off the record. What it cannot undo: the
//! project's own history, whose older root commits still pin the old line
//! (and so the remote's copy of it, its Git endpoint, `newgit clone`, and
//! project tags made from it), copies others already fetched, and another
//! machine, which has no record, merging them back.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    BranchManager, ExportReason, PROJECT_ROOT_INSTANCE, export_reason, local_tracker_ref,
    recorded_source, short,
};
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
    /// Commits the new line has that an earlier reset dropped, `(id,
    /// subject)`: taken back off the record, so this machine stops refusing
    /// them.
    pub restored: Vec<(String, String)>,
}

/// An instance whose branch of `tracker` holds `dropped` of the commits a
/// reset dropped, and `own` commits of its own besides. The project root
/// is listed only with work of its own, which the reset would discard, so
/// it refuses (`project_root`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldBack {
    pub instance: String,
    pub tracker: String,
    pub dropped: usize,
    pub own: usize,
    pub project_root: bool,
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
        let earlier = self.resets()?;
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
            instances.extend(self.held_back(name, after, &ids)?);
            let was_dropped: BTreeSet<&str> = earlier
                .iter()
                .filter(|reset| &reset.tracker == name)
                .flat_map(|reset| reset.dropped.iter().map(String::as_str))
                .collect();
            let restored = if was_dropped.is_empty() {
                Vec::new()
            } else {
                let mut range = vec![after.as_str()];
                if let Some(before) = before {
                    range.extend(["--not", before.as_str()]);
                }
                self.subjects(&range)?
                    .into_iter()
                    .filter(|(id, _)| was_dropped.contains(id.as_str()))
                    .collect()
            };
            lines.push(ResetLine {
                tracker: name.clone(),
                from: before.clone(),
                to: after.clone(),
                dropped,
                added,
                restored,
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
        if let Some(held) = outcome.instances.iter().find(|held| held.project_root) {
            return Err(self.project_root_has_work(&target, held, &moves)?);
        }

        // Written down before anything moves, so a reset that stops half
        // way still refuses what it dropped; taken back off when nothing
        // moved, so a reset that never happened refuses nothing.
        let project_branch = self
            .project_branch()
            .map(|branch| format!("refs/heads/{branch}"));
        let main_before = self
            .source
            .rev_parse(&local_tracker_ref(&tracker, "main"))
            .ok();
        let root_before = project_branch
            .as_deref()
            .and_then(|branch| self.source.rev_parse(branch).ok());
        self.write_resets(&Self::recorded(earlier.clone(), &target, &outcome.lines))?;
        let moved = (|| -> Result<Option<String>> {
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
                return Ok(None);
            }
            let message = format!(
                "Reset {tracker} to {} {}: drops {dropped} commit{}",
                recorded_source(&target.url),
                target.branch,
                if dropped == 1 { "" } else { "s" },
            );
            let moved = [(tracker.clone(), new_main.clone())];
            Ok(Some(self.commit_pins(
                &slug,
                None,
                &base,
                &moved,
                &message,
                definition.mount.as_str(),
                &mains,
                &moved,
            )?))
        })();
        match moved {
            Ok(view) => outcome.view = view,
            Err(error) => {
                let main_after = self
                    .source
                    .rev_parse(&local_tracker_ref(&tracker, "main"))
                    .ok();
                let root_after = project_branch
                    .as_deref()
                    .and_then(|branch| self.source.rev_parse(branch).ok());
                if main_after == main_before && root_after == root_before {
                    self.write_resets(&earlier)?;
                    return Err(error);
                }
                return Err(NewgitError::Unsupported(format!(
                    "{error}\n\nThe reset of `{}` to `{}` did not finish: part of it moved, and \
                     what it drops is written down, so this machine refuses those commits \
                     already. `newgit pull --remote {} --force` again finishes it",
                    crate::tracker::tracker_path(&tracker),
                    target.name,
                    target.name
                )));
            }
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
        self.subjects(&[old, "--not", new])
    }

    /// The commits `range` (`git log` arguments) lists, `(id, subject)`,
    /// newest first.
    fn subjects(&self, range: &[&str]) -> Result<Vec<(String, String)>> {
        let project = self.store.paths().project_root.clone();
        let mut args = vec!["-C", project.as_str(), "log", "--format=%H%x00%s"];
        args.extend_from_slice(range);
        let listed = crate::source::run_git_env(&args, &[])?;
        Ok(listed
            .lines()
            .filter_map(|line| line.split_once('\0'))
            .map(|(id, subject)| (id.to_owned(), subject.to_owned()))
            .collect())
    }

    /// Every instance whose branch of `tracker` holds any of `dropped`, and
    /// the project root when its branch has work of its own the new line
    /// lacks: the reset moves its pin, so that work would be discarded.
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
            let project_root = slug == PROJECT_ROOT_INSTANCE;
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
            let listed = if project_root {
                own > 0
            } else {
                dropped_here > 0
            };
            if listed {
                held.push(HeldBack {
                    instance: names.get(slug).cloned().unwrap_or_else(|| slug.to_owned()),
                    tracker: tracker.to_owned(),
                    dropped: dropped_here,
                    own,
                    project_root,
                });
            }
        }
        Ok(held)
    }

    /// Why a reset refuses while the project root has work of its own on
    /// the old line (`held`): moving its pin would discard it, as plain
    /// `newgit pull` refuses to move a pin holding work of its own.
    fn project_root_has_work(
        &self,
        target: &ExportTarget,
        held: &HeldBack,
        moves: &[(String, Option<String>, String)],
    ) -> Result<NewgitError> {
        let new = moves
            .iter()
            .find(|(name, _, _)| name == &held.tracker)
            .map(|(_, _, after)| after.clone())
            .unwrap_or_default();
        let branch =
            local_tracker_ref(&held.tracker, &format!("instances/{PROJECT_ROOT_INSTANCE}"));
        let main = moves
            .iter()
            .find(|(name, _, _)| name == &held.tracker)
            .and_then(|(_, before, _)| before.clone());
        let mut range = vec![branch.as_str(), "--not", new.as_str()];
        range.extend(main.as_deref());
        let own = self.subjects(&range)?;
        let shown: Vec<String> = own
            .iter()
            .take(5)
            .map(|(id, subject)| format!("{} {subject}", short(id)))
            .collect();
        let path = crate::tracker::tracker_path(&held.tracker);
        Ok(NewgitError::Unsupported(format!(
            "the project root has {} of its own on `{path}` that the target `{}` does not have ({}{}), and \
             resetting moves the project root's pin off them, which would discard that work; \
             nothing moved. Merge it first (`newgit merge main`), so the reset lists it among \
             the commits it drops and you can redo what should stay in an instance, or take it \
             off the project root's branch yourself; then reset again",
            if held.own == 1 {
                "a commit".to_owned()
            } else {
                format!("{} commits", held.own)
            },
            target.name,
            shown.join("; "),
            if own.len() > shown.len() { "; …" } else { "" },
        )))
    }

    /// Whether this machine has a reset written down at all.
    pub(super) fn has_resets(&self) -> bool {
        self.resets_file().exists()
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

    /// `resets` with this reset's `lines` written down: what each dropped,
    /// and every commit an earlier reset dropped that a new line has again
    /// taken back off (a record left with nothing dropped goes).
    fn recorded(
        mut resets: Vec<ResetRecord>,
        target: &ExportTarget,
        lines: &[ResetLine],
    ) -> Vec<ResetRecord> {
        for line in lines.iter().filter(|line| !line.restored.is_empty()) {
            let restored: BTreeSet<&str> =
                line.restored.iter().map(|(id, _)| id.as_str()).collect();
            for reset in resets
                .iter_mut()
                .filter(|reset| reset.tracker == line.tracker)
            {
                reset.dropped.retain(|id| !restored.contains(id.as_str()));
            }
        }
        resets.retain(|reset| !reset.dropped.is_empty());
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
        resets
    }

    fn write_resets(&self, resets: &[ResetRecord]) -> Result<()> {
        let path = self.resets_file();
        if resets.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(NewgitError::Io {
                        path: path.clone(),
                        source: error,
                    })
                }
                _ => Ok(()),
            };
        }
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

    /// Every commit a reset dropped, by tracker, with the target whose
    /// reset dropped it.
    fn dropped_by_tracker(&self) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let mut dropped: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for reset in self.resets()? {
            let ids = dropped.entry(reset.tracker.clone()).or_default();
            for id in reset.dropped {
                ids.insert(id, reset.target.clone());
            }
        }
        Ok(dropped)
    }

    /// Refuse pushing `tip` to `target`, which has `has` already, when a
    /// commit of it the target lacks stands for a root commit pinning a
    /// commit a reset dropped, of a tracker the target ships: compose
    /// expands the pin into that commit's files, so the push would publish
    /// them there. The project's own history still pins the old line (newgit
    /// does not rewrite it), so a target that never had that history, and
    /// ships the tracker, cannot take it.
    pub(super) fn refuse_dropped_pins(
        &self,
        target: &ExportTarget,
        tip: &str,
        has: &[String],
    ) -> Result<()> {
        if !self.resets_file().exists() {
            return Ok(());
        }
        let dropped = self.dropped_by_tracker()?;
        if dropped.is_empty() {
            return Ok(());
        }
        let shipped: BTreeSet<String> = self
            .carved_definitions()
            .iter()
            .filter(|definition| export_reason(target, definition) != ExportReason::Withheld)
            .map(|definition| definition.name.clone())
            .collect();
        if !dropped.keys().any(|tracker| shipped.contains(tracker)) {
            return Ok(());
        }
        let project = self.store.paths().project_root.clone();
        let mut args = vec!["-C", project.as_str(), "rev-list", tip];
        if !has.is_empty() {
            args.push("--not");
            args.extend(has.iter().map(String::as_str));
        }
        let new = crate::source::run_git_env(&args, &[])?;
        // Which root commit each view commit stands for, noted by compose.
        // Fails closed: a commit this cannot identify is one it cannot
        // clear, so a missing or unreadable note refuses the push.
        let listed = crate::source::run_git_env(
            &[
                "-C",
                project.as_str(),
                "notes",
                "--ref",
                crate::compose::NOTES_REF,
                "list",
            ],
            &[],
        )
        .map_err(|error| {
            NewgitError::SplitRefused(format!(
                "the export to `{}` is checked for commits a reset dropped (#442), by the \
                 root commit each of its commits stands for, and the notes saying which \
                 ({}) could not be read, so nothing was pushed: {error}",
                target.name,
                crate::compose::NOTES_REF,
            ))
        })?;
        let notes: BTreeMap<&str, &str> = listed
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(note, view)| (view.trim(), note.trim()))
            .collect();
        let unidentified = |view: &str, why: &str| {
            NewgitError::SplitRefused(format!(
                "the export to `{}` would carry commit {}, which {why}, so whether it pins a \
                 commit a reset dropped (#442) cannot be checked; nothing was pushed. Every \
                 commit an export composes here is noted, so this one was not composed here \
                 or its note was lost",
                target.name,
                short(view),
            ))
        };
        let mut objects = crate::objects::Objects::open(&project)?;
        for view in new.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let Some(note) = notes.get(view) else {
                return Err(unidentified(
                    view,
                    &format!(
                        "has no note ({}) saying which root commit it stands for",
                        crate::compose::NOTES_REF
                    ),
                ));
            };
            let text = String::from_utf8_lossy(&objects.blob(note)?).into_owned();
            let Some((root, commit)) = text.lines().find_map(|line| line.split_once(' ')) else {
                return Err(unidentified(view, "has a note that names no root commit"));
            };
            let state = crate::compose::index_with(&mut objects, root, commit.trim())?;
            for (tracker, pinned) in &state {
                if !shipped.contains(tracker) {
                    continue;
                }
                let Some(reset) = dropped.get(tracker).and_then(|ids| ids.get(pinned)) else {
                    continue;
                };
                let path = crate::tracker::tracker_path(tracker);
                return Err(NewgitError::SplitRefused(format!(
                    "the export to `{}` would carry commit {}, which holds `{path}` at {}, a \
                     commit resetting `{path}`'s line to `{reset}` dropped. `{}` does not have \
                     that commit, so this would publish what the reset dropped there; nothing \
                     was pushed. The commit is `{}` {}, and the project's own history still \
                     pins the old line (newgit does not rewrite it), so a target that ships \
                     `{path}` and has never had that history cannot take it as it is",
                    target.name,
                    short(view),
                    short(pinned),
                    target.name,
                    crate::tracker::tracker_path(root),
                    short(commit.trim()),
                )));
            }
        }
        Ok(())
    }

    /// Refuse forwarding a workspace's push to `origin` when a commit it
    /// sends that `origin` lacks (`rev_list`, in the outbox `repo`, which
    /// mirrors `origin`) is one a reset dropped: the forward is a raw push,
    /// so it would publish it there as it is.
    pub(super) fn refuse_forwarding_dropped(
        &self,
        repo: &camino::Utf8Path,
        rev_list: &[&str],
        env: &[(String, String)],
    ) -> Result<()> {
        if rev_list.is_empty() || !self.resets_file().exists() {
            return Ok(());
        }
        let dropped: BTreeMap<String, (String, String)> = self
            .dropped_by_tracker()?
            .into_iter()
            .flat_map(|(tracker, ids)| {
                ids.into_iter()
                    .map(move |(id, target)| (id, (tracker.clone(), target)))
            })
            .collect();
        if dropped.is_empty() {
            return Ok(());
        }
        let mut args = vec!["-C", repo.as_str(), "rev-list"];
        args.extend_from_slice(rev_list);
        let new = crate::source::run_git_env(&args, env)?;
        let found: Vec<&str> = new
            .lines()
            .map(str::trim)
            .filter(|id| dropped.contains_key(*id))
            .collect();
        let Some((tracker, target)) = found.first().and_then(|id| dropped.get(*id)) else {
            return Ok(());
        };
        let shown: Vec<&str> = found.iter().take(5).map(|id| short(id)).collect();
        let path = crate::tracker::tracker_path(tracker);
        Err(NewgitError::Unsupported(format!(
            "this push holds {} that resetting `{path}`'s line to `{target}` dropped ({}{}), \
             and the store's `origin` does not have {}, so forwarding it would publish what the \
             reset dropped; nothing was pushed. Push a branch that does not hold {}: one made \
             from `main` after the reset (`newgit spawn`), with the work redone on it",
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
            if found.len() == 1 { "it" } else { "them" },
        )))
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
    /// reset dropped (#442) in its tracker's own history: `doing` would
    /// carry it onward again. What is refused is said, with the way on.
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
                 so {doing} would carry {} onward again; nothing moved. An instance with no work \
                 of its own on `{path}` follows the reset with `newgit pull {path}`; one with \
                 work of its own has it on the old line, so redo that work in a new instance \
                 (`newgit spawn`) and remove this one. A reset not published yet is published \
                 by `newgit merge main`, before anything else merges",
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

    /// Refuse pulling from export target `target` when what the pull makes
    /// of a tracker's line (`commits`, `(tracker, commit)`) holds a commit a
    /// reset dropped: `target` still has it (a second target, never
    /// purged), and merging it in would bring it back for the next export
    /// to publish again. Names both ways on: follow `target` instead, which
    /// brings them back out loud, or purge them there too.
    pub(super) fn refuse_pulling_dropped(
        &self,
        target: &ExportTarget,
        commits: &[(&str, &str)],
    ) -> Result<()> {
        if !self.resets_file().exists() {
            return Ok(());
        }
        for (tracker, commit) in commits {
            let found = self.dropped_in(tracker, commit)?;
            let Some((_, reset_target)) = found.first() else {
                continue;
            };
            let shown: Vec<&str> = found.iter().take(5).map(|(id, _)| short(id)).collect();
            let path = crate::tracker::tracker_path(tracker);
            let follow = if target.root.is_some() {
                let name = &target.name;
                format!(
                    "If they are not a secret and `{name}` is the line to keep, `newgit pull \
                     --remote {name} --force --dry-run` says what following it does, and \
                     without `--dry-run` resets `{path}`'s line to `{name}`'s, taking them \
                     back off the record out loud. "
                )
            } else {
                String::new()
            };
            return Err(NewgitError::Unsupported(format!(
                "`{name}` still has {} that resetting `{path}`'s line to `{reset_target}` \
                 dropped ({}{}), so pulling from it would merge {} back in, for the next export \
                 to publish again; nothing moved. {follow}If they are a secret, purge them from \
                 `{name}` as well (rewrite its branch without them and force-push it), then \
                 pull again",
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
                name = target.name,
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
