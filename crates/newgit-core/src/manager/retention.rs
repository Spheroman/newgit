//! Retention for what eviction makes (#446): each `stop --evict` / `start`
//! cycle adds an `evict` checkpoint and the `before-undo` checkpoints its
//! restore's undo takes, and an `evict` checkpoint's dirty commit holds every
//! untracked file's blob. `newgit cleanup` collects the ones that can never
//! be wanted again ([`crate::checkpoint::superseded`]) and the objects only
//! they reach.

use super::*;
use crate::cleanup::CollectedCheckpoint;

/// How old an unreachable object has to be before the store's `git gc`
/// deletes it. Git's own default is two weeks; an hour is still far longer
/// than any newgit command takes between writing an object and the ref that
/// holds it, so a concurrent checkpoint never loses one mid-flight.
const PRUNE_GRACE: &str = "1.hour.ago";

/// Checkpoints retention collects, each with its instance's slug.
type Superseded = Vec<(CheckpointRecord, String)>;

impl BranchManager {
    /// What retention collects from every instance's history: live ones in
    /// `live` (under each one's lock, so a restore is never cut out from
    /// under), and archived ones unless `archived` purges them anyway.
    pub(super) fn superseded_checkpoints(
        &self,
        live: &[BranchInstance],
        archived: ArchivedCheckpoints,
    ) -> Result<(Vec<HeldLock>, Superseded)> {
        let mut locks = Vec::new();
        let mut out = Vec::new();
        for slug in self.store.checkpointed_slugs()? {
            let evicted_to = match live.iter().find(|branch| branch.slug == slug) {
                Some(branch) => {
                    let lock = self.lock_instance(&branch.slug, &branch.name)?;
                    let evicted = match self.store.find_branch(&branch.slug) {
                        Ok(branch) => branch.evicted,
                        Err(NewgitError::UnknownBranchInstance(_)) => None,
                        Err(error) => return Err(error),
                    };
                    locks.push(lock);
                    evicted
                }
                None if archived == ArchivedCheckpoints::Purge => continue,
                None => None,
            };
            let records = CheckpointLog::new(self.store.checkpoint_dir(&slug), &slug).list()?;
            for record in crate::checkpoint::superseded(&records, evicted_to.as_deref()) {
                out.push((record.clone(), slug.clone()));
            }
        }
        Ok((locks, out))
    }

    /// Collect `superseded` ([`Self::superseded_checkpoints`]): the bytes
    /// the store's objects reached only by them take, then, unless
    /// `dry_run`, each record and its store ref deleted and the store's
    /// unreachable objects past [`PRUNE_GRACE`] deleted by `git gc`.
    pub(super) fn collect_checkpoints(
        &self,
        superseded: &[(CheckpointRecord, String)],
        dry_run: bool,
    ) -> Result<(Vec<CollectedCheckpoint>, u64)> {
        if superseded.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let doomed: BTreeSet<&str> = superseded
            .iter()
            .map(|(record, _)| record.source.store_ref.as_str())
            .collect();
        let bytes = self.bytes_only_reached_by(&doomed).unwrap_or(0);
        let mut collected = Vec::new();
        for (record, slug) in superseded {
            if !dry_run {
                if self.source.ref_rev(&record.source.store_ref).is_some() {
                    self.source.delete_ref(&record.source.store_ref)?;
                }
                CheckpointLog::new(self.store.checkpoint_dir(slug), slug).delete(&record.id)?;
            }
            collected.push(CollectedCheckpoint {
                slug: slug.clone(),
                id: record.id.clone(),
                reason: record.reason,
                restore_of: record.restore_of.clone(),
            });
        }
        // Nothing to free, nothing to repack: the project's own repository
        // is not gc'd for nothing.
        if !dry_run && bytes > 0 {
            let prune = format!("--prune={PRUNE_GRACE}");
            crate::source::run_git_env(
                &["-C", self.source.root().as_str(), "gc", "--quiet", &prune],
                &[],
            )?;
        }
        Ok((collected, bytes))
    }

    /// What the objects reachable from the refs in `doomed`, and from no
    /// other ref in the store, take on disk.
    fn bytes_only_reached_by(&self, doomed: &BTreeSet<&str>) -> Result<u64> {
        let root = self.source.root();
        let listing = crate::source::run_git_env(
            &[
                "-C",
                root.as_str(),
                "for-each-ref",
                "--format=%(objectname) %(refname)",
            ],
            &[],
        )?;
        let mut input = String::new();
        for line in listing.lines() {
            let Some((oid, name)) = line.split_once(' ') else {
                continue;
            };
            if doomed.contains(name) {
                input.push_str(oid);
            } else {
                input.push('^');
                input.push_str(oid);
            }
            input.push('\n');
        }
        if let Ok(head) = self.source.rev_parse("HEAD") {
            input.push_str(&format!("^{head}\n"));
        }
        let out = crate::source::run_git_stdin(
            &[
                "-C",
                root.as_str(),
                "rev-list",
                "--objects",
                "--disk-usage",
                "--stdin",
            ],
            input.as_bytes(),
        )?;
        Ok(out.trim().parse().unwrap_or(0))
    }
}

impl BranchManager {
    /// Mark what a restore of `branch` that just succeeded took on its way
    /// (every checkpoint not in `taken_before`): its undo's `before-undo`,
    /// which holds the marker and a clone, not anyone's work. `cleanup`
    /// collects them.
    pub(super) fn mark_restore_checkpoints(
        &self,
        branch: &BranchInstance,
        evict_checkpoint: &str,
        taken_before: &BTreeSet<String>,
    ) -> Result<()> {
        let log = self.checkpoint_log(branch);
        for mut record in log.list()? {
            if taken_before.contains(&record.id) || record.reason != CheckpointReason::BeforeUndo {
                continue;
            }
            record.restore_of = Some(evict_checkpoint.to_owned());
            log.save(&record)?;
        }
        Ok(())
    }
}
