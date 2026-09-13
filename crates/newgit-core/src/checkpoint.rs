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
    pub source: SourceState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracker_states: Vec<TrackerState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_states: Vec<ResourceState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CheckpointReason {
    Explicit,
    /// Safety checkpoint taken automatically before an undo — restoring it
    /// is redo.
    BeforeUndo,
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
    /// unresolved is what makes that visible — verbatim in a restore command,
    /// and a refusal in a cleanup hook, which is where handing over a
    /// meaningless argument would do damage.
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
    pub fn next_id(&self) -> Result<String> {
        let last = self
            .list()?
            .last()
            .map(|record| numeric_id(&record.id))
            .unwrap_or(0);
        Ok(format!("ckpt_{:03}", last + 1))
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

    fn record_path(&self, id: &str) -> Utf8PathBuf {
        self.dir.join(format!("{id}.toml"))
    }
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
            created_at: Utc::now(),
            message: Some("before auth refactor".to_owned()),
            reason: CheckpointReason::Explicit,
            undo_completed: None,
            source: SourceState {
                head_rev: "abc".to_owned(),
                dirty_rev: None,
                store_ref: "refs/newgit/checkpoints/feature-a/ckpt_001".to_owned(),
            },
            tracker_states: vec![],
            resource_states: vec![],
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
