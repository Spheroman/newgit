use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

use crate::branch::BranchInstance;
use crate::checkpoint::CheckpointLog;
use crate::error::{NewgitError, Result};
use crate::materializer::workspace_marker_path;
use crate::resource::Ownership;
use crate::store::{MetadataStore, read_subdirs_sorted};

/// What one cleanup pass did, or would do under `--dry-run`. Garbage
/// collection has to be reportable to be trustworthy: every list here is
/// something that disappeared (or would).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub dry_run: bool,
    /// Instances whose workspace was gone, so newgit finished the teardown.
    pub finalized: Vec<FinalizedInstance>,
    /// Workspace directories with no binding record at all.
    pub orphan_workspaces: Vec<Utf8PathBuf>,
    /// Dead PID files and state directories for instances that are gone.
    pub dead_state: Vec<Utf8PathBuf>,
    /// Checkpoint logs discarded because their instance is archived and the
    /// caller asked for it. Empty unless purging was requested.
    pub purged_checkpoints: Vec<PurgedCheckpoints>,
    pub pruned: Vec<PrunedRev>,
    /// Lane revs kept alive solely because a checkpoint still points at
    /// them — the constraint pruning must never violate, surfaced so the
    /// retained disk is explained rather than mysterious.
    pub pinned_by_checkpoints: usize,
    /// How many of those belong to instances that are already archived — the
    /// ones `--purge-archived` can release.
    pub pinned_by_archived: usize,
    pub warnings: Vec<String>,
}

impl CleanupOutcome {
    pub fn is_empty(&self) -> bool {
        self.finalized.is_empty()
            && self.orphan_workspaces.is_empty()
            && self.dead_state.is_empty()
            && self.purged_checkpoints.is_empty()
            && self.pruned.is_empty()
    }
}

/// Whether an operation that archives an instance also discards the
/// checkpoints it leaves behind. Keeping them is the default: a checkpoint
/// pins the lane revs its undo would need, and newgit never breaks an undo
/// on its own initiative. Purging says the undo will never be wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchivedCheckpoints {
    Keep,
    Purge,
}

/// One instance's discarded checkpoint history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgedCheckpoints {
    /// The instance's slug — its binding record is already archived, so this
    /// is the only name that still exists on disk.
    pub slug: String,
    pub checkpoints: usize,
    /// Store refs the checkpoints held (`refs/newgit/checkpoints/<slug>/*`).
    pub source_refs: usize,
    pub dir: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedInstance {
    pub name: String,
    pub workspace: Utf8PathBuf,
    pub hooks: Vec<HookOutcome>,
    /// Where the binding record was archived to; None under `--dry-run`.
    pub archived_record: Option<Utf8PathBuf>,
}

/// One resource's cleanup hook, and why it did or did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub resource: String,
    pub ownership: Ownership,
    pub detail: HookDetail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDetail {
    Ran {
        command: String,
        ok: bool,
        log: Utf8PathBuf,
    },
    /// Would have run, but this was a dry run.
    WouldRun(String),
    /// `project` or `user` ownership: shared beyond this instance.
    SkippedOwnership,
    /// No `[cleanup] command` defined.
    NoHook,
    /// The hook's command still had an unresolved `{{...}}`, so running it
    /// would have passed a literal placeholder to a destructive command.
    SkippedUnresolved {
        command: String,
        placeholder: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedRev {
    pub tracker: String,
    pub rev: String,
    pub path: Utf8PathBuf,
}

/// A tracker lane rev on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRev {
    pub tracker: String,
    pub rev: String,
    pub path: Utf8PathBuf,
    /// Staging directory a crashed capture left behind (`<rev>.tmp`).
    pub is_staging: bool,
}

/// Whether per-branch teardown may run this resource's cleanup hook at all.
/// Ownership decides, not the presence of a command: a `user`-owned pnpm
/// store with a cleanup command must still survive `newgit remove`.
pub fn may_tear_down(ownership: Ownership) -> bool {
    ownership.per_branch_teardown_may_touch()
}

/// Everything that keeps a lane rev alive, kept apart by where the claim
/// came from so cleanup can explain retained disk instead of just retaining
/// it.
///
/// Checkpoints are roots even for instances whose binding record has been
/// archived. A checkpoint pointing at a pruned rev is not a smaller store,
/// it is a broken undo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotRoots {
    /// Claimed by a surviving instance's tracker binding.
    pub bindings: BTreeSet<(String, String)>,
    /// Claimed by any checkpoint record, live or archived.
    pub checkpoints: BTreeSet<(String, String)>,
    /// The subset of `checkpoints` claimed only by instances whose binding
    /// record is gone. These are the claims a purge can release, so cleanup
    /// can say so instead of reporting retained disk with no way out.
    pub archived_checkpoints: BTreeSet<(String, String)>,
    /// Claimed by a lane's own head (`LATEST`), which new instances project.
    pub lane_heads: BTreeSet<(String, String)>,
}

impl SnapshotRoots {
    /// `branches` is the set of instances that survive the cleanup pass, not
    /// everything on disk — a record about to be archived must not keep its
    /// unreferenced captures alive.
    ///
    /// `archived` says what to do with the checkpoint logs of instances that
    /// have no surviving record: `Keep` (the default) treats them as roots
    /// like any other checkpoint, `Purge` ignores them, because the caller is
    /// discarding them in the same pass — which is what makes a purging dry
    /// run report exactly what the real run would remove.
    pub fn collect(
        store: &MetadataStore,
        branches: &[BranchInstance],
        archived: ArchivedCheckpoints,
    ) -> Result<Self> {
        let mut roots = Self::default();

        for branch in branches {
            for (tracker, binding) in &branch.trackers {
                if let Some(rev) = &binding.content_rev {
                    roots.bindings.insert((tracker.clone(), rev.clone()));
                }
            }
        }

        let live_slugs: BTreeSet<&str> =
            branches.iter().map(|branch| branch.slug.as_str()).collect();
        let mut live_claims: BTreeSet<(String, String)> = BTreeSet::new();
        let mut archived_claims: BTreeSet<(String, String)> = BTreeSet::new();
        for slug in store.checkpointed_slugs()? {
            let claims = if live_slugs.contains(slug.as_str()) {
                &mut live_claims
            } else if archived == ArchivedCheckpoints::Purge {
                continue;
            } else {
                &mut archived_claims
            };
            let log = CheckpointLog::new(store.checkpoint_dir(&slug), &slug);
            for record in log.list()? {
                for state in &record.tracker_states {
                    if let Some(rev) = &state.content_rev {
                        claims.insert((state.name.clone(), rev.clone()));
                    }
                }
                for state in &record.resource_states {
                    if let Some((tracker, rev)) =
                        state.state_ref.as_deref().and_then(parse_tracker_state_ref)
                    {
                        claims.insert((tracker.to_owned(), rev.to_owned()));
                    }
                }
            }
        }
        // A rev a live instance's checkpoint also claims is not something a
        // purge could release, so it does not count as archived-held.
        roots.archived_checkpoints = archived_claims.difference(&live_claims).cloned().collect();
        roots.checkpoints = live_claims.union(&archived_claims).cloned().collect();

        for lane in lane_names(&store.paths().snapshots)? {
            if let Some(head) =
                crate::lane::TrackerLane::new(&store.paths().snapshots, &lane).latest()
            {
                roots.lane_heads.insert((lane, head));
            }
        }

        Ok(roots)
    }

    pub fn contains(&self, tracker: &str, rev: &str) -> bool {
        let key = (tracker.to_owned(), rev.to_owned());
        self.bindings.contains(&key)
            || self.checkpoints.contains(&key)
            || self.lane_heads.contains(&key)
    }

    /// Revs held only because some checkpoint references them — the disk a
    /// user might otherwise expect cleanup to have reclaimed.
    pub fn pinned_only_by_checkpoints(&self) -> impl Iterator<Item = &(String, String)> {
        self.checkpoints
            .iter()
            .filter(|key| !self.bindings.contains(*key) && !self.lane_heads.contains(*key))
    }

    /// Of those, the ones no live instance's checkpoint also claims: purging
    /// the archived logs would release exactly these.
    pub fn pinned_only_by_archived_checkpoints(&self) -> impl Iterator<Item = &(String, String)> {
        self.pinned_only_by_checkpoints()
            .filter(|key| self.archived_checkpoints.contains(*key))
    }
}

/// `tracker:<name>@<rev>` — the state ref an `into_tracker` deposit records.
fn parse_tracker_state_ref(state_ref: &str) -> Option<(&str, &str)> {
    state_ref.strip_prefix("tracker:")?.split_once('@')
}

pub fn lane_names(snapshots_root: &Utf8Path) -> Result<Vec<String>> {
    Ok(read_subdirs_sorted(snapshots_root)?
        .iter()
        .filter_map(|dir| dir.file_name().map(ToOwned::to_owned))
        .collect())
}

/// Every rev directory present in every lane, including leftover staging
/// directories.
pub fn lane_revs(snapshots_root: &Utf8Path) -> Result<Vec<LaneRev>> {
    let mut revs = Vec::new();
    for tracker in lane_names(snapshots_root)? {
        for dir in read_subdirs_sorted(&snapshots_root.join(&tracker))? {
            let Some(name) = dir.file_name() else {
                continue;
            };
            let (rev, is_staging) = match name.strip_suffix(".tmp") {
                Some(rev) => (rev.to_owned(), true),
                None => (name.to_owned(), false),
            };
            revs.push(LaneRev {
                tracker: tracker.clone(),
                rev,
                path: dir,
                is_staging,
            });
        }
    }
    Ok(revs)
}

/// Workspace directories under `workspace_root` that no binding record
/// claims. A workspace is a cache, so an unclaimed one is just garbage —
/// but only the records can say which ones are claimed.
///
/// Deletion is deliberately narrower than "unclaimed": a directory is
/// removed only if newgit can prove it made it (it carries a workspace
/// marker) or there is nothing to lose (it is empty). `[workspace] root` is
/// user-configurable and might point somewhere shared, and "newgit deleted a
/// directory it did not create" is not a failure mode worth risking to
/// reclaim disk. Anything else is reported as unrecognized and left alone.
pub fn orphan_workspaces(
    workspace_root: &Utf8Path,
    branches: &[BranchInstance],
) -> Result<(Vec<Utf8PathBuf>, Vec<String>)> {
    let claimed: BTreeSet<&Utf8Path> = branches
        .iter()
        .map(|branch| branch.workspace_path.as_path())
        .collect();

    let mut orphans = Vec::new();
    let mut warnings = Vec::new();
    for dir in read_subdirs_sorted(workspace_root)? {
        if claimed.contains(dir.as_path()) {
            continue;
        }
        if workspace_marker_path(&dir).is_file() || is_empty_dir(&dir)? {
            orphans.push(dir);
        } else {
            warnings.push(format!(
                "{dir} sits under the workspace root but has no binding record and no newgit \
                 workspace marker, so newgit did not remove it — delete it yourself if it is junk"
            ));
        }
    }
    Ok((orphans, warnings))
}

fn is_empty_dir(path: &Utf8Path) -> Result<bool> {
    let mut entries = std::fs::read_dir(path).map_err(|source| NewgitError::io(path, source))?;
    Ok(entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_state_refs_parse_and_others_are_ignored() {
        assert_eq!(
            parse_tracker_state_ref("tracker:db-snapshots@77e10b2c4451"),
            Some(("db-snapshots", "77e10b2c4451"))
        );
        assert_eq!(parse_tracker_state_ref("hash:9921aa04d2e1"), None);
        assert_eq!(parse_tracker_state_ref("pv_9"), None);
    }

    #[test]
    fn ownership_gates_per_branch_teardown() {
        assert!(may_tear_down(Ownership::Branch));
        assert!(may_tear_down(Ownership::Workspace));
        assert!(may_tear_down(Ownership::External));
        assert!(!may_tear_down(Ownership::Project));
        assert!(!may_tear_down(Ownership::User));
    }

    #[test]
    fn lane_revs_flag_staging_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        std::fs::create_dir_all(root.join("runtime-env/abc123")).expect("mkdir");
        std::fs::create_dir_all(root.join("runtime-env/def456.tmp")).expect("mkdir");
        std::fs::write(root.join("runtime-env/LATEST"), "abc123\n").expect("write");

        let revs = lane_revs(&root).expect("lane revs");
        assert_eq!(revs.len(), 2, "LATEST is a file, not a rev");
        assert_eq!(revs[0].rev, "abc123");
        assert!(!revs[0].is_staging);
        assert_eq!(revs[1].rev, "def456");
        assert!(revs[1].is_staging);
    }
}
