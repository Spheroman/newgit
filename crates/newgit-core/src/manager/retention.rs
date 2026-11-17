//! Retention for what eviction makes (#446): each `stop --evict` / `start`
//! cycle adds an `evict` checkpoint and the `before-undo` checkpoints its
//! restore's undo takes, and an `evict` checkpoint's dirty commit holds every
//! untracked file's blob. `newgit cleanup` collects the ones proven to be
//! nobody's only copy of anything ([`crate::checkpoint::superseded`]): their
//! records and the store refs that held them. The objects only they reached
//! become unreachable, and the store's own `git gc` deletes them on its
//! schedule, under the user's `gc.pruneExpire`; newgit never prunes the store
//! itself, since the publish outbox borrows the store's objects.

use super::*;
use crate::cleanup::CollectedCheckpoint;

/// Checkpoints retention collects, each with its instance's slug.
type Superseded = Vec<(CheckpointRecord, String)>;

impl BranchManager {
    /// What retention collects from every slug's history: the checkpoints
    /// [`crate::checkpoint::superseded`] names, minus every one a live or
    /// archived record names (`evicted`, `removed_to`), plus checkpoint refs
    /// no record names (a checkpoint that failed partway). Each live slug
    /// is read under its lock, released straight after; archived ones are
    /// skipped when `archived` purges them anyway. A slug with an archived
    /// record that cannot be read is left alone: what it names is unknown.
    pub(super) fn superseded_checkpoints(
        &self,
        live: &[BranchInstance],
        archived: ArchivedCheckpoints,
    ) -> Result<(Superseded, Vec<String>)> {
        let mut named: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut unreadable: BTreeSet<String> = BTreeSet::new();
        for (slug, record) in self.store.archived_branches()? {
            match record {
                Ok(record) => {
                    let ids = named.entry(record.slug.clone()).or_default();
                    ids.extend(record.evicted.clone());
                    ids.extend(record.removed_to.clone());
                }
                Err(_) => {
                    unreadable.insert(slug);
                }
            }
        }
        let mut out = Vec::new();
        let mut orphans = Vec::new();
        for slug in self.store.checkpointed_slugs()? {
            if unreadable.contains(&slug) {
                continue;
            }
            let is_live = live.iter().any(|branch| branch.slug == slug);
            if !is_live && archived == ArchivedCheckpoints::Purge {
                continue;
            }
            let mut named = named.remove(&slug).unwrap_or_default();
            // Under the lock for as long as one slug takes to read: a
            // checkpoint writes its ref before its record, and a restore
            // stamps what it brought back.
            let _lock = self.lock_instance(&slug, &slug)?;
            match self.store.find_branch(&slug) {
                Ok(branch) => {
                    named.extend(branch.evicted);
                    named.extend(branch.removed_to);
                }
                Err(NewgitError::UnknownBranchInstance(_)) => {}
                Err(error) => return Err(error),
            }
            let records = CheckpointLog::new(self.store.checkpoint_dir(&slug), &slug).list()?;
            for record in crate::checkpoint::superseded(&records, &named) {
                out.push((record.clone(), slug.clone()));
            }
            let recorded: BTreeSet<&str> = records
                .iter()
                .map(|record| record.source.store_ref.as_str())
                .collect();
            let prefix = format!("refs/newgit/checkpoints/{slug}/");
            for name in self
                .source
                .refs_under(&format!("refs/newgit/checkpoints/{slug}"))?
            {
                let is_checkpoint = name
                    .strip_prefix(&prefix)
                    .is_some_and(|id| id.starts_with("ckpt_") && !id.contains('/'));
                if is_checkpoint && !recorded.contains(name.as_str()) {
                    orphans.push(name);
                }
            }
        }
        Ok((out, orphans))
    }

    /// Collect `superseded` ([`Self::superseded_checkpoints`]) and the
    /// `orphans` refs: the bytes the store's objects reached only by them
    /// take, then, unless `dry_run`, each record and its store ref deleted.
    /// Nothing is pruned here: the objects go at the store's own next `git
    /// gc` past its grace period. Each slug's lock is held only while its
    /// records are deleted, and each record is read again under it, so one
    /// a restore or a removal just came to name stays.
    pub(super) fn collect_checkpoints(
        &self,
        superseded: &[(CheckpointRecord, String)],
        orphans: &[String],
        dry_run: bool,
    ) -> Result<(Vec<CollectedCheckpoint>, u64)> {
        if superseded.is_empty() && orphans.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let doomed: BTreeSet<&str> = superseded
            .iter()
            .map(|(record, _)| record.source.store_ref.as_str())
            .chain(orphans.iter().map(String::as_str))
            .collect();
        let bytes = self.bytes_only_reached_by(&doomed).unwrap_or(0);
        if dry_run {
            let collected = superseded
                .iter()
                .map(|(record, slug)| collected(record, slug))
                .collect();
            return Ok((collected, bytes));
        }
        let mut by_slug: BTreeMap<&str, Vec<&CheckpointRecord>> = BTreeMap::new();
        for (record, slug) in superseded {
            by_slug.entry(slug.as_str()).or_default().push(record);
        }
        let mut out = Vec::new();
        for (slug, records) in by_slug {
            let _lock = self.lock_instance(slug, slug)?;
            let log = CheckpointLog::new(self.store.checkpoint_dir(slug), slug);
            // Read again: what changed since the pass above wins.
            let now = log.list()?;
            let mut named = BTreeSet::new();
            if let Ok(branch) = self.store.find_branch(slug) {
                named.extend(branch.evicted);
                named.extend(branch.removed_to);
            }
            let still: BTreeSet<&str> = crate::checkpoint::superseded(&now, &named)
                .into_iter()
                .map(|record| record.id.as_str())
                .collect();
            for record in records {
                if !still.contains(record.id.as_str()) {
                    continue;
                }
                if self.source.ref_rev(&record.source.store_ref).is_some() {
                    self.source.delete_ref(&record.source.store_ref)?;
                }
                log.delete(&record.id)?;
                out.push(collected(record, slug));
            }
        }
        for name in orphans {
            // `refs/newgit/checkpoints/<slug>/<id>`, under that slug's lock,
            // and only while still no record names it.
            let Some((slug, id)) = name
                .strip_prefix("refs/newgit/checkpoints/")
                .and_then(|rest| rest.rsplit_once('/'))
            else {
                continue;
            };
            let _lock = self.lock_instance(slug, slug)?;
            let log = CheckpointLog::new(self.store.checkpoint_dir(slug), slug);
            if log.list()?.iter().all(|record| record.id != id) {
                self.source.delete_ref(name)?;
            }
        }
        Ok((out, bytes))
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

fn collected(record: &CheckpointRecord, slug: &str) -> CollectedCheckpoint {
    CollectedCheckpoint {
        slug: slug.to_owned(),
        id: record.id.clone(),
        reason: record.reason,
        restore_of: record.restore_of.clone(),
    }
}

impl BranchManager {
    /// Mark what a restore of `branch` that just succeeded proved: the evict
    /// checkpoint it brought back is `restored` (so it is no longer anyone's
    /// only copy), and what the restore took on its way (every checkpoint
    /// not in `taken_before`: its undo's `before-undo`, which holds the
    /// marker and a clone, not anyone's work) is `restore_of` it. `cleanup`
    /// collects them.
    pub(super) fn mark_restore_checkpoints(
        &self,
        branch: &BranchInstance,
        evict_checkpoint: &str,
        taken_before: &BTreeSet<String>,
    ) -> Result<()> {
        let log = self.checkpoint_log(branch);
        for mut record in log.list()? {
            if record.id == evict_checkpoint && record.reason == CheckpointReason::Evict {
                record.restored = true;
                log.save(&record)?;
                continue;
            }
            if taken_before.contains(&record.id) || record.reason != CheckpointReason::BeforeUndo {
                continue;
            }
            record.restore_of = Some(evict_checkpoint.to_owned());
            log.save(&record)?;
        }
        Ok(())
    }
}
