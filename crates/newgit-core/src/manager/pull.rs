//! `newgit pull [<tracker>...]` (#345, #426): the walk that brings the
//! mounted trackers in Git up to date, from root down, taking what is new on
//! each tracker's rooted export target first, and moving each child's pin by
//! a commit in the tracker that mounts it. `newgit pull --remote`, and the
//! commits and refs both write, stay in the manager.

use std::collections::{BTreeMap, BTreeSet};

use super::{BranchManager, PROJECT_ROOT_INSTANCE, PinMove, RootedPull, descends_from};
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;
use crate::tracker::TrackerDefinition;

impl BranchManager {
    /// `newgit pull [<tracker>...]` (#345, #426): bring the mounted trackers
    /// in Git up to date in `instance` (the project root when `None`): every
    /// one, at any depth, or only those `trackers` names (ids).
    ///
    /// For each: a tracker with a rooted export target first takes what is
    /// new there, as `newgit pull --remote <target>` would (a fast-forward,
    /// or a real merge with its `follow` ref); then its pin moves to that,
    /// or to its `follow` ref, when it fast-forwards the pin. The pin of a
    /// tracker mounted below another lives in that tracker's commits, so
    /// moving it is a commit in that tracker, whose own pin then moves in
    /// its parent, up to root: one commit per tracker whose pins moved, made
    /// bottom-up, each `Update trackers: t3code (14 commits), ui-kit (2
    /// commits)` naming what moved below it. Root's is the view commit, on
    /// this instance's branch. No tracker's `main` moves: every
    /// commit lands on the trackers' branches for this instance, and `newgit
    /// merge` lands them on `main`. Nothing is written when no pin moves.
    ///
    /// Each passed over and named, never half-moved, without stopping the
    /// rest: a pin holding commits its ref lacks (moving it would drop them),
    /// a tracker the remote will not let the caller read (403), one whose
    /// export target cannot be pulled from (unreachable, a conflict, a file
    /// of a tracker outside Git), and one mounted in a tracker this instance
    /// does not bind. Every one read refused is an error: then nothing is
    /// known to be up to date. Trackers outside Git have no pins and are
    /// left alone.
    pub fn pull(&self, instance: Option<&str>, trackers: &[String]) -> Result<PullOutcome> {
        self.require_migrated()?;
        let (slug, workspace, base) = self.pull_base(instance)?;
        let (_, index) = self.base_pins(&base)?;
        for tracker in trackers {
            if !index.contains_key(tracker)
                || !self
                    .carved
                    .iter()
                    .any(|definition| &definition.name == tracker)
            {
                let name = crate::tracker::tracker_path(tracker);
                // `newgit pull <tracker> <instance>`, as it was before the
                // instance became `--instance`.
                if self.store.find_branch(&name).is_ok() {
                    let mut command = vec!["newgit pull".to_owned()];
                    command.extend(
                        trackers
                            .iter()
                            .filter(|other| *other != tracker)
                            .map(|other| crate::tracker::tracker_path(other)),
                    );
                    command.push(format!("--instance {name}"));
                    return Err(NewgitError::Unsupported(format!(
                        "`{name}` is an instance: `{}`",
                        command.join(" ")
                    )));
                }
                return Err(NewgitError::Unsupported(format!(
                    "`{name}` is not a tracker in Git mounted here: `newgit pull` moves a mounted \
                     tracker's pin"
                )));
            }
        }
        let dropped = if slug == PROJECT_ROOT_INSTANCE {
            BTreeSet::new()
        } else {
            self.store
                .find_branch(&slug)
                .map(|branch| branch.dropped)
                .unwrap_or_default()
        };
        let mut run = PullRun {
            only: trackers.iter().cloned().collect(),
            dropped,
            targets: self
                .export_targets()?
                .into_iter()
                .filter(|target| target.root.is_some())
                .collect(),
            land: Vec::new(),
            outcome: PullOutcome {
                outside_git: if trackers.is_empty() {
                    self.trackers
                        .iter()
                        .map(|definition| definition.name.clone())
                        .collect()
                } else {
                    Vec::new()
                },
                ..PullOutcome::default()
            },
        };
        let root = self.root.name.clone();
        let moved = self.pull_below(&mut run, &root, &base)?;
        let mut outcome = run.outcome;
        // Every read refused (a token used in the wrong project, a member
        // removed): whether anything is behind is unknown, so say so
        // instead of reporting success.
        let read = outcome.pulled.len()
            + outcome.up_to_date.len()
            + outcome.diverged.len()
            + outcome.no_follow.len()
            + outcome.target_failed.len();
        if read == 0 && !outcome.unreadable.is_empty() {
            return Err(NewgitError::Remote {
                status: 403,
                message: format!(
                    "no mounted tracker could be read on the remote: {}",
                    outcome
                        .unreadable
                        .iter()
                        .map(|(tracker, reason)| format!("`{tracker}` ({reason})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        if moved.is_empty() {
            return Ok(outcome);
        }
        let message = pull_message(&outcome.pulled, None);
        let mounts: Vec<String> = outcome
            .pulled
            .iter()
            .filter_map(|pulled| {
                self.carved
                    .iter()
                    .find(|definition| definition.name == pulled.tracker)
                    .map(|definition| definition.mount.to_string())
            })
            .collect();
        let view = self.commit_pins(
            &slug,
            workspace.as_deref(),
            &base,
            &moved,
            &message,
            &mounts.join("`, `"),
            &[],
            &run.land,
        )?;
        outcome.view = Some(view);
        Ok(outcome)
    }

    /// Pull every tracker in Git that `tracker`'s commit `commit` pins, and
    /// those below them: the pins that moved, `(name in commit, new)`.
    fn pull_below(
        &self,
        run: &mut PullRun,
        tracker: &str,
        commit: &str,
    ) -> Result<Vec<(String, String)>> {
        let project = self.store.paths().project_root.clone();
        let pins = {
            let mut objects = crate::objects::Objects::open(&project)?;
            match objects.try_commit(commit)? {
                Some(found) => crate::compose::pins_in(&mut objects, &found.tree)?,
                None => BTreeMap::new(),
            }
        };
        let mut moved = Vec::new();
        for (name, pin) in pins {
            let id = crate::tracker::child_id(tracker, &name);
            let Some(definition) = self
                .carved
                .iter()
                .find(|definition| definition.name == id)
                .cloned()
            else {
                continue;
            };
            let new = self.pull_pinned(run, &definition, &pin)?;
            if new != pin {
                run.land.push((id, new.clone()));
                moved.push((name, new));
            }
        }
        Ok(moved)
    }

    /// Pull `definition`'s tracker, pinned at `pin`, and the trackers below
    /// it: its new commit, `pin` when nothing moved.
    fn pull_pinned(
        &self,
        run: &mut PullRun,
        definition: &TrackerDefinition,
        pin: &str,
    ) -> Result<String> {
        let tracker = definition.name.as_str();
        let mut start = pin.to_owned();
        let below = |run: &PullRun| -> Vec<String> {
            self.carved
                .iter()
                .filter(|child| child.name != tracker && descends_from(&child.name, tracker))
                .map(|child| child.name.clone())
                .filter(|child| run.only.is_empty() || run.only.contains(child))
                .collect()
        };
        if run.only.is_empty() || run.only.contains(tracker) {
            match self.pull_one(run, definition, pin)? {
                PullStep::Move(new) => start = new,
                PullStep::Stay => {}
                // Its export target ships what is under it too: none of
                // it moves, so nothing is half-moved. A tracker under it
                // that was named is listed with it.
                PullStep::Skip => {
                    if !run.only.is_empty() {
                        for child in below(run) {
                            run.outcome.with_parent.push((child, tracker.to_owned()));
                        }
                    }
                    return Ok(start);
                }
            }
        }
        // Its children's pins are its commits, and an instance that does
        // not bind it does not commit to it. Each is named when it was
        // asked for, or when it is behind: when the pull leaves it where it
        // would have moved.
        if run.dropped.contains(tracker) {
            let behind = if run.only.is_empty() {
                let mut behind = BTreeSet::new();
                self.behind_below(tracker, &start, &mut behind)?;
                behind
            } else {
                BTreeSet::new()
            };
            for child in below(run) {
                if run.only.contains(&child) || behind.contains(&child) {
                    run.outcome.unbound.push((child, tracker.to_owned()));
                }
            }
            return Ok(start);
        }
        let first = run.outcome.pulled.len();
        let moved = self.pull_below(run, tracker, &start)?;
        if moved.is_empty() {
            return Ok(start);
        }
        let message = pull_message(&run.outcome.pulled[first..], Some(tracker));
        let made = self.pin_commit(&start, &moved, &message)?;
        run.outcome
            .mounting
            .push((tracker.to_owned(), made.clone()));
        Ok(made)
    }

    /// Every tracker below `tracker`'s commit `commit`, at any depth, whose
    /// pin its `follow` ref would move forward: what a pull leaves behind
    /// when `tracker` is not bound. Its export targets are not fetched, and
    /// a tracker that cannot be read is not counted.
    fn behind_below(
        &self,
        tracker: &str,
        commit: &str,
        behind: &mut BTreeSet<String>,
    ) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let pins = {
            let mut objects = crate::objects::Objects::open(&project)?;
            match objects.try_commit(commit)? {
                Some(found) => crate::compose::pins_in(&mut objects, &found.tree)?,
                None => BTreeMap::new(),
            }
        };
        for (name, pin) in pins {
            let id = crate::tracker::child_id(tracker, &name);
            let Some(definition) = self.carved.iter().find(|definition| definition.name == id)
            else {
                continue;
            };
            if let Ok(Some(new)) = self.follow_commit(definition)
                && self.pin_move(&id, &pin, &new)? == PinMove::Forward
            {
                behind.insert(id.clone());
            }
            self.behind_below(&id, &pin, behind)?;
        }
        Ok(())
    }

    /// Where `definition`'s own pin, now `pin`, moves: what its rooted export
    /// targets add to its `follow` ref, when that fast-forwards the pin.
    /// Why it stays, or is skipped, is in the outcome.
    fn pull_one(
        &self,
        run: &mut PullRun,
        definition: &TrackerDefinition,
        pin: &str,
    ) -> Result<PullStep> {
        let tracker = definition.name.clone();
        // A tracker the remote will not let this caller read is passed
        // over, like a missing ref, so it does not stop the rest.
        let mut new = match self.follow_commit(definition) {
            Ok(Some(new)) => new,
            Ok(None) => {
                run.outcome
                    .no_follow
                    .push((tracker, definition.follow.clone()));
                return Ok(PullStep::Stay);
            }
            Err(NewgitError::Remote {
                status: 403,
                message,
            }) => {
                run.outcome.unreadable.push((tracker, message));
                return Ok(PullStep::Stay);
            }
            Err(error) => return Err(error),
        };
        let mut from = definition.follow.clone();
        let mut children: Vec<(String, String)> = Vec::new();
        let targets: Vec<ExportTarget> = run
            .targets
            .iter()
            .filter(|target| target.root.as_deref() == Some(tracker.as_str()))
            .cloned()
            .collect();
        for target in &targets {
            match self.pull_target(target, &tracker, pin, &new) {
                Ok(None) => {}
                Ok(Some(rooted)) => {
                    from = format!("{from} and {}", rooted.from);
                    new = rooted.main;
                    children.extend(rooted.children);
                }
                Err(error) => {
                    run.outcome.target_failed.push((
                        tracker,
                        target.name.clone(),
                        error.to_string(),
                    ));
                    return Ok(PullStep::Skip);
                }
            }
        }
        Ok(match self.pin_move(&tracker, pin, &new)? {
            PinMove::UpToDate => {
                run.outcome.up_to_date.push(tracker);
                PullStep::Stay
            }
            PinMove::Diverged => {
                run.outcome
                    .diverged
                    .push((tracker, definition.follow.clone()));
                PullStep::Stay
            }
            PinMove::Forward => {
                run.outcome.pulled.push(PulledTracker {
                    commits: self.commits_between(pin, &new)?,
                    tracker,
                    from,
                });
                run.land.extend(children);
                PullStep::Move(new)
            }
        })
    }

    /// What `tracker`'s rooted export target `target` adds to `ours` (its
    /// `follow` ref here), as `newgit pull --remote` would bring it into
    /// `main`: `None` when the target has nothing the pin or `ours` lack, or
    /// no branch yet. One fetch from the target. Moves nothing.
    fn pull_target(
        &self,
        target: &ExportTarget,
        tracker: &str,
        pin: &str,
        ours: &str,
    ) -> Result<Option<RootedPull>> {
        let Some(theirs) = self.fetch_target(target)? else {
            return Ok(None);
        };
        // Git answers "yes" truly even from a snapshot: only a "no" needs the
        // history.
        let has = |commit: &str, theirs: &str| -> Result<bool> {
            Ok(commit == theirs || self.source.is_ancestor(theirs, commit)?)
        };
        if has(pin, &theirs)? || has(ours, &theirs)? {
            return Ok(None);
        }
        self.need_history(tracker)?;
        let rooted = self.rooted_merge(target, tracker, &theirs, Some(ours))?;
        if rooted.main == ours || has(pin, &rooted.tip)? {
            return Ok(None);
        }
        Ok(Some(rooted))
    }
}

/// A tracker whose pin `newgit pull` moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledTracker {
    pub tracker: String,
    /// Where its commits came from: its `follow` ref, and its rooted
    /// export targets that had something new (`main and github.com/acme/ui
    /// main`).
    pub from: String,
    /// How many of the tracker's commits came in.
    pub commits: usize,
}

/// What `newgit pull` did (#345, #426).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullOutcome {
    /// The trackers whose pins moved, at any depth, by id, bottom-up.
    pub pulled: Vec<PulledTracker>,
    /// The one view commit moving them all; none when no pin moved.
    pub view: Option<String>,
    /// The commit made in each tracker that mounts one whose pin moved:
    /// `(tracker, commit)`, bottom-up. Root's is `view`.
    pub mounting: Vec<(String, String)>,
    /// Trackers whose pins already have their `follow` ref.
    pub up_to_date: Vec<String>,
    /// Trackers whose pins hold commits their `follow` ref (the second
    /// item) does not, left where they are: moving them would drop those
    /// commits.
    pub diverged: Vec<(String, String)>,
    /// Trackers with no `follow` ref (the second item) to pull.
    pub no_follow: Vec<(String, String)>,
    /// Trackers the remote refused to let this caller read (403), with its
    /// reason: passed over, not pulled.
    pub unreadable: Vec<(String, String)>,
    /// Trackers whose rooted export target (the second item) could not be
    /// pulled from, with why (unreachable, a conflict): their pins stay.
    pub target_failed: Vec<(String, String, String)>,
    /// Trackers mounted in a tracker this instance does not bind (the second
    /// item): their pins are its commits, so they stay. Each is listed when
    /// it was named, or when its `follow` ref is ahead of its pin.
    pub unbound: Vec<(String, String)>,
    /// Trackers named to pull that sit under one (the second item) whose
    /// export target could not be pulled from: they stay with it.
    pub with_parent: Vec<(String, String)>,
    /// Trackers outside Git: they have no pins, so `newgit pull` leaves
    /// them alone (`newgit tracker pull` updates one). Listed only when no
    /// tracker was named.
    pub outside_git: Vec<String>,
}

/// One `newgit pull` under way.
struct PullRun {
    /// The trackers named; every one when empty.
    only: BTreeSet<String>,
    /// The trackers this instance does not bind.
    dropped: BTreeSet<String>,
    /// Every export target rooted at a tracker.
    targets: Vec<ExportTarget>,
    /// Every tracker's new commit, at any depth: what lands on its branch
    /// for this instance.
    land: Vec<(String, String)>,
    outcome: PullOutcome,
}

/// What a pull does with one tracker's own pin.
enum PullStep {
    /// It moves here.
    Move(String),
    /// It stays; what is under it may still move.
    Stay,
    /// It, and everything under it, stays.
    Skip,
}

/// The message of a pull's commit in tracker `within` (root when `None`)
/// moving `pulled`, each named from there: `Update trackers: lib (2
/// commits), ui (1 commit)`.
fn pull_message(pulled: &[PulledTracker], within: Option<&str>) -> String {
    let name = |tracker: &str| {
        let path = crate::tracker::tracker_path(tracker);
        match within {
            Some(within) => path
                .strip_prefix(&format!("{}/", crate::tracker::tracker_path(within)))
                .map_or_else(|| path.clone(), ToOwned::to_owned),
            None => path,
        }
    };
    format!(
        "Update trackers: {}",
        pulled
            .iter()
            .map(|pulled| format!(
                "{} ({} commit{})",
                name(&pulled.tracker),
                pulled.commits,
                if pulled.commits == 1 { "" } else { "s" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
