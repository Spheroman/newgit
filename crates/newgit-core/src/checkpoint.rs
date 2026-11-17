use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::store::{read_dir_sorted, read_toml_at, write_toml_at};

/// One coherent snapshot across source, trackers, and resources — the record
/// `newgit undo` restores. Stored per instance at
/// `.newgit/checkpoints/<slug>/ckpt_NNN.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub id: String,
    pub branch: String,
    /// The id of the instance that took it ([`crate::branch::BranchInstance::id`]).
    /// A name's history is shared by every instance ever spawned under it,
    /// so this is what tells them apart: `remove --purge` deletes only its
    /// own instance's, and `cleanup` collects a `remove` checkpoint only
    /// when the same instance took a later one. Absent in records from
    /// before it was kept, which neither ever deletes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub reason: CheckpointReason,
    /// Set on a `before-undo` checkpoint once the undo it preceded finished.
    /// `false` means that undo left at least one resource unrestored, so this
    /// snapshot is of a state the instance never cleanly left — it is not a
    /// redo point, and a later prune can treat it as droppable where an
    /// explicit checkpoint never could be.
    ///
    /// Recorded rather than acted on: newgit cannot know at save time whether
    /// the undo will succeed, and deleting the only record of a state is the
    /// one thing checkpoints exist to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_completed: Option<bool>,
    /// Set on a checkpoint a restore of an evicted workspace took on its way
    /// (its undo's `before-undo`), once that restore succeeded: the evict
    /// checkpoint it brought back. What it holds is the marker the eviction
    /// left and a clone, not anyone's work, so `newgit cleanup` collects it
    /// ([`superseded`]). A restore that failed never sets it: its
    /// checkpoint may hold what was written at the path while it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_of: Option<String>,
    /// Set on an `evict` checkpoint once a restore brought it back into the
    /// instance's workspace and succeeded: proof the workspace that came
    /// back holds what it holds, so `newgit cleanup` may collect it once a
    /// later eviction supersedes it ([`superseded`]). An evict checkpoint
    /// nothing restored is never collected: the instance may have been
    /// removed while evicted, and then it is the only copy of its work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restored: bool,
    pub source: SourceState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracker_states: Vec<TrackerState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_states: Vec<ResourceState>,
    /// The checkpoint as a list: one commit per tracker in Git, for the
    /// committed work (`head`) and with the uncommitted work too (`tip`).
    /// Filled in when the checkpoint syncs to a remote; another machine
    /// composes its source from these.
    #[serde(default, skip_serializing_if = "CodeState::is_empty")]
    pub code: CodeState,
    /// The trackers the instance did not bind at the checkpoint: undo puts
    /// the binding back along with everything else.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub dropped: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeState {
    #[serde(default)]
    pub head: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub tip: std::collections::BTreeMap<String, String>,
}

impl CodeState {
    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tip.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CheckpointReason {
    Explicit,
    /// Safety checkpoint taken automatically before an undo — restoring it
    /// is redo.
    BeforeUndo,
    /// Taken automatically when a `git push` from the workspace publishes
    /// to the project's remote — a push `undo` cannot take back, so it is
    /// the one moment a checkpoint has to exist.
    Push,
    /// Taken by `newgit stop --evict` just before the workspace is deleted:
    /// the state `newgit start` brings the workspace back to.
    Evict,
    /// Taken by `newgit remove` just before the workspace is deleted, so
    /// removing never deletes work no checkpoint holds: the archived record
    /// names it (`removed_to`), and `newgit spawn --from-checkpoint
    /// <instance>/<id>` brings the workspace back.
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceState {
    /// Workspace HEAD at checkpoint time.
    pub head_rev: String,
    /// Dangling commit (parent: head_rev) holding uncommitted and untracked
    /// state; absent when the worktree was clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_rev: Option<String>,
    /// Ref in the store repo keeping these commits alive.
    pub store_ref: String,
    /// The branch the workspace had checked out — normally the instance's
    /// own, but an agent that ran `git checkout -b try-x` was on `try-x`,
    /// and undo puts it back there. Absent when HEAD was detached, and in
    /// records from before it was kept (those were on the instance's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_out: Option<String>,
    /// HEAD was detached at `head_rev`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

impl SourceState {
    /// The branch HEAD was on, `None` when it was detached. `own` is the
    /// instance's branch, which a record from before `checked_out` was on.
    pub fn head_branch<'a>(&'a self, own: &'a str) -> Option<&'a str> {
        if self.detached {
            None
        } else {
            Some(self.checked_out.as_deref().unwrap_or(own))
        }
    }

    /// What HEAD was on instead of the instance's branch `own`, if anything.
    pub fn off_branch(&self, own: &str) -> Option<OffBranch> {
        OffBranch::of(self.head_branch(own), own)
    }
}

/// What a workspace has checked out when it is not the instance's own
/// branch. newgit does not pretend otherwise: a checkpoint holds the
/// workspace as it stands, and the instance's branch moves only with
/// commits made on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffBranch {
    /// Some other branch: `git checkout -b try-x`.
    Branch(String),
    /// No branch at all: `git checkout <rev>`, or a rebase in progress.
    Detached,
}

impl OffBranch {
    /// `head` is the workspace's branch (`None` when detached), `own` the
    /// instance's.
    pub fn of(head: Option<&str>, own: &str) -> Option<Self> {
        match head {
            Some(head) if head == own => None,
            Some(head) => Some(Self::Branch(head.to_owned())),
            None => Some(Self::Detached),
        }
    }
}

impl std::fmt::Display for OffBranch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Branch(branch) => write!(f, "on `{branch}`"),
            Self::Detached => f.write_str("detached"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerState {
    pub name: String,
    pub definition_rev: String,
    /// Lane rev captured at checkpoint time; absent when the tracker had no
    /// content (undo then clears its owned paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rev: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceState {
    pub name: String,
    pub definition_rev: String,
    /// Checkpoint mode that produced `state_ref`: none|hash|command|external.
    pub mode: String,
    /// `hash:<hex12>`, `tracker:<name>@<rev>`, or an opaque command/external ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_ref: Option<String>,
    /// Resolved filesystem path substituted for `{{state_ref}}` in restore
    /// commands, when the ref points at deposited content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_path: Option<Utf8PathBuf>,
    /// A long-running action was alive at checkpoint time; undo restarts it.
    #[serde(default)]
    pub was_running: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved_ports: BTreeMap<String, u16>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved_exports: BTreeMap<String, String>,
    /// This resource's `[restore]` runs a command that can fail: `command`
    /// or `recompute`. `none` and `external` do not — there is nothing to
    /// prove, so `restore_proven` is meaningless for them.
    #[serde(default)]
    pub restore_exercisable: bool,
    /// `[restore]` has completed successfully at least once on this
    /// instance, as of this checkpoint. False on a checkpoint record written
    /// before this field existed — that predates the tracking, not a known
    /// failure, but "unproven" is the honest default until re-proven.
    #[serde(default)]
    pub restore_proven: bool,
}

/// The prefix a `hash` checkpoint writes its state ref with.
pub const HASH_STATE_REF_PREFIX: &str = "hash:";

impl ResourceState {
    /// What `{{state_ref}}` means for this record, or `None` when the record
    /// holds nothing a command could be handed.
    ///
    /// Deposited content resolves to its path — a restore command wants the
    /// dump, not the `tracker:<name>@<rev>` that located it. A `hash:` ref
    /// resolves to nothing at all: it is the content hash of `[identity]
    /// paths`, which says whether the inputs moved and never identifies a
    /// concrete thing to restore or tear down. Leaving `{{state_ref}}`
    /// unresolved is what makes that visible: a restore command or a cleanup
    /// hook that uses it is refused rather than run, since either one handed
    /// a meaningless argument does damage.
    pub fn consumable_state_ref(&self) -> Option<String> {
        if let Some(path) = &self.state_path {
            return Some(path.to_string());
        }
        match self.state_ref.as_deref() {
            Some(state_ref) if !state_ref.starts_with(HASH_STATE_REF_PREFIX) => {
                Some(state_ref.to_owned())
            }
            _ => None,
        }
    }
}

/// Written next to the checkpoint when resource restores fail during undo,
/// so the failure survives the terminal: what failed, where the logs are,
/// and how to re-run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub checkpoint: String,
    pub branch: String,
    pub created_at: DateTime<Utc>,
    pub failures: Vec<RestoreFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoreFailure {
    pub resource: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<Utf8PathBuf>,
    /// The command to repair and re-run, e.g. `newgit action deps.prepare`.
    pub retry_with: String,
}

/// One instance's checkpoint history on disk.
#[derive(Debug, Clone)]
pub struct CheckpointLog {
    dir: Utf8PathBuf,
    instance: String,
}

impl CheckpointLog {
    /// `dir` is `.newgit/checkpoints/<slug>/`; `instance` is only for errors.
    pub fn new(dir: Utf8PathBuf, instance: &str) -> Self {
        Self {
            dir,
            instance: instance.to_owned(),
        }
    }

    /// Records in creation order (numeric id order).
    pub fn list(&self) -> Result<Vec<CheckpointRecord>> {
        let mut records: Vec<CheckpointRecord> = Vec::new();
        for entry in read_dir_sorted(&self.dir)? {
            if entry.extension() == Some("toml")
                && entry
                    .file_name()
                    .is_some_and(|name| name.starts_with("ckpt_") && !name.contains(".recovery."))
            {
                records.push(read_toml_at(&entry)?);
            }
        }
        records.sort_by_key(|record| numeric_id(&record.id));
        Ok(records)
    }

    pub fn load(&self, id: &str) -> Result<CheckpointRecord> {
        let path = self.record_path(id);
        if !path.is_file() {
            return Err(NewgitError::UnknownCheckpoint {
                instance: self.instance.clone(),
                id: id.to_owned(),
            });
        }
        read_toml_at(&path)
    }

    pub fn latest(&self) -> Result<CheckpointRecord> {
        self.list()?
            .into_iter()
            .next_back()
            .ok_or_else(|| NewgitError::NoCheckpoints(self.instance.clone()))
    }

    /// The id the next `save` should use.
    /// One past the highest id this log has ever held: a record `cleanup`
    /// collected leaves its id behind ([`Self::delete`]), so an id is never
    /// reused for a different state.
    pub fn next_id(&self) -> Result<String> {
        let last = self
            .list()?
            .last()
            .map(|record| numeric_id(&record.id))
            .unwrap_or(0)
            .max(self.highest_deleted());
        Ok(format!("ckpt_{:03}", last + 1))
    }

    fn highest_deleted_path(&self) -> Utf8PathBuf {
        self.dir.join("highest-deleted")
    }

    fn highest_deleted(&self) -> u64 {
        std::fs::read_to_string(self.highest_deleted_path())
            .map(|text| numeric_id(text.trim()))
            .unwrap_or(0)
    }

    pub fn save(&self, record: &CheckpointRecord) -> Result<Utf8PathBuf> {
        create_dir_all(&self.dir)?;
        let path = self.record_path(&record.id);
        write_toml_at(&path, &format!("checkpoint `{}`", record.id), record)?;
        Ok(path)
    }

    pub fn save_recovery(&self, record: &RecoveryRecord) -> Result<Utf8PathBuf> {
        create_dir_all(&self.dir)?;
        let path = self
            .dir
            .join(format!("{}.recovery.toml", record.checkpoint));
        write_toml_at(
            &path,
            &format!("recovery record for `{}`", record.checkpoint),
            record,
        )?;
        Ok(path)
    }

    /// Delete a record, and the recovery record beside it if there is one.
    /// Its id stays taken: [`Self::next_id`] never hands it out again.
    pub fn delete(&self, id: &str) -> Result<()> {
        if numeric_id(id) > self.highest_deleted() {
            let path = self.highest_deleted_path();
            std::fs::write(&path, format!("{id}\n"))
                .map_err(|source| NewgitError::io(&path, source))?;
        }
        for path in [
            self.record_path(id),
            self.dir.join(format!("{id}.recovery.toml")),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(NewgitError::io(&path, source)),
            }
        }
        Ok(())
    }

    fn record_path(&self, id: &str) -> Utf8PathBuf {
        self.dir.join(format!("{id}.toml"))
    }
}

/// The checkpoints of one slug's history `newgit cleanup` collects (#446),
/// in id order. Only what is proven to be nobody's only copy of anything:
///
/// - an `evict` checkpoint a restore brought back and that succeeded
///   ([`CheckpointRecord::restored`]), once a later eviction superseded it.
///   One nothing restored stays, whatever came after: an instance removed
///   while evicted keeps its work only there, and a later instance with the
///   same name shares the history.
/// - a checkpoint a restore that succeeded took on its way
///   ([`CheckpointRecord::restore_of`]): the marker and a clone.
/// - a `remove` checkpoint the same instance ([`CheckpointRecord::instance`])
///   took a later one after: a removal that failed after its checkpoint,
///   retried. Another instance's later removal supersedes nothing: each
///   instance under a name is its own work. A record with no instance is
///   never collected.
///
/// Never one in `named`: what a live or archived record of this slug names
/// (`evicted`, `removed_to`), which is what `newgit start` or `newgit spawn
/// --from-checkpoint` brings back. Never a checkpoint anyone asked for or a
/// push's: an `explicit` checkpoint is how a state is kept for good.
pub fn superseded<'a>(
    records: &'a [CheckpointRecord],
    named: &std::collections::BTreeSet<String>,
) -> Vec<&'a CheckpointRecord> {
    let latest = |reason: CheckpointReason| {
        records
            .iter()
            .filter(|record| record.reason == reason)
            .max_by_key(|record| numeric_id(&record.id))
            .map(|record| record.id.as_str())
    };
    let latest_evict = latest(CheckpointReason::Evict);
    let retried = |record: &CheckpointRecord| {
        record.instance.as_deref().is_some_and(|instance| {
            records.iter().any(|later| {
                later.reason == CheckpointReason::Remove
                    && later.instance.as_deref() == Some(instance)
                    && numeric_id(&later.id) > numeric_id(&record.id)
            })
        })
    };
    let mut out: Vec<&CheckpointRecord> = records
        .iter()
        .filter(|record| !named.contains(&record.id))
        .filter(|record| match record.reason {
            CheckpointReason::Evict => record.restored && Some(record.id.as_str()) != latest_evict,
            CheckpointReason::BeforeUndo => record.restore_of.is_some(),
            CheckpointReason::Remove => retried(record),
            CheckpointReason::Explicit | CheckpointReason::Push => false,
        })
        .collect();
    out.sort_by_key(|record| numeric_id(&record.id));
    out
}

fn numeric_id(id: &str) -> u64 {
    id.rsplit('_')
        .next()
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_sequential_and_survive_a_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let log = CheckpointLog::new(dir.join("feature-a"), "feature-a");

        assert_eq!(log.next_id().expect("next"), "ckpt_001");
        assert!(matches!(log.latest(), Err(NewgitError::NoCheckpoints(_))));

        let record = CheckpointRecord {
            id: "ckpt_001".to_owned(),
            branch: "feature-a".to_owned(),
            instance: None,
            created_at: Utc::now(),
            message: Some("before auth refactor".to_owned()),
            reason: CheckpointReason::Explicit,
            undo_completed: None,
            restore_of: None,
            restored: false,
            source: SourceState {
                head_rev: "abc".to_owned(),
                dirty_rev: None,
                store_ref: "refs/newgit/checkpoints/feature-a/ckpt_001".to_owned(),
                checked_out: Some("feature-a".to_owned()),
                detached: false,
            },
            tracker_states: vec![],
            resource_states: vec![],
            code: CodeState::default(),
            dropped: Default::default(),
        };
        log.save(&record).expect("save");

        assert_eq!(log.next_id().expect("next"), "ckpt_002");
        assert_eq!(log.latest().expect("latest"), record);
        assert_eq!(log.load("ckpt_001").expect("load"), record);
        assert!(matches!(
            log.load("ckpt_009"),
            Err(NewgitError::UnknownCheckpoint { .. })
        ));
    }
    #[test]
    fn retention_collects_only_what_was_restored_or_retried() {
        let record =
            |n: u64, reason: CheckpointReason, restore_of: Option<&str>, restored: bool| {
                CheckpointRecord {
                    id: format!("ckpt_{n:03}"),
                    branch: "x".to_owned(),
                    instance: Some("first".to_owned()),
                    created_at: Utc::now(),
                    message: None,
                    reason,
                    undo_completed: None,
                    restore_of: restore_of.map(ToOwned::to_owned),
                    restored,
                    source: SourceState {
                        head_rev: "abc".to_owned(),
                        dirty_rev: None,
                        store_ref: format!("refs/newgit/checkpoints/x/ckpt_{n:03}"),
                        checked_out: None,
                        detached: false,
                    },
                    tracker_states: vec![],
                    resource_states: vec![],
                    code: CodeState::default(),
                    dropped: Default::default(),
                }
            };
        use CheckpointReason::*;
        let records = vec![
            record(1, Explicit, None, false),
            record(2, Evict, None, true),
            record(3, BeforeUndo, Some("ckpt_002"), false),
            // An undo someone ran: its redo point stays.
            record(4, BeforeUndo, None, false),
            // Never restored: the instance was removed while evicted, and
            // this is the only copy of its work.
            record(5, Evict, None, false),
            // A restore that failed: no `restore_of`, so it stays.
            record(6, BeforeUndo, None, false),
            record(7, Evict, None, true),
            record(8, BeforeUndo, Some("ckpt_007"), false),
            record(9, Push, None, false),
            // A removal that failed after its checkpoint, then retried.
            record(10, Remove, None, false),
            record(11, Remove, None, false),
            // The latest eviction, restored or not, stays.
            record(12, Evict, None, true),
            // A later instance under the name, removed: the first
            // instance's last removal stays, and so does this one.
            CheckpointRecord {
                instance: Some("second".to_owned()),
                ..record(13, Remove, None, false)
            },
            // From before records named their instance: never collected.
            CheckpointRecord {
                instance: None,
                ..record(14, Remove, None, false)
            },
            CheckpointRecord {
                instance: None,
                ..record(15, Remove, None, false)
            },
        ];
        let ids = |records: Vec<&CheckpointRecord>| -> Vec<String> {
            records.iter().map(|record| record.id.clone()).collect()
        };
        let none = std::collections::BTreeSet::new();
        assert_eq!(
            ids(superseded(&records, &none)),
            ["ckpt_002", "ckpt_003", "ckpt_007", "ckpt_008", "ckpt_010"]
        );
        // What a record names stays, whatever else the history says.
        let named = ["ckpt_007".to_owned(), "ckpt_010".to_owned()].into();
        assert_eq!(
            ids(superseded(&records, &named)),
            ["ckpt_002", "ckpt_003", "ckpt_008"]
        );
    }

    /// What `{{state_ref}}` is allowed to become. A hash is the case worth
    /// pinning: it is a recorded ref, so "there is no ref" is not why it
    /// resolves to nothing — it is the wrong kind of thing to hand a command.
    #[test]
    fn a_hash_ref_is_not_something_a_command_can_be_handed() {
        let state = |state_ref: Option<&str>, state_path: Option<&str>| ResourceState {
            name: "deps".to_owned(),
            definition_rev: "sha256:000000000000".to_owned(),
            mode: "hash".to_owned(),
            state_ref: state_ref.map(ToOwned::to_owned),
            state_path: state_path.map(Utf8PathBuf::from),
            was_running: false,
            resolved_ports: BTreeMap::new(),
            resolved_exports: BTreeMap::new(),
            restore_exercisable: false,
            restore_proven: false,
        };

        assert_eq!(
            state(Some("hash:0fa284b46875"), None).consumable_state_ref(),
            None
        );
        assert_eq!(
            state(Some("pv_9"), None).consumable_state_ref(),
            Some("pv_9".to_owned()),
            "an opaque handle is exactly what a command wants"
        );
        assert_eq!(
            state(Some("tracker:db-snapshots@a1b2"), Some("/lane/a1b2/db.sql"))
                .consumable_state_ref(),
            Some("/lane/a1b2/db.sql".to_owned()),
            "deposited content resolves to the path, not the ref that located it"
        );
        assert_eq!(state(None, None).consumable_state_ref(), None);
    }
}
