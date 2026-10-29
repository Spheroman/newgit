use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;

use crate::branch::{
    BranchInstance, RemoteBinding, ResourceBinding, ResourceStatus, TrackerBinding, branch_slug,
    validate_name,
};
use crate::checkpoint::{
    CheckpointLog, CheckpointReason, CheckpointRecord, HASH_STATE_REF_PREFIX, RecoveryRecord,
    ResourceState, RestoreFailure, SourceState, TrackerState,
};
use crate::cleanup::{
    ArchivedCheckpoints, CleanupOutcome, FinalizedInstance, HookDetail, HookOutcome, PrunedInstall,
    PrunedRev, PurgedCheckpoints, SnapshotRoots, may_tear_down, orphan_workspaces,
};
use crate::config::ProjectConfig;
use crate::deposit_diff::{DepositDiff, diff_deposits};
use crate::error::{NewgitError, Result};
use crate::export::{self, ExportFilter, ExportPlan, prepare_destination};
use crate::export_target::ExportTarget;
use crate::exports::{RenderContext, STATE_REF_PLACEHOLDER, render, unresolved_placeholder};
use crate::installs::{Admission, InstallReport, InstallStore, TreeRender};
use crate::lock::{InstanceLock, LOCKED_ENV, locked_env_value};
use crate::materializer::{
    Materializer, RealDirMaterializer, SOURCE_TEMPLATE_DIR, exclude_produced_paths,
    exclude_tracker_paths, unexclude_tracker_paths,
};
use crate::ports;
use crate::remote::{Acting, Remote, RemoteLink, Scope, basic_auth_header};
use crate::render::{self, RenderRecord};
use crate::resource::{
    Captures, CheckpointMode, DataEdges, GraphProblem, ResourceDefinition, ResourceGraph,
    RestoreMode, parse_captures, resolve_graph,
};
use crate::source::{GitSource, RefUpdate};
use crate::store::MetadataStore;
use crate::supervisor::{StopOutcome, Supervisor, run_captured, run_foreground};
use crate::templates::{instantiate, resource_template};
use crate::tracker::{
    Storage, TrackerDefinition, collect_files, collect_owned_files, content_rev, normalize_path,
};
use crate::tracker_repo::{
    Branch, CaptureAt, TrackerRepo, clear_owned_paths, copy_file, parse_tracker_state_ref,
    pinned_revs, prune_unreferenced,
};

/// The store remote a workspace's `git push` is forwarded to.
const PUBLISH_REMOTE: &str = "origin";

/// Orchestrates branch-instance lifecycle against one store.
#[derive(Debug)]
pub struct BranchManager {
    store: MetadataStore,
    /// Held rather than built per call: constructing one probes the
    /// filesystem for copy-on-write support, which is two file writes and a
    /// `cp`. Once per manager, not once per lookup.
    installs: InstallStore,
    config: ProjectConfig,
    source: GitSource,
    trackers: Vec<TrackerDefinition>,
    resources: Vec<ResourceDefinition>,
    /// The resolved graph: bind order, lifecycle order, inferred data edges,
    /// and why it does not hold together if it doesn't. Resources whose
    /// dependencies are unresolved are still ordered; see `graph_problems`.
    graph: ResourceGraph,
    /// The project's remote, when `[remote]` is configured.
    remote: Option<RemoteLink>,
    /// The tracker owning everything no other tracker claims. Held apart
    /// from `trackers`, the ones newgit captures from files: root's history
    /// is the project's own Git history, committed with Git.
    root: TrackerDefinition,
    /// Committed trackers with their own histories, composed into workspaces
    /// (docs/design/trackers.md, step 3): `project` and any carved-out code.
    code: Vec<TrackerDefinition>,
    /// The instance locks this manager holds right now, by slug (see
    /// [`crate::lock`]). A nested call for an instance already locked gets
    /// the held lock back instead of waiting on its own process; the entry
    /// dies with the last guard.
    locks: Mutex<BTreeMap<String, Weak<InstanceLock>>>,
}

/// An instance's lock as a command holds it; dropping the last guard for an
/// instance releases it. See [`BranchManager::lock_instance`].
#[derive(Debug)]
pub struct HeldLock(Arc<InstanceLock>);

impl HeldLock {
    pub fn slug(&self) -> &str {
        self.0.slug()
    }
}

/// What creating a code tracker did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCodeTrackerOutcome {
    pub path: Utf8PathBuf,
    /// The tracker its history was carved out of.
    pub source: String,
    /// How many commits it starts with.
    pub commits: u32,
}

/// What `spawn --from-checkpoint` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnFromCheckpointOutcome {
    /// The instance as it stands after the restore.
    pub branch: BranchInstance,
    pub spawned: SpawnOutcome,
    pub undo: UndoOutcome,
    /// Each `autostart` action started once the restore was done. Started
    /// after it rather than at the spawn, so the restore does not stop a
    /// process the checkpoint never had running.
    pub started: Vec<StopStartResource>,
    /// Who took the checkpoint, and the agent that did, if one did.
    pub author: String,
    pub agent: Option<String>,
    /// Trackers that did not come across, and why; plus the restore's own.
    pub warnings: Vec<String>,
}

impl SpawnFromCheckpointOutcome {
    /// Complete when the restore was, and every resource the instance ended
    /// up with is ready. Asked of the record the restore left rather than of
    /// `spawned`, because the restore runs after the bind: it can bring back
    /// a resource the bind failed, fail one the bind brought up, or bind one
    /// for a tracker the checkpoint had and the spawn did not — and a
    /// resource the checkpoint holds no state for keeps what the bind left.
    /// The question `SpawnOutcome::is_complete` and `UndoOutcome::is_complete`
    /// each ask, asked of both steps together.
    pub fn is_complete(&self) -> bool {
        self.undo.is_complete()
            && self
                .branch
                .resources
                .values()
                .all(|binding| binding.status == ResourceStatus::Ready)
            && self
                .started
                .iter()
                .all(|start| start.status != StopStartStatus::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOutcome {
    pub branch: BranchInstance,
    pub record_path: Utf8PathBuf,
    /// False when the instance attached to a pre-existing source branch.
    pub created_source_branch: bool,
    pub trackers: Vec<TrackerBindOutcome>,
    pub resources: Vec<ResourceBindOutcome>,
    /// Each `autostart` action spawn tried to start, dependencies first. A
    /// resource whose start failed did not come up, and counts as failed.
    pub started: Vec<StopStartResource>,
    /// Set when this name's checkpoint directory already held records from a
    /// previous, archived instance, so numbering continues rather than
    /// restarting. See [`InheritedCheckpoints`].
    pub inherited_checkpoints: Option<InheritedCheckpoints>,
    /// Why the instance is not on the project's remote yet, when it has one
    /// and registering failed. The instance works locally regardless, and
    /// registration is retried at its next checkpoint.
    pub remote_warning: Option<String>,
    /// Why the project root's commits could not be split into its tracker
    /// branches. Spawning goes on: nothing has left the machine, and the same
    /// split runs again before anything does.
    pub projection_warning: Option<String>,
    /// Why the workspace was `git clone`d rather than cloned from the
    /// source template on macOS. The workspace is the same either way; the
    /// spawn was only slower.
    pub template_warning: Option<String>,
}

/// Checkpoint numbering is per name, and `remove` archives the binding record
/// without deleting the checkpoint files — so spawning a fresh instance under
/// a name that has been removed before continues the old numbering, and its
/// first checkpoint comes back as `ckpt_004` on an instance thirty seconds
/// old (#84).
///
/// Restarting at `ckpt_001` is not available: the old `ckpt_001.toml` is
/// still on disk (that is what `newgit cleanup --purge-archived` exists to
/// release), and reusing the id would make it ambiguous which of two
/// checkpoints `newgit undo ckpt_001` meant. So `spawn` says it instead, at
/// the moment it becomes true, rather than leaving the number to be
/// explained after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedCheckpoints {
    /// How many records the name already carries.
    pub existing: usize,
    /// The id the first checkpoint on this instance will get.
    pub next_id: String,
}

impl SpawnOutcome {
    /// Resources that did not come up ready: `Failed` (export, render, or
    /// prepare itself failed) and `Pending` (prepare never ran because a
    /// dependency blocked it) both count, and so does a `Ready` resource with
    /// a non-empty `blocked_by` — one with no `prepare` to withhold, whose
    /// command actions are refused all the same (#209). A blocked resource
    /// is not a milder case than a failed one — it is equally unusable either
    /// way, it just failed for a reason one step upstream.
    pub fn failed_resources(&self) -> Vec<&str> {
        let mut failed: Vec<&str> = self
            .resources
            .iter()
            .filter(|resource| resource.status != ResourceStatus::Ready || resource.is_blocked())
            .map(|resource| resource.name.as_str())
            .collect();
        for start in &self.started {
            if start.status == StopStartStatus::Failed && !failed.contains(&start.name.as_str()) {
                failed.push(&start.name);
            }
        }
        failed
    }

    /// The instance is complete when every resource bound cleanly. Mirrors
    /// `UndoOutcome::is_complete` — same question, asked at spawn instead of
    /// restore.
    pub fn is_complete(&self) -> bool {
        self.failed_resources().is_empty()
    }
}

/// How a resource was bound at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBindOutcome {
    pub name: String,
    pub ports: BTreeMap<String, u16>,
    pub status: ResourceStatus,
    /// Present when a `prepare` action ran: (succeeded, log path).
    pub prepare: Option<(bool, Utf8PathBuf)>,
    /// Resource dependencies that were not ready when this resource was
    /// bound. They withheld `prepare` if there was one to run, and
    /// `newgit action` refuses this resource's command actions until they
    /// are repaired — so a resource with any is blocked whatever its own
    /// `status` says, and counts as not bound (#209).
    pub blocked_by: Vec<String>,
    /// Whether `blocked_by` withheld a runnable `prepare`. False for a
    /// blocked resource with no `prepare` to run, which is blocked all the
    /// same but not *at* `prepare`.
    pub prepare_withheld: bool,
    /// Export names `prepare` published through `captures`.
    pub captured: Vec<String>,
    /// One warning per declared capture `prepare` never emitted.
    pub missing_captures: Vec<String>,
    /// Files rendered before `prepare`, and what it cost to render them.
    pub rendered: Vec<RenderOutcome>,
    /// Why a render failed, when one did. A render failure blocks `prepare`
    /// the way a failed dependency does: a `prepare` run against unrendered
    /// config would start a service on the wrong port.
    pub render_error: Option<String>,
    /// Why an export refused to resolve, when one did. Separate from
    /// `render_error` so the report names what actually went wrong: an
    /// export that never resolved is not a file that failed to render.
    pub export_error: Option<String>,
    /// What the install store did, when this resource declares a tree it
    /// produces. `None` means the resource is not eligible for it.
    pub install: Option<InstallReport>,
}

impl ResourceBindOutcome {
    /// Whether a dependency kept this resource from coming up. Its own
    /// `status` can still be `Ready` — a resource with no runnable `prepare`
    /// is bound `Ready` because nothing was withheld from it — but every
    /// surface that reports on it (`status`, `action`, `spawn`) says
    /// blocked, and so does this.
    pub fn is_blocked(&self) -> bool {
        !self.blocked_by.is_empty()
    }
}

/// Rendered files temporarily reverted to committed values, and the content
/// to put back. The workspace must end a capture exactly as it started it —
/// the instance is still running against those rendered values.
#[derive(Debug, Default)]
struct RenderedRestore {
    files: Vec<(Utf8PathBuf, String)>,
}

impl RenderedRestore {
    fn restore(self) -> Result<()> {
        for (path, contents) in self.files {
            std::fs::write(&path, contents).map_err(|source| NewgitError::io(path, source))?;
        }
        Ok(())
    }
}

/// One rendered file.
///
/// Carries no warning about lost hand edits: at bind there are none, and a
/// caveat printed when nothing is wrong is silent at the moment something
/// is. That report lives in `render_drift`, which speaks at checkpoint and
/// before a re-render, naming the file and how much of it is about to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutcome {
    pub path: Utf8PathBuf,
    pub replacements: usize,
    /// The tracker owning the path, if any. `None` means source-owned.
    pub tracker: Option<String>,
}

/// One `[[render]]` target checked against the working tree. What
/// `BranchManager::render_check` reports, one per `path` across every
/// resource — `render --check`'s per-file line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCheckTarget {
    pub resource: String,
    pub path: Utf8PathBuf,
    /// The tracker owning the path, if any. `None` means source-owned.
    pub tracker: Option<String>,
    /// `None` when `path` could not be read from the working tree at all —
    /// distinct from every `find` failing to match inside it.
    pub checks: Option<Vec<render::FindCheck>>,
    /// The path is inside the resource's `[identity] produces`. Its input
    /// is a stored tree, not the working tree, so there is nothing here to
    /// check: the store reports a render that matched nothing when it
    /// stores one.
    pub in_built_tree: bool,
}

impl RenderCheckTarget {
    /// The path was readable and every `find` matched its declared count.
    pub fn ok(&self) -> bool {
        self.in_built_tree
            || self
                .checks
                .as_ref()
                .is_some_and(|checks| checks.iter().all(render::FindCheck::ok))
    }
}

/// One variable of the command environment, and the declaration it came
/// from. `check_env_names` guarantees every name has exactly one declaring
/// resource, so the origin is never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    pub source: EnvSource,
}

/// Where a command-environment variable is declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSource {
    /// A key in the resource's `[exports]`.
    Export { resource: String },
    /// Captured from an action's stdout, with no `[exports]` key of its own.
    Capture { resource: String, action: String },
    /// A port's `env` name.
    Port { resource: String, port: String },
    /// `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE`, set by newgit itself.
    Newgit,
}

impl std::fmt::Display for EnvSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Export { resource } => write!(f, "{resource} [exports]"),
            Self::Capture { resource, action } => {
                write!(f, "{resource} [actions.{action}] captures")
            }
            Self::Port { resource, port } => write!(f, "{resource} [ports.{port}]"),
            Self::Newgit => f.write_str("newgit"),
        }
    }
}

/// The declaration a bound export value came from: an `[exports]` key when
/// there is one (a capture of the same name only refines it), otherwise the
/// action that captures it. A binding whose definition has since vanished
/// is still an export of that resource.
fn export_source(resource: &str, definition: Option<&ResourceDefinition>, key: &str) -> EnvSource {
    let capturing_action = definition
        .filter(|definition| !definition.exports.contains_key(key))
        .and_then(|definition| {
            definition
                .actions
                .iter()
                .find(|(_, action)| action.captures.iter().any(|name| name == key))
        });
    match capturing_action {
        Some((action, _)) => EnvSource::Capture {
            resource: resource.to_owned(),
            action: action.clone(),
        },
        None => EnvSource::Export {
            resource: resource.to_owned(),
        },
    }
}

/// The signal a command-less action sends: its own, else the resource's
/// stop signal.
fn action_signal(definition: &ResourceDefinition, action: &crate::resource::ActionSpec) -> String {
    action
        .signal
        .clone()
        .unwrap_or_else(|| definition.stop_signal())
}

/// The long-running action `undo` and `start` bring a resource back with:
/// the first one it defines, by name.
fn long_running_action(
    definition: &ResourceDefinition,
) -> Option<(&str, &crate::resource::ActionSpec)> {
    definition
        .actions
        .iter()
        .find(|(_, action)| action.long_running)
        .map(|(name, action)| (name.as_str(), action))
}

/// What `newgit action <resource>.<action>` would do, without doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPlan {
    /// A command-less action (e.g. `stop`): signal the supervised process.
    Signal { signal: String },
    /// Run this exact command — rendered, in this directory, with
    /// [`BranchManager::assemble_env`] layered over the caller's environment.
    Command {
        command: String,
        cwd: Utf8PathBuf,
        /// Started under supervision rather than run to completion.
        long_running: bool,
        /// Names read out of stdout into the binding's exports.
        captures: Vec<String>,
    },
}

/// What running an action did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Long-running action started under supervision.
    Started {
        pid: u32,
        log: Utf8PathBuf,
    },
    Stopped(StopOutcome),
    /// One-shot command finished with this exit code.
    Ran {
        code: i32,
        log: Utf8PathBuf,
        /// One warning per declared capture the command never emitted.
        missing_captures: Vec<String>,
        /// Present when this was the action that builds `[identity]
        /// produces`, and so had a tree worth publishing.
        install: Option<InstallReport>,
    },
}

/// What `newgit stop` or `newgit start` did to an instance: one entry per
/// resource it acted on, none when there was nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopStartOutcome {
    pub instance: String,
    pub resources: Vec<StopStartResource>,
}

impl StopStartOutcome {
    /// Whether every resource it acted on ended up as asked.
    pub fn ok(&self) -> bool {
        self.resources
            .iter()
            .all(|resource| resource.status != StopStartStatus::Failed)
    }
}

/// One resource `newgit stop` or `newgit start` acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopStartResource {
    pub name: String,
    /// The long-running action stopped or started — the one `start` runs
    /// again, so a stop and the start after it name the same action.
    pub action: String,
    pub status: StopStartStatus,
    /// Why, when `status` is `Failed`.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStartStatus {
    Stopped,
    Started,
    Failed,
}

impl StopStartStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Started => "started",
            Self::Failed => "failed",
        }
    }
}

/// What `newgit stop` stopped and `newgit start` has not yet brought back,
/// at `.newgit/state/<slug>/stopped.toml`. Living in the state directory,
/// it goes with the instance on `remove`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct StoppedRecord {
    #[serde(default)]
    resources: Vec<StoppedResource>,
}

/// One thing `start` owes: `action` is the long-running action to start
/// again, or `resume` for a resource whose `suspend` ran. A resource can owe
/// both, and `resume` is never a long-running action's name, so the two
/// never collide.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StoppedResource {
    name: String,
    action: String,
}

impl StoppedResource {
    fn is_resume(&self) -> bool {
        self.action == "resume"
    }
}

/// How a tracker's content landed in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerBindOutcome {
    pub name: String,
    pub content_rev: Option<String>,
    pub files: usize,
    pub origin: BindOrigin,
    /// Re-renders that failed after the lane head landed. See
    /// [`RestoreReport::warnings`].
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOrigin {
    /// Projected from the lane head.
    LaneHead,
    /// Bound with no captured content yet.
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceReport {
    pub branch: BranchInstance,
    pub workspace_exists: bool,
    /// Live HEAD of the workspace clone, when it can be read.
    pub live_rev: Option<String>,
    pub trackers: Vec<TrackerReport>,
    pub resources: Vec<ResourceReport>,
    /// How far `branch.base_ref` has moved since this instance branched,
    /// computed fresh from the store's own refs on every call. `None` when
    /// the instance has no recorded base, or its base branch no longer
    /// exists in the store to compare against.
    pub base: Option<BaseReport>,
    pub readiness: Readiness,
}

/// Whether an instance works right now — checked, not remembered: each
/// resource bound, each tree it builds present, each port of a running one
/// answering. What `status` and the end of `spawn` say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// What is wrong, one line each; empty when ready.
    pub problems: Vec<String>,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseReport {
    pub base_ref: String,
    /// Commits `base_ref`'s current tip has that this instance's spawn
    /// point did not.
    pub ahead: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceReport {
    pub name: String,
    /// `running`, `stopped`, `ready`, `pending`, `failed`, or `—` (unbound).
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerReport {
    pub name: String,
    /// None when the tracker is defined but this instance has no binding.
    pub content_rev: Option<String>,
    /// The lane head/default, when one has been merged.
    pub lane_head: Option<String>,
}

impl TrackerReport {
    /// The lane has content this instance never had: pulling is safe advice.
    pub fn never_pulled(&self) -> bool {
        self.lane_head.is_some() && self.content_rev.is_none()
    }

    /// Bound content differs from the lane head. Without rev ancestry the
    /// direction is unknowable — this instance may be ahead, behind, or
    /// diverged — so callers must not advise one direction.
    pub fn diverged(&self) -> bool {
        self.content_rev.is_some() && self.lane_head.is_some() && self.content_rev != self.lane_head
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub branch: BranchInstance,
    pub archived_record: Utf8PathBuf,
    /// What each resource's cleanup hook did, dependents first.
    pub hooks: Vec<HookOutcome>,
    /// The checkpoint history discarded, when removal was asked to purge it.
    pub purged_checkpoints: Option<PurgedCheckpoints>,
    /// How many checkpoints removal left behind instead — retained disk the
    /// caller should be told about, since nothing reaches them any more.
    pub kept_checkpoints: usize,
    /// Why the instance could not be removed from the remote, when it was on
    /// one: its fork and token outlive it until removed there.
    pub remote_warning: Option<String>,
    /// What happened to the instance's source branch in the store, and why.
    pub source_branch: SourceBranchOutcome,
    /// The workspace's HEAD when no ref in the store contained it: commits
    /// made after the last checkpoint, deleted with the workspace. `remove`
    /// does not checkpoint first (the workspace is disposable), but it does
    /// say so, rather than let "nothing unique" on the branch read as
    /// "nothing lost".
    pub unsaved_workspace_head: Option<String>,
}

/// What `remove` should do with the instance's source branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceBranchPolicy {
    /// Delete it when it holds nothing unique — newgit created it, nothing
    /// has it checked out, and its tip is already on another branch, a tag,
    /// or the remote. Otherwise keep it and say which of those failed.
    #[default]
    Auto,
    /// Keep it whatever it holds.
    Keep,
    /// Delete it whatever it holds. Still refuses a branch checked out in
    /// the store, before anything is torn down.
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceBranchOutcome {
    /// Deleted at `tip`. `found_on` names one other ref whose history holds
    /// the tip (`main`, `origin/feature-a`, `tag v1.2`) — `None` only when
    /// the deletion was forced past commits that existed nowhere else.
    Deleted {
        tip: String,
        found_on: Option<String>,
        /// Whether a checkpoint ref in the store still reaches `tip` after
        /// removal — i.e. whether `git branch <name> <tip>` keeps working
        /// past the next `git gc`.
        pinned_by_checkpoint: bool,
    },
    Kept(KeptBranch),
    /// The branch was already gone from the store.
    Missing,
}

/// Why `remove` left the source branch in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptBranch {
    /// `--keep-branch`.
    Asked,
    /// It existed before `spawn`; newgit adopted it and does not own it.
    Adopted,
    /// Checked out in the store at this path.
    CheckedOut(Utf8PathBuf),
    /// Its tip is on no other branch, tag, or remote ref — deleting it is
    /// the only way these commits stop being a branch.
    Unique { tip: String },
    /// Its tip is on no local ref, and the publish remote could not be
    /// reached to confirm it has the tip. Keeping is the safe direction.
    RemoteUnconfirmed { tip: String, error: String },
    /// The branch moved while removal ran, so the deletion — guarded on the
    /// tip it decided about — did not happen. `now` is where it points.
    Moved {
        expected: String,
        now: Option<String>,
    },
}

/// Whether, and how, [`BranchManager::ref_holding`] may read the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboxRead {
    /// Bring it up to date with the publish remote first (a network fetch),
    /// and count it only if that succeeded.
    Refresh,
    /// Read it as it stands — only right after a `Refresh`.
    AsIs,
    /// Do not count it.
    Skip,
}

/// What [`BranchManager::ref_holding`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holding {
    found_on: Option<String>,
    /// `found_on` came from the outbox.
    via_outbox: bool,
    /// The outbox refresh failed, so the remote went uncounted.
    remote_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    pub destination: Utf8PathBuf,
    /// Branch name in the exported repository (the instance's source ref).
    pub branch: String,
    pub instance: String,
    /// Workspace HEAD the export was taken from.
    pub source_head: String,
    /// The single commit the export produced.
    pub commit: String,
    pub plan: ExportPlan,
    /// Committed files the export's own `.gitignore` still matches.
    pub still_gitignored: Vec<Utf8PathBuf>,
}

/// Why a code tracker is in a remote export, or that it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportReason {
    /// Its audience is `public`.
    Public,
    /// The target's `trackers` names it.
    Listed,
    /// Neither: its history stays out.
    Withheld,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedTracker {
    pub name: String,
    pub audience: String,
    pub reason: ExportReason,
    /// Its `main`, or `None` when it has no history yet.
    pub commit: Option<String>,
    /// The project root has commits in it that `main` does not: work this
    /// export leaves behind until `newgit merge main`.
    pub unmerged: bool,
}

struct ComposedExport {
    tip: String,
    commits: usize,
}

/// An instance's branch, exported to one `branches = true` target after a
/// push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchExport {
    pub target: String,
    pub url: String,
    pub branch: String,
    pub result: std::result::Result<BranchExported, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchExported {
    /// The target's branch is the pushed commit itself — every tracker the
    /// history touched ships — so `gh pr create` finds it with no flags.
    pub same_commits: bool,
    /// Code trackers this target does not ship.
    pub withheld: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteExportOptions {
    /// Replace the target branch even when the export does not descend from it.
    pub force: bool,
    /// Compose and check, but push nothing.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExportOutcome {
    pub target: ExportTarget,
    /// Where Git pushed (or would have).
    pub url: String,
    /// Every code tracker, shipped or withheld.
    pub trackers: Vec<ExportedTracker>,
    /// The composed history's tip.
    pub tip: String,
    pub commits: usize,
    /// False on a dry run.
    pub pushed: bool,
    /// Why the project root's own work could not be compared with `main`.
    pub project_root_error: Option<String>,
    /// Commits the target's branch had that `main` did not — merged there,
    /// say, as a pull request — brought into `main` before pushing (or, on a
    /// dry run, that would be). Zero when the target was not ahead.
    pub pulled: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOutcome {
    pub record: CheckpointRecord,
    pub record_path: Utf8PathBuf,
    pub warnings: Vec<String>,
    /// Why the checkpoint is not on the remote, when there is one and syncing
    /// failed. (Also among `warnings`.)
    pub sync_error: Option<String>,
    /// Resources that were not up, so not captured.
    pub degraded: Vec<String>,
}

/// A `git push` from a workspace, published: the checkpoint taken first, and
/// the remote it was forwarded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub checkpoint: CheckpointOutcome,
    pub remote: String,
    pub url: String,
    /// The instance's branch, exported to each `branches = true` target.
    pub exports: Vec<BranchExport>,
}

/// What `checkpoint --verify` found: a checkpoint, a real restore of it, a
/// second checkpoint, and whether the two agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub before: CheckpointRecord,
    pub undo: UndoOutcome,
    pub after: CheckpointRecord,
    pub resources: Vec<VerifyResource>,
}

impl VerifyOutcome {
    /// The restore is proven when the middle `undo` completed cleanly and
    /// every resource whose restore could fail landed on the state ref it
    /// started from — which needs `before` to have captured it at all.
    pub fn is_proven(&self) -> bool {
        self.undo.is_complete()
            && self
                .resources
                .iter()
                .filter(|resource| resource.exercised)
                .all(|resource| resource.agree && !resource.degraded)
    }
}

/// One resource's before/after comparison from a verify run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResource {
    pub name: String,
    /// False for `[restore] mode = "none"` or `"external"` — nothing ran
    /// that could prove or disprove anything, so agreement here is trivial.
    pub exercised: bool,
    /// `before` went ahead without this resource, which was not up: there
    /// was no state to compare, so its restore is unproven whatever the
    /// two refs say.
    pub degraded: bool,
    pub before_state_ref: Option<String>,
    pub after_state_ref: Option<String>,
    pub agree: bool,
    /// On an exercised mismatch only: which half of the round trip broke.
    pub cause: Option<MismatchCause>,
    /// On a mismatch between `into_tracker` deposits only: what differs
    /// between two of them, and where both live.
    pub diff: Option<VerifyDiff>,
}

/// The deposit diff shown under a MISMATCH, and which pair it compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyDiff {
    pub pair: DiffPair,
    pub result: std::result::Result<DepositDiff, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPair {
    /// The checkpoint before the restore against the one after it — what
    /// the restore changed, for every cause but an unstable checkpoint.
    BeforeAfter,
    /// `after` against the control taken straight after it. Nothing was
    /// restored between those two, so every line that differs is the
    /// checkpoint's own noise — exactly what an unstable checkpoint needs
    /// to show, without the restore's changes mixed in.
    AfterControl,
}

/// Why `before` and `after` disagree. A mismatch has two opposite fixes —
/// a `[restore]` that lands on the wrong state, or a `[checkpoint]` that
/// never produces the same ref twice — and the two refs alone cannot say
/// which. A control checkpoint can: the resource checkpointed once more
/// straight after `after`, with nothing restored in between. Either way the
/// verify still fails; this is evidence for the person fixing it, never a
/// reason to call a mismatch verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MismatchCause {
    /// The restore command itself failed during the undo, so of course the
    /// state moved. No control was taken.
    RestoreFailed,
    /// The control reproduced `after`: the checkpoint is stable, so the
    /// restore landed on a different state than the one checkpointed.
    RestoreLandedElsewhere,
    /// The control differed from `after` with nothing restored between
    /// them: the checkpoint command does not produce the same ref for the
    /// same state, so this run could not judge the restore at all.
    CheckpointUnstable { control_state_ref: Option<String> },
    /// The control checkpoint could not be taken.
    ControlFailed(String),
}

/// What this undo knows about a resource beyond the checkpoint being
/// restored — the facts a `recompute` needs to decide whether to run at all.
#[derive(Debug, Clone, Copy)]
struct RestoreContext<'a> {
    /// This resource's state ref in the safety checkpoint taken moments ago:
    /// the workspace as it stood before this undo touched anything.
    pre_undo_state_ref: Option<&'a str>,
    /// This undo restored a binding other than the one the instance had:
    /// what `{{bound.trackers}}` and `{{bound.paths}}` render to moved, so
    /// an install keyed on the binding is not the tree the checkpoint
    /// describes even when its identity paths are unchanged.
    binding_moved: bool,
    force_recompute: bool,
}

/// What one `newgit undo` should touch.
#[derive(Debug, Default, Clone)]
pub struct UndoOptions {
    /// Checkpoint id to restore; the latest when absent.
    pub to: Option<String>,
    /// Restore only these resources, leaving source, trackers, and every
    /// other resource untouched. Empty means the whole snapshot.
    pub only: Vec<String>,
    /// Re-run `recompute` restores even when identity is unchanged — for a
    /// tree that has been damaged out from under its lockfile, which the
    /// identity hash cannot see.
    pub force_recompute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    /// Whether `--only` narrowed this undo. A partial undo restores no source
    /// and no tracker content, so it is not a snapshot the instance was ever
    /// in — callers report it differently on purpose.
    pub partial: bool,
    /// The checkpoint that was restored.
    pub restored: CheckpointRecord,
    /// Safety checkpoint taken first — restoring it again is redo.
    pub safety: CheckpointRecord,
    pub trackers: Vec<UndoTrackerOutcome>,
    pub resources: Vec<UndoResourceOutcome>,
    /// Written when any resource restore failed.
    pub recovery_record: Option<Utf8PathBuf>,
    pub warnings: Vec<String>,
}

impl UndoOutcome {
    /// Resources whose restore failed.
    ///
    /// A restore command is not transactional: one that fails halfway (reset
    /// the schema, then fail to load the rows) leaves its resource in neither
    /// the pre-undo state nor the checkpoint state. newgit cannot fix that,
    /// but it must not describe the instance as restored when it happened.
    pub fn failed_resources(&self) -> Vec<&str> {
        self.resources
            .iter()
            .filter(|resource| !resource.ok)
            .map(|resource| resource.name.as_str())
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.recovery_record.is_none() && self.failed_resources().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoTrackerOutcome {
    pub name: String,
    /// None means the checkpoint had no content: owned paths were cleared.
    pub rev: Option<String>,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoResourceOutcome {
    /// What the restore did, for display: `none`, `recompute(prepare)`,
    /// `command`, `external (no-op)`, `not restored` for one whose checkpoint
    /// holds no state for it; `+ restarted` when a process came back.
    pub action: String,
    pub name: String,
    /// Whether the restore action itself succeeded. A restore that runs
    /// nothing always does, so this says nothing about whether the resource
    /// is up — that is `status`.
    pub ok: bool,
    /// The status the resource is left in, as `newgit status` will report
    /// it. A restore that ran nothing leaves the status it found, so a
    /// `prepare` that failed before the undo is still `Failed` after it,
    /// with `ok` true; a restart that failed after an ok restore is
    /// `Failed` too.
    pub status: ResourceStatus,
    /// The resource was running at checkpoint time and its restore was ok,
    /// but starting it again failed. `ok` stays true — the restore did what
    /// it set out to — and `status` is `Failed`.
    pub restart_failed: bool,
    /// Dependencies not `Ready` once the undo is done, as `newgit status`
    /// derives `blocked(..)` from them.
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    pub rev: String,
    pub files: usize,
    /// False when the content was identical to the previous binding.
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedReport {
    pub rev: String,
    pub files: usize,
    /// False when the lane head already pointed at this content.
    pub changed: bool,
    /// Declared paths with nothing on disk in the store repo. Reported rather
    /// than silently skipped: a lane seeded from half its paths is a bug you
    /// want to hear about now, not at the first `spawn`.
    pub missing_paths: Vec<Utf8PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub rev: String,
    pub files: usize,
    /// Where the pre-restore content was saved, when it differed.
    pub safety_rev: Option<String>,
    /// Re-renders that failed after the content moved. Reported rather than
    /// swallowed: the workspace is then running on committed defaults, which
    /// is a different thing from what the instance was bound to.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTrackerOutcome {
    pub tracker: String,
    pub rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddTrackerOutcome {
    pub path: Utf8PathBuf,
    pub ignored_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPathsOutcome {
    pub path: Utf8PathBuf,
    pub added_paths: Vec<Utf8PathBuf>,
    pub ignored_patterns: Vec<String>,
    /// At least one newly tracked path already has content in the store repo,
    /// so the lane can be seeded from it without spawning anything.
    pub seedable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResourceOutcome {
    pub path: Utf8PathBuf,
    /// Companion definitions created because the template depends on them.
    pub companions_created: Vec<Utf8PathBuf>,
    /// Tracker lanes created because the template deposits into them.
    pub trackers_created: Vec<Utf8PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveTrackerOutcome {
    pub path: Utf8PathBuf,
    /// Pattern lines dropped from the store repo's `.gitignore`.
    pub gitignore_removed: Vec<String>,
    /// Live instances whose workspace had this tracker's paths cleared from
    /// `.git/info/exclude`.
    pub workspaces_cleared: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveResourceOutcome {
    pub path: Utf8PathBuf,
    /// `--force` only: instances whose binding was dropped and ports released.
    pub unbound_instances: Vec<String>,
}

impl BranchManager {
    pub fn open(store: MetadataStore) -> Result<Self> {
        store.ensure_initialized()?;
        let config = store.load_config()?;
        let source = GitSource::open(&store.paths().project_root, config.project.source)?;
        let mut trackers = store.load_tracker_definitions()?;
        // A project initialized before `root` or `project` existed has no
        // file for them; it still has both — everything its other trackers
        // do not claim, and its own definitions.
        if !trackers.iter().any(TrackerDefinition::owns_everything) {
            trackers.push(TrackerDefinition::root());
        }
        if !trackers
            .iter()
            .any(|definition| definition.name == crate::tracker::PROJECT_TRACKER)
        {
            trackers.push(TrackerDefinition::project());
        }
        if !trackers
            .iter()
            .any(|definition| definition.name == crate::tracker::ACCESS_TRACKER)
        {
            trackers.push(TrackerDefinition::access());
        }
        crate::tracker::resolve_ownership(&mut trackers)?;
        let root = trackers.remove(
            trackers
                .iter()
                .position(TrackerDefinition::owns_everything)
                .expect("root was just ensured"),
        );
        // Code trackers are composed into workspaces; the rest are captured
        // from files.
        let (code, trackers): (Vec<_>, Vec<_>) =
            trackers.into_iter().partition(TrackerDefinition::is_code);
        let resources = store.load_resource_definitions()?;
        let tracker_names: BTreeSet<String> = trackers.iter().map(|t| t.name.clone()).collect();
        let graph = resolve_graph(&resources, &tracker_names);
        // Only built when some resource could actually use it: constructing
        // it ensures the local-ignore rule, and a project with no install to
        // share has no business gaining an `installs/` line.
        let installs = if resources
            .iter()
            .any(|definition| !definition.identity_produces().is_empty())
        {
            store.install_store()
        } else {
            InstallStore::at(store.paths().installs.clone())
        };
        let remote = config
            .remote
            .as_ref()
            .zip(config.remote_project())
            .map(|(section, project)| RemoteLink::http(project, &section.url));
        Ok(Self {
            store,
            installs,
            config,
            source,
            trackers,
            resources,
            graph,
            remote,
            root,
            code,
            locks: Mutex::new(BTreeMap::new()),
        })
    }

    /// Hold `slug`'s lock for the rest of the command: every command that
    /// changes an instance takes it first, so two of them cannot interleave
    /// their load-change-save (#212). Waits for another process holding it.
    /// Within this manager it is reentrant — `checkpoint_verify` calling
    /// `undo` gets the lock it already holds — so a nested call never waits
    /// for its own caller.
    pub fn lock_instance(&self, slug: &str, name: &str) -> Result<HeldLock> {
        if let Some(held) = self.held_lock(slug) {
            return Ok(HeldLock(held));
        }
        let lock = Arc::new(self.store.lock_instance(slug, name)?);
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(slug.to_owned(), Arc::downgrade(&lock));
        Ok(HeldLock(lock))
    }

    /// Lock the instance `instance` names and load its record under the
    /// lock: the shape of every command that will save that record back.
    fn locked_branch(&self, instance: &str) -> Result<(HeldLock, BranchInstance)> {
        // A name resolves to its slug without the lock; the slug is what the
        // lock is keyed on, and only the record's contents can change.
        let found = self.store.find_branch(instance)?;
        let lock = self.lock_instance(&found.slug, &found.name)?;
        let branch = self.store.find_branch(&found.slug)?;
        Ok((lock, branch))
    }

    /// Every instance lock this manager holds right now.
    fn held_locks(&self) -> Vec<Arc<InstanceLock>> {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn held_lock(&self, slug: &str) -> Option<Arc<InstanceLock>> {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(slug)
            .and_then(Weak::upgrade)
    }

    /// The shared store of built trees. One per manager — see the field.
    fn installs(&self) -> &InstallStore {
        &self.installs
    }

    /// Ways the resource graph is incomplete. An empty slice means it resolves.
    pub fn graph_problems(&self) -> &[GraphProblem] {
        &self.graph.problems
    }

    /// Edges read out of `{{exports.*}}` rather than declared in
    /// `depends_on`, so `newgit resource list` can show them.
    pub fn data_edges(&self) -> &DataEdges {
        &self.graph.data_edges
    }

    /// Use `link` as the project's remote instead of the one `[remote]`
    /// names — how tests stand in a fake.
    pub fn with_remote(mut self, link: RemoteLink) -> Self {
        self.remote = Some(link);
        self
    }

    pub fn remote_link(&self) -> Option<&RemoteLink> {
        self.remote.as_ref()
    }

    /// The gate for commands that act on the graph — `spawn`, `run`, `action`,
    /// `checkpoint`, `undo`. Commands that *build* the graph
    /// (`tracker create`, `tracker track`, `resource add`) must not call this:
    /// they are how an incomplete graph gets completed. Nor may teardown,
    /// which is the way out of one — see [`Self::remove`].
    pub fn require_resolvable_graph(&self) -> Result<()> {
        match self.graph.problems.first() {
            Some(problem) => Err(problem.clone().into_error()),
            None => Ok(()),
        }
    }

    pub fn store(&self) -> &MetadataStore {
        &self.store
    }

    /// `{{scripts}}` — the store's `.newgit/scripts/`.
    ///
    /// A resource definition is read from the store, but anything it shells
    /// out to used to be read from the workspace, where it is subject to
    /// source materialization: the two halves of one definition lived under
    /// different rules, and only the TOML half was editable in place. A
    /// script here resolves like the definition that calls it, so iterating on
    /// a `prepare` does not mean committing every attempt.
    fn scripts_dir(&self) -> &str {
        self.store.paths().scripts.as_str()
    }

    pub fn tracker_definitions(&self) -> &[TrackerDefinition] {
        &self.trackers
    }

    /// The root tracker: owns everything no other tracker claims.
    pub fn root_tracker(&self) -> &TrackerDefinition {
        &self.root
    }

    /// Code trackers: committed, with their own histories, composed into
    /// workspaces. `project` is always one.
    pub fn code_trackers(&self) -> &[TrackerDefinition] {
        &self.code
    }

    /// Create a code tracker owning `paths`, its history carved out of
    /// whichever tracker owns them now (usually root): every commit that
    /// touched them, marked with its source, so composing the two still
    /// reproduces the original history.
    pub fn create_code_tracker(
        &self,
        name: &str,
        audience: &str,
        paths: &[Utf8PathBuf],
    ) -> Result<CreateCodeTrackerOutcome> {
        validate_name(name)?;
        self.refuse_root(name, "it always exists")?;
        if paths.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "a code tracker's paths are set when it is created: `newgit tracker create {name} \
                 --code --path <path>`"
            )));
        }
        let definition = TrackerDefinition::code(name, audience.to_owned(), paths)?;

        // Who owns these paths now: one tracker, so there is one history to
        // carve from.
        let mut owners = BTreeSet::new();
        for path in &definition.paths {
            let owner = self
                .code
                .iter()
                .chain(std::iter::once(&self.root))
                .filter(|candidate| candidate.owns(path))
                .map(|candidate| candidate.name.clone())
                .next()
                .unwrap_or_else(|| self.root.name.clone());
            owners.insert(owner);
        }
        if owners.len() > 1 {
            return Err(NewgitError::Unsupported(format!(
                "`{name}`'s paths belong to {} now; carve one tracker's paths at a time",
                owners
                    .iter()
                    .map(|owner| format!("`{owner}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )));
        }
        let source = owners.into_iter().next().expect("one owner");

        let mut all: Vec<TrackerDefinition> =
            self.trackers.iter().chain(&self.code).cloned().collect();
        all.push(self.root.clone());
        all.push(definition.clone());
        crate::tracker::resolve_ownership(&mut all)?;

        let tip = self.carve_from(&source, &definition)?;
        let path = self.store.create_tracker_definition(&definition)?;
        let commits = match &tip {
            Some(tip) => self.source.commit_count(tip)?,
            None => 0,
        };
        Ok(CreateCodeTrackerOutcome {
            path,
            source,
            commits,
        })
    }

    /// Carve `definition`'s history out of `source`'s into the tracker
    /// store: every branch of it — `main`, the project root's
    /// `instances/main`, and every instance's — so no instance is left with
    /// work that still files those paths under `source`. Carving is
    /// deterministic, so the branches share every commit up to where they
    /// part. Both repositories end up referencing what they hold: the carved
    /// commits are made in the project repo under its own refs, then fetched
    /// into the store. The next projection recomposes every view, so their
    /// notes name the new tracker.
    ///
    /// Returns the carved `main`'s tip.
    fn carve_from(&self, source: &str, definition: &TrackerDefinition) -> Result<Option<String>> {
        self.fetch_code_trackers()?;
        let mut carved_main = None;
        let mut carved = Vec::new();
        for branch in self.code_branches(source)? {
            let source_tip = self.source.rev_parse(&local_tracker_ref(source, &branch))?;
            let target = local_tracker_ref(&definition.name, &branch);
            let tip = crate::compose::carve(
                &self.store.paths().project_root,
                &source_tip,
                definition,
                &target,
            )?;
            if branch == "main" {
                carved_main = tip.clone();
            }
            if tip.is_some() {
                carved.push(branch);
            }
        }
        if !carved.is_empty() {
            let branches: Vec<&str> = carved.iter().map(String::as_str).collect();
            self.publish_code_refs(&definition.name, &branches)?;
        }
        Ok(carved_main)
    }

    /// Bring every code tracker's branches from the store into the project
    /// repo (`refs/newgit/trackers/<tracker>/…`), where composing and
    /// splitting run — one fetch. Code trackers only: a secret tracker's
    /// content never enters the project repo, which is the one `git push
    /// --mirror` publishes and every workspace clone hardlinks.
    fn fetch_code_trackers(&self) -> Result<()> {
        let store = self.store.paths().tracker_store.clone();
        crate::tracker_repo::ensure_store(&store)?;
        let refspecs: Vec<String> = self
            .code_definitions()
            .iter()
            .map(|definition| {
                format!(
                    "+refs/trackers/{0}/*:refs/newgit/trackers/{0}/*",
                    definition.name
                )
            })
            .collect();
        GitSource::fetch_refs(
            &["-C", self.store.paths().project_root.as_str()],
            store.as_str(),
            &refspecs,
            None,
        )
    }

    /// A code tracker's branches in the project repo: `main`,
    /// `instances/<slug>`.
    fn code_branches(&self, tracker: &str) -> Result<Vec<String>> {
        let prefix = format!("refs/newgit/trackers/{tracker}/");
        Ok(crate::source::run_git_env(
            &[
                "-C",
                self.store.paths().project_root.as_str(),
                "for-each-ref",
                "--format=%(refname)",
                &prefix,
            ],
            &[],
        )?
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(ToOwned::to_owned)
        .collect())
    }

    /// One code tracker branch, brought from the store into the project
    /// repo: its commit there, or `None` when the store has no such branch.
    fn fetch_code_ref(&self, tracker: &str, branch: &str) -> Result<Option<String>> {
        self.fetch_code_trackers()?;
        Ok(self
            .source
            .rev_parse(&local_tracker_ref(tracker, branch))
            .ok())
    }

    /// The reverse: a code tracker's branches, made in the project repo,
    /// fetched into the store under the tracker's own refs.
    fn publish_code_refs(&self, tracker: &str, branches: &[&str]) -> Result<()> {
        let store = self.store.paths().tracker_store.clone();
        crate::tracker_repo::ensure_store(&store)?;
        let refspecs: Vec<String> = branches
            .iter()
            .map(|branch| {
                format!(
                    "+{}:refs/trackers/{tracker}/{branch}",
                    local_tracker_ref(tracker, branch)
                )
            })
            .collect();
        GitSource::fetch_refs(
            &[&format!("--git-dir={store}")],
            self.store.paths().project_root.as_str(),
            &refspecs,
            None,
        )
    }

    /// Root and every code tracker: what a view composes.
    fn code_definitions(&self) -> Vec<TrackerDefinition> {
        std::iter::once(self.root.clone())
            .chain(self.code.iter().cloned())
            .collect()
    }

    /// Every definition, secret trackers included: what names the owner of
    /// a path nothing bound owns.
    fn all_definitions(&self) -> Vec<TrackerDefinition> {
        self.code_definitions()
            .into_iter()
            .chain(self.trackers.iter().cloned())
            .collect()
    }

    /// The code tracker `definition`'s paths belong to until it is carved:
    /// the most specific other code tracker claiming them, usually root.
    fn carve_source(&self, definition: &TrackerDefinition) -> String {
        let Some(path) = definition.paths.first() else {
            return self.root.name.clone();
        };
        let claim = |other: &TrackerDefinition| {
            other
                .paths
                .iter()
                .filter(|owned| owned.as_str() != "." && path.starts_with(owned))
                .map(|owned| owned.as_str().len())
                .max()
        };
        self.code
            .iter()
            .filter(|other| other.name != definition.name)
            .filter_map(|other| claim(other).map(|length| (length, other)))
            .max_by_key(|(length, _)| *length)
            .map_or_else(|| self.root.name.clone(), |(_, other)| other.name.clone())
    }

    /// Get the project repo ready to compose or split: the code trackers'
    /// branches are in the project repo, every code tracker has its history
    /// (carved out of its owner the first time it is needed — a tracker
    /// defined by hand, or `project`), and every view's commits carry notes
    /// saying which tracker commits they stand for — recomposed when there
    /// are none yet, or when the set of code trackers with history changed.
    fn prepare_projection(&self) -> Result<()> {
        self.fetch_code_trackers()?;
        // A tracker carves out of the one claiming a strict prefix of its
        // path, so shorter claims go first: `project` (`.newgit`) has its
        // history before `access` (`.newgit/trackers`) carves out of it.
        let mut by_claim: Vec<&TrackerDefinition> = self.code.iter().collect();
        by_claim.sort_by_key(|definition| {
            definition
                .paths
                .first()
                .map_or(0, |path| path.as_str().len())
        });
        for definition in by_claim {
            if !self.code_branches(&definition.name)?.is_empty() {
                continue;
            }
            // Carving reads the whole history: once is enough. A tracker it
            // found nothing for gets its first commit from a split.
            let marker = format!("refs/newgit/carve-checked/{}", definition.name);
            if self.source.rev_parse(&marker).is_ok() {
                continue;
            }
            let source = self.carve_source(definition);
            let Some(source_main) = self.fetch_code_ref(&source, "main")? else {
                continue;
            };
            self.carve_from(&source, definition)?;
            self.source.update_ref(&marker, &source_main)?;
        }

        let mut names = String::new();
        for definition in self.code_definitions() {
            if !self.code_branches(&definition.name)?.is_empty() {
                names.push_str(&format!("{}\n", definition.name));
            }
        }
        let composed_file = self.store.paths().local.join("composed");
        let composed = std::fs::read_to_string(&composed_file).unwrap_or_default();
        let noted = self.source.rev_parse(crate::compose::NOTES_REF).is_ok();
        if composed == names && noted {
            return Ok(());
        }
        self.recompose_views()?;
        std::fs::write(&composed_file, names)
            .map_err(|source| NewgitError::io(&composed_file, source))
    }

    /// Compose every view again — the project root's and each instance's —
    /// from its tracker branches (each tracker's `instances/<slug>`, else
    /// its `main`). Where the result is the commits that are already there,
    /// nothing is written but their notes, which now name every tracker.
    fn recompose_views(&self) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let code = self.code_definitions();
        let mut slugs = BTreeSet::from(["main".to_owned()]);
        for definition in &code {
            for branch in self.code_branches(&definition.name)? {
                if let Some(slug) = branch.strip_prefix("instances/") {
                    slugs.insert(slug.to_owned());
                }
            }
        }
        let all = self.all_definitions();
        for slug in slugs {
            let mut tips = Vec::new();
            for definition in &code {
                let tip = [format!("instances/{slug}"), "main".to_owned()]
                    .iter()
                    .find_map(|branch| {
                        self.source
                            .rev_parse(&local_tracker_ref(&definition.name, branch))
                            .ok()
                    });
                if let Some(commit) = tip {
                    tips.push(crate::compose::TrackerTip { definition, commit });
                }
            }
            if tips.is_empty() {
                continue;
            }
            crate::compose::compose(
                &project,
                &format!("refs/newgit/views/instances/{slug}"),
                &tips,
                &all,
            )?;
        }
        Ok(())
    }

    /// Split instance `slug`'s commits up to `tip` into its tracker
    /// branches (`instances/<slug>`), in the project repo and the store.
    /// Every bound tracker's branch is set, moved or not: the branch is the
    /// instance's state, so a reset moves it back.
    ///
    /// A path whose tracker this instance has not bound refuses the whole
    /// split, naming it. Secrets are checked where work leaves, by
    /// [`Self::refuse_secrets`], against exactly what is sent.
    fn split_instance(&self, slug: &str, tip: &str) -> Result<usize> {
        let project = self.store.paths().project_root.clone();
        self.prepare_projection()?;
        let base = crate::compose::find_base(&project, tip)?.ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "newgit cannot tell which tracker commits {} stands for: its history shares no \
                 commit with the trackers' histories (a `.newgit/` from before trackers had \
                 histories, or from a plain `git clone`, gets them from `newgit init`)",
                &tip[..tip.len().min(12)]
            ))
        })?;
        let code = self.code_definitions();
        let dropped = if slug == PROJECT_ROOT_INSTANCE {
            BTreeSet::new()
        } else {
            self.store
                .find_branch(slug)
                .map(|branch| branch.dropped)
                .unwrap_or_default()
        };
        // Only what the instance binds: a dropped tracker is out of its
        // scope, so work in it refuses, naming `newgit tracker use`.
        let bound: Vec<&TrackerDefinition> = code
            .iter()
            .filter(|definition| !dropped.contains(&definition.name))
            .collect();
        let outcome = crate::compose::split(&project, &base, tip, &bound, &self.all_definitions())
            .map_err(|error| match error {
                crate::compose::SplitError::Refused(refusal) => {
                    NewgitError::SplitRefused(refusal.to_string())
                }
                crate::compose::SplitError::Git(error) => error,
            })?;
        let branch = format!("instances/{slug}");
        for (tracker, commit) in &outcome.state {
            if !code.iter().any(|definition| &definition.name == tracker) {
                continue;
            }
            self.source
                .update_ref(&local_tracker_ref(tracker, &branch), commit)?;
            self.publish_code_refs(tracker, &[&branch])?;
        }
        Ok(outcome.made.len())
    }

    /// Refuse when any commit `rev_list` selects (in `repo`) adds or changes
    /// a file a secret tracker owns — every commit, every parent of a merge.
    /// The one gate for secrets: a push runs it over everything it sends,
    /// a sync over everything it uploads.
    fn refuse_secrets(
        &self,
        repo: &Utf8Path,
        rev_list: &[&str],
        env: &[(String, String)],
    ) -> Result<()> {
        let all = self.all_definitions();
        match crate::compose::find_secret(repo, rev_list, env, &all)? {
            None => Ok(()),
            Some((commit, path, tracker)) => Err(NewgitError::SplitRefused(format!(
                "commit {} changes `{path}`, which belongs to the secret tracker `{tracker}`, \
                 and secrets never leave this machine through Git. Take it out of that commit \
                 (`git rm --cached {path}` then `git commit --amend`, or an interactive rebase \
                 if the commit is older) and try again",
                &commit[..commit.len().min(12)]
            ))),
        }
    }

    /// Put branch `branch` here, composed from the code trackers' `main`s in
    /// the tracker store: the project repo's `refs/heads/<branch>` and its
    /// working tree become the composed view, and that branch is the project
    /// root's own. What `newgit clone` ends with. Returns the view's commit.
    pub fn check_out_composed(&self, branch: &str) -> Result<String> {
        self.prepare_projection()?;
        let view = self.source.rev_parse(&format!(
            "refs/newgit/views/instances/{PROJECT_ROOT_INSTANCE}"
        ))?;
        let root = self.store.paths().project_root.clone();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", root.as_str()];
            full.extend_from_slice(args);
            crate::source::run_git_env(&full, &[])
        };
        git(&["update-ref", &format!("refs/heads/{branch}"), &view])?;
        git(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?;
        git(&["reset", "-q", "--hard", &view])?;
        git(&["update-ref", ADOPTED_REF, &view])?;
        let file = self.store.paths().project_branch_file();
        std::fs::write(&file, format!("{branch}\n"))
            .map_err(|source| NewgitError::io(&file, source))?;
        Ok(view)
    }

    /// Refuse when anything about to be published from `rev` — every commit
    /// behind it, every parent of every merge, back to the history newgit
    /// adopted (as `newgit init` found it, or a clone received it) — adds or
    /// changes a secret tracker's file. Every commit pushed is checked, view
    /// commits and tracker commits alike: a tracker's `main` can hold work
    /// merged here that no branch of the project root has. The
    /// one gate every path that sends work to the remote passes: sync, push,
    /// merge, and a project's first publication.
    fn refuse_unpublished(&self, revs: &[&str]) -> Result<()> {
        if revs.is_empty() {
            return Ok(());
        }
        let root = self.store.paths().project_root.clone();
        let adopted = self.source.rev_parse(ADOPTED_REF).ok();
        let mut args = revs.to_vec();
        args.push("--not");
        args.extend(adopted.as_deref());
        self.refuse_secrets(&root, &args, &[])
    }

    /// The project root is the instance `main` (docs/design/trackers.md): bring
    /// its tracker branches up to its own branch — the one `newgit init`
    /// found checked out — not whatever it has checked out now.
    pub fn sync_project_root(&self) -> Result<usize> {
        let file = self.store.paths().project_branch_file();
        let branch = match std::fs::read_to_string(&file) {
            Ok(branch) => branch.trim().to_owned(),
            Err(_) => {
                let Ok(branch) = crate::source::run_git_env(
                    &[
                        "-C",
                        self.store.paths().project_root.as_str(),
                        "symbolic-ref",
                        "--short",
                        "-q",
                        "HEAD",
                    ],
                    &[],
                ) else {
                    return Ok(0);
                };
                let branch = branch.trim().to_owned();
                std::fs::write(&file, format!("{branch}\n"))
                    .map_err(|source| NewgitError::io(&file, source))?;
                branch
            }
        };
        let Ok(tip) = self.source.rev_parse(&format!("refs/heads/{branch}")) else {
            return Ok(0);
        };
        self.split_instance(PROJECT_ROOT_INSTANCE, &tip)
    }

    /// Where `tracker`'s history is for `instance` (its branch, once its
    /// work has been split), or its `main`: the store and the ref, for `git
    /// log`. An instance's branch is brought up to its last checkpoint
    /// first.
    pub fn tracker_log_target(
        &self,
        tracker: &str,
        instance: Option<&str>,
    ) -> Result<(Utf8PathBuf, String)> {
        let store = self.store.paths().tracker_store.clone();
        let known = tracker == self.root.name
            || self
                .code
                .iter()
                .any(|definition| definition.name == tracker)
            || self
                .trackers
                .iter()
                .any(|definition| definition.name == tracker);
        if !known {
            return Err(NewgitError::UnknownTracker(tracker.to_owned()));
        }
        let is_code = tracker == self.root.name
            || self
                .code
                .iter()
                .any(|definition| definition.name == tracker);
        let reference = match instance {
            Some(instance) => {
                let branch = self.store.find_branch(instance)?;
                if is_code {
                    let tip = self
                        .source
                        .rev_parse(&format!("refs/heads/{}", branch.source_ref))?;
                    self.split_instance(&branch.slug, &tip)?;
                }
                format!("refs/trackers/{tracker}/instances/{}", branch.slug)
            }
            None => {
                if is_code {
                    self.sync_project_root()?;
                    format!("refs/trackers/{tracker}/instances/main")
                } else {
                    format!("refs/trackers/{tracker}/main")
                }
            }
        };
        Ok((store, reference))
    }

    /// Refuse a tracker command aimed at root or a code tracker, saying what
    /// to do instead: both are committed with Git.
    fn refuse_root(&self, tracker: &str, action: &str) -> Result<()> {
        if self
            .code
            .iter()
            .any(|definition| definition.name == tracker)
        {
            return Err(NewgitError::Unsupported(format!(
                "`{tracker}` is a code tracker: its content is committed, and newgit splits your \
                 commits into it — {action}"
            )));
        }
        if tracker == self.root.name {
            return Err(NewgitError::Unsupported(format!(
                "`{tracker}` owns everything no other tracker claims, and its history is this \
                 project's own Git history: {action}"
            )));
        }
        Ok(())
    }

    /// Check that `branch` works right now (see [`Readiness`]).
    pub fn readiness(&self, branch: &BranchInstance) -> Readiness {
        let mut problems = Vec::new();
        if !branch.workspace_path.is_dir() {
            problems.push(format!(
                "its workspace is missing ({})",
                branch.workspace_path
            ));
            return Readiness { problems };
        }
        let supervisor = self.supervisor(branch);
        for (name, binding) in &branch.resources {
            let Ok(definition) = self.resource_definition(name) else {
                continue;
            };
            match binding.status {
                ResourceStatus::Ready => {}
                ResourceStatus::Failed => {
                    problems.push(format!(
                        "`{name}` failed — retry with `newgit action {name}.prepare`; its logs are \
                         under {}",
                        self.store.paths().logs.join(&branch.slug)
                    ));
                    continue;
                }
                // Not always a dependency's doing: ports are claimed in the
                // record before `prepare` runs, so a resource is `Pending`
                // while that runs and after a spawn that died during it, and
                // it stays so once a dependency that failed comes up.
                ResourceStatus::Pending
                    if self.blocked_dependencies(branch, definition).is_empty() =>
                {
                    problems.push(format!(
                        "`{name}` has not been prepared — unless newgit is still preparing it, \
                         run `newgit action {name}.prepare`"
                    ));
                    continue;
                }
                ResourceStatus::Pending => {
                    problems.push(format!(
                        "`{name}` never came up (a dependency of it did not)"
                    ));
                    continue;
                }
            }
            for path in definition.identity_produces() {
                if !branch.workspace_path.join(path).exists() {
                    problems.push(format!(
                        "`{name}`'s {path} is missing — rebuild with `newgit action {name}.prepare`"
                    ));
                }
            }
            if supervisor.running_pid(name).is_some() {
                for (port_name, port) in &binding.resolved_ports {
                    let address = std::net::SocketAddr::from(([127, 0, 0, 1], *port));
                    let answering = std::net::TcpStream::connect_timeout(
                        &address,
                        std::time::Duration::from_millis(300),
                    )
                    .is_ok();
                    if !answering {
                        problems.push(format!(
                            "`{name}` is running but its port {port_name} ({port}) is not \
                             answering yet"
                        ));
                    }
                }
            }
        }
        Readiness { problems }
    }

    /// Whether every tree `definition` builds is in the workspace.
    fn produced_present(&self, branch: &BranchInstance, definition: &ResourceDefinition) -> bool {
        definition
            .identity_produces()
            .iter()
            .all(|path| branch.workspace_path.join(path).exists())
    }

    /// The tracker-path invariant, checked loudly: non-source tracker paths
    /// must be gitignored unless deliberately dual-tracked with source.
    pub fn gitignore_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for definition in &self.trackers {
            for path in &definition.paths {
                if let Ok(true) = self.source.is_tracked(path.as_str()) {
                    warnings.push(format!(
                        "tracker `{}` owns `{path}`, which Git also tracks (dual-tracked): \
                         branch-local content will show as modifications and can be committed \
                         into source history — untrack it with `git rm --cached {path}` unless \
                         this is deliberate",
                        definition.name
                    ));
                } else if let Ok(false) = self.source.is_ignored(path.as_str()) {
                    warnings.push(format!(
                        "tracker `{}` owns `{path}` but the store repo does not gitignore it; \
                         agents may commit it into source history (fine only if deliberately \
                         dual-tracked)",
                        definition.name
                    ));
                }
            }
        }
        warnings
    }

    pub fn spawn(&self, name: &str, from: Option<&str>) -> Result<SpawnOutcome> {
        self.spawn_binding(name, from, &BindingChoice::default())
    }

    /// Spawn, binding what `choice` asks on top of the defaults (see
    /// [`Self::choose_binding`]).
    pub fn spawn_binding(
        &self,
        name: &str,
        from: Option<&str>,
        choice: &BindingChoice,
    ) -> Result<SpawnOutcome> {
        self.spawn_inner(name, from, choice, true)
    }

    /// The spawn itself. `autostart` false leaves the `autostart` actions to
    /// the caller: a spawn from a checkpoint starts them after its restore.
    fn spawn_inner(
        &self,
        name: &str,
        from: Option<&str>,
        choice: &BindingChoice,
        autostart: bool,
    ) -> Result<SpawnOutcome> {
        self.require_resolvable_graph()?;
        if choice.private && self.remote.is_none() {
            return Err(NewgitError::Unsupported(
                "a private instance is private on the project's newgit remote, and this project \
                 has none: nothing here could keep it private, and a `git push` would publish \
                 it. `newgit remote add` first"
                    .to_owned(),
            ));
        }
        let dropped = self.choose_binding(choice)?;
        validate_name(name)?;
        if branch_slug(name) == PROJECT_ROOT_INSTANCE {
            return Err(NewgitError::Unsupported(format!(
                "`{name}` is the project root's own instance — its tracker branches are \
                 `instances/{PROJECT_ROOT_INSTANCE}`; pick another name"
            )));
        }

        let slug = branch_slug(name);
        let _lock = self.lock_instance(&slug, name)?;
        let record_path = self.store.branch_record_path(&slug);
        if record_path.exists() {
            return Err(NewgitError::BranchInstanceExists {
                name: name.to_owned(),
                path: record_path,
            });
        }
        // The project root is the instance `main`; what it has committed
        // reaches its tracker branches before anything is composed from them.
        let projection_warning = self
            .sync_project_root()
            .err()
            .map(|error| error.to_string());

        let mut base = None;
        let created_source_branch = if self.source.branch_exists(name)? {
            if let Some(base) = from {
                return Err(NewgitError::Unsupported(format!(
                    "source branch `{name}` already exists; `--from {base}` only applies when \
                     creating a new branch"
                )));
            }
            false
        } else {
            let requested = from.unwrap_or("HEAD");
            let base_rev = self.source.rev_parse(requested)?;
            self.source.create_branch(name, &base_rev)?;
            // Prefer a branch name over the literal request ("HEAD") so
            // `status` has something readable to report drift against; a
            // bare SHA or tag names a fixed point, so there is nothing to
            // compare a moving base to and `resolve_branch_name` says so.
            if let Some(base_ref) = self.source.resolve_branch_name(requested)? {
                base = Some((base_ref, base_rev));
            }
            true
        };

        let source_rev = self.source.rev_parse(&format!("refs/heads/{name}"))?;
        let workspace_path = self
            .config
            .workspace_root(&self.store.paths().project_root)
            .join(&slug);
        let mut branch = BranchInstance::new(name, name, source_rev, workspace_path)?;
        branch.private = choice.private;
        branch.created_source_branch = created_source_branch;
        if let Some((base_ref, base_rev)) = base {
            branch = branch.with_base(base_ref, base_rev);
        }

        branch.dropped = dropped;
        let template_warning = self
            .materializer()
            .materialize_reporting(&self.source, &branch)?;
        if !branch.dropped.is_empty() {
            self.apply_sparse(&branch)?;
        }

        // Before any lane content lands, make the clone's Git ignore the
        // paths trackers own and the trees resources build — otherwise both
        // arrive as untracked files an agent, or a checkpoint's `git add
        // -A`, can commit into source history.
        self.ensure_workspace_excludes(&branch)?;
        self.ensure_workspace_push(&branch)?;

        let mut tracker_outcomes = Vec::new();
        for definition in &self.trackers {
            if branch.dropped.contains(&definition.name) {
                continue;
            }
            let outcome = self.bind_tracker(&mut branch, definition, false)?;
            tracker_outcomes.push(outcome);
        }

        // Read before the record is written, so this reflects what was on
        // disk *before* this instance existed: anything found here was left
        // by a previous instance of the same name.
        let log = CheckpointLog::new(self.store.checkpoint_dir(&branch.slug), &branch.name);
        let existing = log.list()?.len();
        let inherited_checkpoints = if existing > 0 {
            Some(InheritedCheckpoints {
                existing,
                next_id: log.next_id()?,
            })
        } else {
            None
        };

        // Written before the resources bind, not after: the ports they are
        // given are claimed by writing them into this record, before their
        // `prepare` runs (see `claim_ports`).
        let record_path = self.store.create_branch_record(&branch)?;
        let resource_outcomes = self.bind_resources(&mut branch)?;
        self.store.save_branch_record(&branch)?;
        let started = if autostart {
            self.autostart(&branch, None)
        } else {
            Vec::new()
        };

        // The instance exists locally whatever the remote says: a failure to
        // register is reported, not fatal, and retried at the next checkpoint.
        let remote_warning = self
            .register_remote(&mut branch)
            .err()
            .map(|error| error.to_string());

        Ok(SpawnOutcome {
            branch,
            record_path,
            created_source_branch,
            trackers: tracker_outcomes,
            resources: resource_outcomes,
            started,
            inherited_checkpoints,
            remote_warning,
            projection_warning,
            template_warning,
        })
    }

    /// Put an instance on the project's remote, if it has one and the instance
    /// is not there yet: register it (which forks nothing — an instance forks
    /// a code tracker the first time it changes it — and mints the
    /// workspace's own token), keep the token in the workspace's `.git/`, and
    /// point the workspace's `origin` at the project's Git endpoint.
    ///
    /// Idempotent: a registered instance is left alone, so callers retry it
    /// freely.
    fn register_remote(&self, branch: &mut BranchInstance) -> Result<()> {
        let Some(link) = &self.remote else {
            return Ok(());
        };
        if branch.remote.is_some() {
            return Ok(());
        }
        let registered = link.connect(Acting::Login)?.register_instance(
            &link.project,
            &branch.slug,
            &branch.source_rev,
            branch.private,
        )?;
        // Never assume it: a remote that does not know private instances
        // registers a public one. Stopping here keeps every sync of this
        // instance from running, so nothing of it leaves.
        if branch.private && !registered.instance.private {
            return Err(NewgitError::Unsupported(format!(
                "{} registered `{}` as an ordinary instance, not a private one (it predates \
                 private instances), so nothing of it will be synced there",
                link.project, branch.name
            )));
        }
        crate::remote::write_private(
            &crate::remote::workspace_token_path(&branch.workspace_path),
            &registered.token,
        )?;
        branch.remote = Some(RemoteBinding {
            project: link.project.clone(),
            endpoint: registered.endpoint.clone(),
        });
        branch.updated_at = Utc::now();
        self.store.save_branch_record(branch)?;
        // `origin` now fetches the endpoint, and Git asks newgit for the
        // password — the workspace's own token.
        self.ensure_workspace_push(branch)?;
        GitSource::workspace_use_credential_helper(
            &branch.workspace_path,
            &[&registered.endpoint],
            &format!("!{} credential", sh_quote(&newgit_program())),
        )
    }

    /// Spawn someone's checkpoint here: a new instance named `name` that comes
    /// up as the checkpoint was — source (uncommitted work included), tracker
    /// content, and resource state such as a database restored from its dump.
    ///
    /// Trackers resolve for the person spawning, not the author: a per-user
    /// tracker gets *their own* content, a shared one the checkpoint's exact
    /// rev, and one they may not read nothing — each said in `warnings`.
    ///
    /// Built from parts that already exist: fetch, `spawn` at the checkpoint's
    /// head, import the record into the new instance's own history, `undo`
    /// to it.
    pub fn spawn_from_checkpoint(
        &self,
        name: &str,
        from_instance: &str,
        checkpoint_id: &str,
    ) -> Result<SpawnFromCheckpointOutcome> {
        let link = self.remote.as_ref().ok_or(NewgitError::NoRemote)?;
        validate_name(name)?;
        // Held across the spawn and the undo that finishes it: nothing else
        // gets to see the instance half-built.
        let _lock = self.lock_instance(&branch_slug(name), name)?;
        let remote = link.connect(Acting::Login)?;
        let project = link.project.clone();
        let from_slug = branch_slug(from_instance);
        let found = remote.checkpoint(&project, &from_slug, checkpoint_id)?;
        let mut record: CheckpointRecord =
            serde_json::from_value(found.record.clone()).map_err(|error| NewgitError::Remote {
                status: 0,
                message: format!("checkpoint {from_slug}/{checkpoint_id} is not one this newgit can read: {error}"),
            })?;
        let mut warnings = Vec::new();

        // Source: the checkpoint's list, composed here from the code trackers
        // this person may read — the author's exact commits when they read
        // everything the author did.
        let imported = self.import_code(
            remote.as_ref(),
            &project,
            &from_slug,
            checkpoint_id,
            &record.code,
            &mut warnings,
        )?;
        record.source.head_rev = imported.head.clone();
        record.source.dirty_rev = (imported.tip != imported.head).then(|| imported.tip.clone());
        let imported_ref = format!("refs/newgit/imported/{from_slug}/{checkpoint_id}");
        self.source.update_ref(&imported_ref, &imported.tip)?;

        // Trackers, resolved for whoever is spawning.
        let me = remote.me()?.login;
        for state in &mut record.tracker_states {
            let Some(definition) = self
                .trackers
                .iter()
                .find(|definition| definition.name == state.name)
            else {
                continue;
            };
            let resolved = self.resolve_imported_tracker(
                remote.as_ref(),
                &project,
                definition,
                &me,
                state.content_rev.as_deref(),
            );
            match resolved {
                Ok(rev) => state.content_rev = rev,
                Err(reason) => {
                    warnings.push(format!("tracker `{}`: {reason}", state.name));
                    state.content_rev = None;
                }
            }
        }

        let spawned = self.spawn_inner(
            name,
            Some(&record.source.head_rev),
            &BindingChoice::default(),
            false,
        )?;
        let mut branch = spawned.branch.clone();
        // Its tracker branches start where the checkpoint's committed work is.
        for (tracker, commit) in &imported.trackers {
            let instance_branch = format!("instances/{}", branch.slug);
            self.source
                .update_ref(&local_tracker_ref(tracker, &instance_branch), commit)?;
            self.publish_code_refs(tracker, &[&instance_branch])?;
        }

        // The record joins the new instance's own history, rewritten to
        // point at what is on this machine.
        let log = self.checkpoint_log(&branch);
        record.id = log.next_id()?;
        record.branch = branch.name.clone();
        record.message = Some(format!(
            "spawned from {from_slug}/{checkpoint_id} (by {}{})",
            found.created_by,
            found
                .agent
                .as_deref()
                .map(|agent| format!(", agent {agent}"))
                .unwrap_or_default()
        ));
        let store_ref = format!("refs/newgit/checkpoints/{}/{}", branch.slug, record.id);
        self.source
            .update_ref(&store_ref, &self.source.rev_parse(&imported_ref)?)?;
        self.source.delete_ref(&imported_ref)?;
        record.source.store_ref = store_ref;
        for state in &mut record.resource_states {
            // A deposit's recorded path was on the author's machine; it is the
            // same rev's file here, in this machine's lane checkout cache.
            if let (Some(state_ref), Some(path)) =
                (state.state_ref.as_deref(), state.state_path.as_ref())
                && let Some((tracker, rev)) = parse_tracker_state_ref(state_ref)
            {
                let marker = format!("/{tracker}/{rev}");
                let relative = path
                    .as_str()
                    .split_once(&marker)
                    .map(|(_, rest)| rest.trim_start_matches('/').to_owned())
                    .unwrap_or_default();
                let local = self.store.paths().tracker_checkouts.join(tracker).join(rev);
                state.state_path = Some(if relative.is_empty() {
                    local
                } else {
                    local.join(relative)
                });
            }
        }
        log.save(&record)?;

        let undo = self.undo(
            &branch.name,
            &UndoOptions {
                to: Some(record.id.clone()),
                ..UndoOptions::default()
            },
        )?;
        warnings.extend(undo.warnings.iter().cloned());
        branch = self.store.find_branch(&branch.name)?;
        // After the restore, which leaves running what the checkpoint had
        // running; anything already up is skipped.
        let started = self.autostart(&branch, None);

        Ok(SpawnFromCheckpointOutcome {
            branch,
            spawned,
            undo,
            started,
            author: found.created_by,
            agent: found.agent,
            warnings,
        })
    }

    /// Which rev of `definition` someone importing a checkpoint gets, after
    /// fetching it into the local lane; `Err` says why they get none.
    fn resolve_imported_tracker(
        &self,
        remote: &dyn Remote,
        project: &str,
        definition: &TrackerDefinition,
        me: &str,
        checkpoint_rev: Option<&str>,
    ) -> std::result::Result<Option<String>, String> {
        let lane = self.lane(&definition.name);
        if definition.storage != Storage::Remote {
            // Never synced: usable only if this machine already has it.
            return match checkpoint_rev {
                Some(rev) if lane.has_rev(rev) => Ok(Some(rev.to_owned())),
                Some(_) => Err(
                    "kept on the author's machine (`storage = \"local\"`), so not here".to_owned(),
                ),
                None => Ok(None),
            };
        }
        let lanes = format!("--git-dir={}", self.store.paths().tracker_store);
        crate::tracker_repo::ensure_store(&self.store.paths().tracker_store)
            .map_err(|error| error.to_string())?;
        if definition.audience == "user" {
            // Your own content, never the author's.
            let repo = tracker_repo(project, definition, me);
            let credential = match remote.credential(project, &repo, Scope::Read) {
                Ok(credential) => credential,
                Err(NewgitError::Remote { status: 404, .. }) => {
                    return match lane.main() {
                        Some(rev) => Ok(Some(rev)),
                        None => Err(no_own_content(&definition.name)),
                    };
                }
                Err(error) => return Err(error.to_string()),
            };
            let pattern = format!("refs/trackers/{}/*", definition.name);
            GitSource::fetch_refs(
                &[&lanes],
                credential.remote.as_deref().unwrap_or_default(),
                &[format!("+{pattern}:{pattern}")],
                Some(&basic_auth_header(&credential)),
            )
            .map_err(|error| error.to_string())?;
            return match lane.main() {
                Some(rev) => Ok(Some(rev)),
                None => Err(no_own_content(&definition.name)),
            };
        }
        let Some(rev) = checkpoint_rev else {
            return Ok(None);
        };
        if lane.has_rev(rev) {
            return Ok(Some(rev.to_owned()));
        }
        let repo = tracker_repo(project, definition, me);
        let credential =
            remote
                .credential(project, &repo, Scope::Read)
                .map_err(|error| match error {
                    NewgitError::Remote { message, .. } => format!("refused: {message}"),
                    other => other.to_string(),
                })?;
        let rev_ref = format!("refs/trackers/{}/pins/{rev}", definition.name);
        GitSource::fetch_refs(
            &[&lanes],
            credential.remote.as_deref().unwrap_or_default(),
            &[format!("+{rev_ref}:{rev_ref}")],
            Some(&basic_auth_header(&credential)),
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(rev.to_owned()))
    }

    /// Allocate ports, render exports, and run `prepare` hooks in dependency
    /// order. Prepare failures are loud but leave the instance spawned —
    /// re-run with `newgit action <resource>.prepare`.
    fn bind_resources(&self, branch: &mut BranchInstance) -> Result<Vec<ResourceBindOutcome>> {
        let mut outcomes = Vec::new();

        for name in &self.graph.bind_order {
            // A resource a tracker brings binds only while that tracker does;
            // one bound already (a `tracker use` adding to a live instance)
            // is left as it is.
            if !self.resource_active(branch, name) || branch.resources.contains_key(name) {
                continue;
            }
            let definition = self.resource_definition(name)?;

            let resolved_ports = self.claim_ports(branch, definition)?;

            // An export may compose a dependency's export, the same way a
            // `[[render]]` already can. Bindings happen in dependency order,
            // so everything upstream is resolved by the time this runs —
            // without it a *file* could carry another resource's URL while
            // the resource itself could not publish one, which is the wrong
            // way round: `[exports]` is what produces those values.
            let bound = self.bound_exports(branch);

            // Everywhere else an unknown `{{...}}` renders verbatim, so the
            // mistake is visible to whoever typed it. An export is the
            // exception for the same reason `[cleanup]` and `[[render]]` are:
            // it is written into the binding record once and handed to every
            // later action and `newgit run` as an environment variable, so
            // the mistake surfaces in a different process, hours later, as a
            // malformed URL. The unresolved value is dropped rather than
            // stored — an absent variable is a failure something downstream
            // can detect; `http://127.0.0.1:{{ports.db.api}}` is not.
            let (resolved_exports, unresolved) =
                self.resolve_exports(branch, definition, &resolved_ports, &bound);

            // Nothing this resource exports is stored when any of it fails.
            // A binding that publishes half an environment is the case the
            // refusal exists to prevent: `assemble_env` hands bindings to
            // `newgit run` and to every dependent's actions without asking
            // what status they hold, so a surviving `BASE_URL` beside a
            // dropped `HEALTH_URL` is exactly the malformed-environment
            // failure, one variable further down.
            let export_error = (!unresolved.is_empty()).then(|| {
                NewgitError::ExportUnresolved {
                    resource: definition.name.clone(),
                    exports: unresolved
                        .iter()
                        .map(|(export, placeholder)| format!("`{export}` ({placeholder})"))
                        .collect::<Vec<_>>()
                        .join(", "),
                }
                .to_string()
            });
            let resolved_exports = match export_error {
                Some(_) => BTreeMap::new(),
                None => resolved_exports,
            };

            branch.resources.insert(
                definition.name.clone(),
                ResourceBinding {
                    definition_rev: definition.definition_rev.clone(),
                    resolved_ports: resolved_ports.clone(),
                    resolved_exports,
                    rendered: Vec::new(),
                    status: ResourceStatus::Pending,
                    restore_proven: false,
                },
            );

            // Render before prepare, with ports allocated and every
            // dependency's exports already bound: the file a tool reads its
            // port from has to be right before the tool is started.
            let (rendered, render_error) = if export_error.is_some() {
                (Vec::new(), None)
            } else {
                match self.render_resource(branch, definition) {
                    Ok(rendered) => (rendered, None),
                    Err(error) => (Vec::new(), Some(error.to_string())),
                }
            };

            let mut blocked_by = self.blocked_dependencies(branch, definition);
            if render_error.is_some() || export_error.is_some() {
                if let Some(binding) = branch.resources.get_mut(&definition.name) {
                    binding.status = ResourceStatus::Failed;
                }
                outcomes.push(ResourceBindOutcome {
                    name: definition.name.clone(),
                    ports: resolved_ports,
                    status: ResourceStatus::Failed,
                    prepare: None,
                    blocked_by: std::mem::take(&mut blocked_by),
                    prepare_withheld: false,
                    captured: Vec::new(),
                    missing_captures: Vec::new(),
                    rendered: Vec::new(),
                    render_error,
                    export_error,
                    install: None,
                });
                continue;
            }

            // Save the binding as it stands, exports and rendered files
            // included and still `Pending`, before anything long runs. The
            // claim `claim_ports` saved holds this resource's ports and
            // nothing else, and there is none for a resource without ports,
            // so a spawn that died in `prepare` would leave a record whose
            // `newgit action <resource>.prepare` runs without the exports and
            // marks it ready, or one without the resource at all, which
            // `status` calls ready.
            self.store.save_branch_record(branch)?;

            // Prepare runs with the bindings made so far, so dependents see
            // their dependencies' exports. Failed dependencies block
            // dependents; the instance still spawns so logs can be inspected.
            //
            // A blocker only ever withholds a *command that would otherwise
            // run*. A resource with no runnable `prepare` has nothing to
            // withhold — there was never going to be a command here, blocked
            // dependency or not — so it is `Ready` as soon as it is bound,
            // exactly as it would be if nothing upstream had failed. Skipping
            // straight to that determination (instead of parking such a
            // resource at `Pending` because *something* in its graph is
            // unready) is what keeps `admin`/`mobile`-style dependents from
            // getting stuck the moment their one blocker clears: nothing
            // ever reruns to move them off `Pending`, but nothing needs to.
            //
            // That `Ready` is the record's own fact about this resource, not
            // the last word on it: `blocked_by` is derived from the graph at
            // read time by every surface that reports on the resource, so
            // `status`, `action`, `spawn` and `spawn --json` all call it
            // blocked while a dependency is down (#209).
            let runnable_prepare = definition.runnable_prepare();
            let mut captured_names = Vec::new();
            let mut missing_captures = Vec::new();
            let mut install = None;

            // The install store stands in front of the producing action, and
            // only here. `newgit spawn` asks for an instance, so how its tree
            // comes to exist is newgit's business; `newgit action deps.prepare`
            // asks for a *command*, and quietly not running it would be a lie
            // about what just happened.
            //
            // Computed once and reused by the publish below, because deriving
            // it runs the resource's `key_command`.
            let install_key = match runnable_prepare {
                Some(_) if blocked_by.is_empty() => match self.install_key(branch, definition) {
                    Some(Ok(key)) => Some(key),
                    Some(Err(report)) => {
                        install = Some(report);
                        None
                    }
                    None => None,
                },
                _ => None,
            };
            let filled = match &install_key {
                Some(key) => {
                    install = self.fill_from_install_store(branch, definition, key);
                    matches!(install, Some(InstallReport::Filled { .. }))
                }
                None => false,
            };
            if filled && let Some(binding) = branch.resources.get_mut(&definition.name) {
                binding.status = ResourceStatus::Ready;
            }

            let (status, prepare) = match runnable_prepare {
                _ if filled => (ResourceStatus::Ready, None),
                None => {
                    if let Some(binding) = branch.resources.get_mut(&definition.name) {
                        binding.status = ResourceStatus::Ready;
                    }
                    (ResourceStatus::Ready, None)
                }
                Some(_) if !blocked_by.is_empty() => {
                    // Left at the `Pending` it was bound with: nothing was
                    // withheld from this resource specifically, its
                    // dependency just is not ready yet. Whether that is
                    // still true is a question for read time
                    // (`blocked_dependencies`), not a fact to freeze into
                    // the record now.
                    (ResourceStatus::Pending, None)
                }
                Some(action) => {
                    let log = self
                        .store
                        .action_log_path(&branch.slug, &format!("{}.prepare", definition.name));
                    let (code, captured) =
                        self.run_one_shot(branch, definition, action, "prepare", &log, true)?;
                    captured_names = captured.found.keys().cloned().collect();
                    missing_captures = Self::missing_capture_warnings(
                        &definition.name,
                        "prepare",
                        &captured,
                        &log,
                    );
                    Self::apply_captures(branch, &definition.name, captured.found);
                    let status = if code == 0 {
                        ResourceStatus::Ready
                    } else {
                        ResourceStatus::Failed
                    };
                    if let Some(binding) = branch.resources.get_mut(&definition.name) {
                        binding.status = status;
                    }
                    if code == 0
                        && let Some(key) = &install_key
                    {
                        install = self.admit_to_install_store(branch, definition, key);
                    }
                    (status, Some((code == 0, log)))
                }
            };

            outcomes.push(ResourceBindOutcome {
                name: definition.name.clone(),
                ports: resolved_ports,
                status,
                prepare,
                prepare_withheld: runnable_prepare.is_some() && !blocked_by.is_empty(),
                blocked_by,
                captured: captured_names,
                missing_captures,
                rendered,
                render_error: None,
                export_error: None,
                install,
            });
        }
        branch.updated_at = Utc::now();
        Ok(outcomes)
    }

    /// Allocate `definition`'s ports for `branch`, and write them into its
    /// binding record before anyone else can allocate.
    ///
    /// The records are the only claim on a port ([`ports::used_ports`]), and
    /// `prepare` runs long before the caller saves the finished binding. Two
    /// spawns overlapping there, the ordinary case with one per agent, would
    /// each read records the other had not written yet, and the bind probe
    /// finds nothing listening on a port whose service is not up yet, so
    /// both would be handed it. So the claim is written here, before
    /// `prepare`, and reading the records, allocating and writing it are one
    /// step under the store's port lock ([`MetadataStore::lock_ports`]). The
    /// lock is let go before `prepare` runs: a spawn waits while another
    /// chooses its ports, never for its install.
    ///
    /// The record is saved as the instance stands, plus this resource bound
    /// `Pending` on the ports just chosen. The caller's own save replaces it
    /// with the finished binding.
    fn claim_ports(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<BTreeMap<String, u16>> {
        if definition.ports.is_empty() {
            return Ok(BTreeMap::new());
        }
        let _lock = self.store.lock_ports()?;
        let mut used = ports::used_ports(&self.store.load_branches()?);
        let mut resolved_ports = BTreeMap::new();
        for (port_name, request) in &definition.ports {
            resolved_ports.insert(
                port_name.clone(),
                ports::allocate(request.start, &mut used)?,
            );
        }
        let mut claim = branch.clone();
        claim.resources.insert(
            definition.name.clone(),
            ResourceBinding {
                definition_rev: definition.definition_rev.clone(),
                resolved_ports: resolved_ports.clone(),
                resolved_exports: BTreeMap::new(),
                rendered: Vec::new(),
                status: ResourceStatus::Pending,
                restore_proven: false,
            },
        );
        claim.updated_at = Utc::now();
        self.store.save_branch_record(&claim)?;
        Ok(resolved_ports)
    }

    /// Write this workspace's local-ignore rules: tracker-owned paths and
    /// the trees resources declare they produce. Idempotent, so it doubles as
    /// the re-sync for a workspace that predates a definition change.
    fn ensure_workspace_excludes(&self, branch: &BranchInstance) -> Result<()> {
        let owned: Vec<Utf8PathBuf> = self
            .trackers
            .iter()
            .flat_map(|definition| definition.paths.iter().cloned())
            .collect();
        exclude_tracker_paths(&branch.workspace_path, &owned)?;

        let produced: Vec<Utf8PathBuf> = self
            .resources
            .iter()
            .flat_map(|definition| definition.identity_produces().iter().cloned())
            .collect();
        exclude_produced_paths(&branch.workspace_path, &produced)
    }

    /// How workspaces are materialized here. On macOS they are cloned from
    /// a template checkout beside them (see
    /// [`GitSource::clone_via_template`]); elsewhere there is no
    /// whole-tree clone to make that faster than `git clone`.
    fn materializer(&self) -> RealDirMaterializer {
        let workspace_root = self.config.workspace_root(&self.store.paths().project_root);
        RealDirMaterializer {
            source_template: cfg!(target_os = "macos")
                .then(|| workspace_root.join(SOURCE_TEMPLATE_DIR)),
        }
    }

    /// Make this workspace's `origin` behave like the project's real remote.
    ///
    /// A workspace is cloned from the store, so its `origin` starts as a
    /// local path: `gh` cannot map it to a repository, `git pull` reads the
    /// store rather than the remote, and a push lands silently in the store.
    /// Instead `origin` fetches from the store's own `origin` directly, and
    /// pushes to newgit's outbox (see [`Self::serve_push`]), whose hook
    /// checkpoints and forwards (see [`Self::receive_push`]). To the agent,
    /// and to `gh`, it is an ordinary clone of the real repository.
    ///
    /// The hook is selected per push with `-c core.hooksPath`, so a global
    /// `core.hooksPath` (husky, lefthook) cannot switch it off. Idempotent,
    /// like the excludes, so it doubles as the re-sync — and follows the
    /// store if its `origin` changes.
    fn ensure_workspace_push(&self, branch: &BranchInstance) -> Result<()> {
        let outbox = self.outbox();
        self.source.ensure_outbox(&outbox)?;
        let hook = outbox.join("hooks/pre-receive");
        let script = pre_receive_script(&self.store.paths().project_root, &newgit_program());
        if std::fs::read_to_string(&hook).ok().as_deref() != Some(script.as_str()) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&hook, &script).map_err(|error| NewgitError::io(&hook, error))?;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| NewgitError::io(&hook, error))?;
        }

        // Git runs this with the outbox's path appended.
        let receive_pack = format!(
            "{} push-receive-pack --store {} -- {}",
            sh_quote(&newgit_program()),
            sh_quote(self.store.paths().project_root.as_str()),
            sh_quote(&branch.name),
        );
        // On a remote, `origin` is the project's Git endpoint; otherwise the
        // store's own origin.
        let fetch_url = match &branch.remote {
            Some(remote) => Some(remote.endpoint.clone()),
            None => self.source.remote_url(PUBLISH_REMOTE, false)?,
        };
        GitSource::workspace_route_origin(
            &branch.workspace_path,
            fetch_url.as_deref(),
            &outbox,
            &receive_pack,
        )?;
        if branch.remote.is_some() {
            self.route_export_remotes(branch, &outbox)?;
        }
        Ok(())
    }

    /// On a newgit remote, `origin` is the project's Git endpoint, which
    /// `gh` cannot map to a GitHub repository. So each `branches = true`
    /// target is a remote in the workspace too, named after it, and the
    /// first is the one `gh` resolves (`gh-resolved = base`): `gh pr create`
    /// opens a pull request there, against the branch newgit exported.
    ///
    /// Pushing to it directly is refused, naming `origin`: a direct push
    /// would carry trackers the target withholds, and skip the checkpoint.
    fn route_export_remotes(&self, branch: &BranchInstance, outbox: &Utf8Path) -> Result<()> {
        let workspace = branch.workspace_path.as_str();
        let config = |key: &str, value: &str| {
            crate::source::run_git_env(&["-C", workspace, "config", key, value], &[]).map(|_| ())
        };
        let targets = self.export_targets()?;
        let mut resolved = false;
        for target in targets.iter().filter(|target| target.branches) {
            let remote = &target.name;
            config(&format!("remote.{remote}.url"), &target.push_url())?;
            config(
                &format!("remote.{remote}.fetch"),
                &format!("+refs/heads/*:refs/remotes/{remote}/*"),
            )?;
            config(&format!("remote.{remote}.pushurl"), outbox.as_str())?;
            config(
                &format!("remote.{remote}.receivepack"),
                &format!(
                    "{} push-receive-pack --store {} --export-target {} -- {}",
                    sh_quote(&newgit_program()),
                    sh_quote(self.store.paths().project_root.as_str()),
                    sh_quote(remote),
                    sh_quote(&branch.name),
                ),
            )?;
            if !resolved {
                config(&format!("remote.{remote}.gh-resolved"), "base")?;
                resolved = true;
            }
            // What the target's own branch holds, as of the last export from
            // here, so `gh pr create --fill` can list the PR's commits.
            let main = export_ref(&target.name, PROJECT_ROOT_INSTANCE);
            if self.source.rev_parse(&main).is_ok() {
                crate::source::run_git_env(
                    &[
                        "-C",
                        workspace,
                        "fetch",
                        "--quiet",
                        "--no-tags",
                        self.store.paths().project_root.as_str(),
                        &format!("+{main}:refs/remotes/{remote}/{}", target.branch),
                    ],
                    &[],
                )?;
            }
        }
        Ok(())
    }

    /// Where workspace pushes land: a bare repository in newgit's local
    /// state, never the store itself. The store's branch refs belong to
    /// checkpoint and undo; the outbox's belong to the real remote.
    fn outbox(&self) -> Utf8PathBuf {
        self.store.paths().local.join("publish/outbox.git")
    }

    /// The key this instance's tree would be stored under, and the paths that
    /// tree covers. `None` means the resource declares no `[identity]
    /// produces` and is simply not a store participant.
    ///
    /// Every failure below returns [`InstallReport::Unavailable`] rather than
    /// an error, and that is deliberate: the store sits in front of a command
    /// that still works. A broken `key_command` or an unreadable lockfile
    /// must cost an install, never a spawn.
    fn install_key(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Option<std::result::Result<String, InstallReport>> {
        if definition.identity_produces().is_empty() {
            return None;
        }
        let inputs = match collect_files(&branch.workspace_path, definition.identity_paths()) {
            Ok(inputs) => inputs,
            Err(error) => {
                return Some(Err(InstallReport::Unavailable {
                    reason: format!("identity paths could not be read: {error}"),
                }));
            }
        };
        // No inputs on disk is not an empty key, it is no key: the lockfile
        // has not been written yet, and every instance in that state would
        // otherwise hash the same and share a tree built from nothing.
        if inputs.is_empty() {
            return Some(Err(InstallReport::Unavailable {
                reason: "no `[identity] paths` are present in the workspace yet".to_owned(),
            }));
        }
        let rev = match content_rev(&inputs) {
            Ok(rev) => rev,
            Err(error) => {
                return Some(Err(InstallReport::Unavailable {
                    reason: format!("identity paths could not be hashed: {error}"),
                }));
            }
        };

        let material = match definition.identity_key_command() {
            None => None,
            Some(command) => {
                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.key", definition.name));
                let env = match self.assemble_env(branch) {
                    Ok(env) => env,
                    Err(error) => {
                        return Some(Err(InstallReport::Unavailable {
                            reason: format!("environment could not be assembled: {error}"),
                        }));
                    }
                };
                let command = self.render_as_action_command(branch, definition, command);
                match run_captured(&command, &branch.workspace_path, &env, &log) {
                    Ok((0, stdout)) => Some(stdout),
                    Ok((code, _)) => {
                        return Some(Err(InstallReport::Unavailable {
                            reason: format!("`key_command` exited with {code} (see {log})"),
                        }));
                    }
                    Err(error) => {
                        return Some(Err(InstallReport::Unavailable {
                            reason: format!("`key_command` could not be run: {error}"),
                        }));
                    }
                }
            }
        };
        Some(Ok(crate::installs::identity_key(
            &rev,
            material.as_deref(),
            &self.bound_vars(branch),
            &definition.definition_rev,
        )))
    }

    /// Clone a stored tree in instead of building one, when the key hits.
    ///
    /// `key` is passed in rather than recomputed: the caller needs it again
    /// to publish on a miss, and deriving it runs the resource's
    /// `key_command`. Once per operation, not once per lookup.
    fn fill_from_install_store(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        key: &str,
    ) -> Option<InstallReport> {
        let key = key.to_owned();
        let store = self.installs();
        let entry = store.lookup(&definition.name, &key)?;
        let renders = match self.tree_renders(branch, definition) {
            Ok(renders) => renders,
            Err(error) => {
                return Some(InstallReport::Unavailable {
                    reason: format!("entry {key} could not be rendered for this instance: {error}"),
                });
            }
        };
        match store.fill(
            &entry,
            &branch.workspace_path,
            definition.identity_produces(),
            &renders,
        ) {
            Ok(Some(method)) => Some(InstallReport::Filled { key, method }),
            // The workspace already has the tree, so there is nothing to
            // fill and the real install is the right answer.
            Ok(None) => None,
            Err(error) => Some(InstallReport::Unavailable {
                reason: format!("entry {key} could not be cloned in: {error}"),
            }),
        }
    }

    /// Clone a stored tree over the one an undo is rewinding away from.
    ///
    /// Distinct from [`Self::fill_from_install_store`] only in that the
    /// workspace already has a tree: at spawn there is nothing to replace,
    /// and here there always is. It is the same guarantee either way — the
    /// entry was built from inputs that hash to what the workspace now
    /// holds, which after a source restore is the checkpoint's lockfile.
    fn refill_from_install_store(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        key: &str,
    ) -> Option<InstallReport> {
        let key = key.to_owned();
        let store = self.installs();
        let entry = store.lookup(&definition.name, &key)?;
        let renders = match self.tree_renders(branch, definition) {
            Ok(renders) => renders,
            Err(error) => {
                return Some(InstallReport::Unavailable {
                    reason: format!("entry {key} could not be rendered for this instance: {error}"),
                });
            }
        };
        match store.replace(
            &entry,
            &branch.workspace_path,
            definition.identity_produces(),
            &renders,
        ) {
            Ok(method) => Some(InstallReport::Filled { key, method }),
            Err(error) => Some(InstallReport::Unavailable {
                reason: format!("entry {key} could not be cloned in: {error}"),
            }),
        }
    }

    /// Drop this resource's entry for the identity the workspace now holds.
    ///
    /// `--force-recompute` means "do not trust that identity describes the
    /// tree" — usually because the tree is damaged in a way the lockfile
    /// cannot show. A cache keyed on that identity is under exactly the same
    /// suspicion, so the flag drops the entry too and the rebuild republishes
    /// it. Without this, the one failure the store can have — a bad entry —
    /// would have no way out but deleting the directory by hand.
    fn forget_install_entry(&self, definition: &ResourceDefinition, key: &str) {
        let _ = self.installs().remove(&definition.name, key);
    }

    /// Publish what the producing action just built, for the next instance.
    fn admit_to_install_store(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        key: &str,
    ) -> Option<InstallReport> {
        let key = key.to_owned();
        let store = self.installs();
        let renders = match self.tree_renders(branch, definition) {
            Ok(renders) => renders,
            Err(error) => {
                return Some(InstallReport::Unavailable {
                    reason: format!("the built tree could not be reversed for storing: {error}"),
                });
            }
        };
        match store.admit(
            &definition.name,
            &key,
            &branch.workspace_path,
            definition.identity_produces(),
            &renders,
        ) {
            Ok(Admission::Stored { method, unrendered }) => Some(InstallReport::Stored {
                key,
                method,
                unrendered,
            }),
            Ok(Admission::AlreadyStored) => Some(InstallReport::AlreadyStored { key }),
            Ok(Admission::Declined { path }) => Some(InstallReport::Declined { key, path }),
            // The action reported success without building what it declared.
            // Worth saying, because the declaration and the command disagree.
            Ok(Admission::Incomplete { path }) => Some(InstallReport::Unavailable {
                reason: format!("`{path}` was not built, so there was nothing to store"),
            }),
            Err(error) => Some(InstallReport::Unavailable {
                reason: format!("entry {key} could not be stored: {error}"),
            }),
        }
    }

    /// The `[[render]]`s a resource declares into its own built tree,
    /// resolved for this instance. Same values a committed-file render sees,
    /// so `{{workspace}}` means the same thing in both.
    fn tree_renders(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<Vec<TreeRender>> {
        let specs: Vec<&render::RenderSpec> = definition
            .render
            .iter()
            .filter(|spec| definition.renders_into_produced(spec))
            .collect();
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let ports = branch
            .resources
            .get(&definition.name)
            .map(|binding| binding.resolved_ports.clone())
            .unwrap_or_default();
        let exports = self.bound_exports(branch);
        let bound = self.bound_vars(branch);
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: Some(&ports),
            exports: Some(&exports),
            bound: Some(&bound),
            ..RenderContext::default()
        };
        specs
            .into_iter()
            .map(|spec| {
                Ok(TreeRender {
                    path: spec.path.clone(),
                    applied: render::resolve(&definition.name, spec, &context)?,
                })
            })
            .collect()
    }

    /// Substitute this instance's values into the files a resource declares,
    /// and record what was done on the binding.
    ///
    /// Committed content is the input, never the working file — so this is
    /// idempotent and safe to re-run after an undo or a tracker pull.
    fn render_resource(
        &self,
        branch: &mut BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<Vec<RenderOutcome>> {
        if definition.render.is_empty() {
            return Ok(Vec::new());
        }

        let context_ports = branch
            .resources
            .get(&definition.name)
            .map(|binding| binding.resolved_ports.clone())
            .unwrap_or_default();
        // A template sees what a command in this instance would see: its own
        // ports plus every export bound so far, in dependency order. Anything
        // narrower and an Expo `.env` needing the database's URL would want a
        // second mechanism.
        let context_exports = self.bound_exports(branch);

        let mut records = Vec::new();
        let mut outcomes = Vec::new();
        let mut source_owned = Vec::new();

        // A render into the resource's own built tree is the install
        // store's: it has no committed content, and it runs when the store
        // fills the tree (see `tree_renders`).
        for spec in definition
            .render
            .iter()
            .filter(|spec| !definition.renders_into_produced(spec))
        {
            let owner = self.tracker_owning(&spec.path);
            let committed = self.committed_content(branch, &spec.path, owner.as_deref())?;
            let Some(committed) = committed else {
                return Err(NewgitError::RenderPathNotCommitted {
                    resource: definition.name.clone(),
                    path: spec.path.clone(),
                });
            };

            let bound = self.bound_vars(branch);
            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: Some(&context_ports),
                exports: Some(&context_exports),
                bound: Some(&bound),
                ..RenderContext::default()
            };
            let (contents, applied) = render::apply(&definition.name, spec, &committed, &context)?;

            let target = branch.workspace_path.join(&spec.path);
            if let Some(parent) = target.parent() {
                crate::materializer::create_dir_all(parent)?;
            }
            std::fs::write(&target, contents)
                .map_err(|source| NewgitError::io(target.clone(), source))?;

            if owner.is_none() {
                source_owned.push(spec.path.clone());
            }
            outcomes.push(RenderOutcome {
                path: spec.path.clone(),
                replacements: applied.len(),
                tracker: owner.clone(),
            });
            records.push(RenderRecord {
                path: spec.path.clone(),
                tracker: owner,
                applied,
            });
        }

        // Source-owned targets only: tracker-owned paths are already in
        // `.git/info/exclude`, and marking an untracked path skip-worktree is
        // an error rather than a no-op.
        GitSource::workspace_skip_worktree(&branch.workspace_path, &source_owned)?;

        if let Some(binding) = branch.resources.get_mut(&definition.name) {
            binding.rendered = records;
        }
        Ok(outcomes)
    }

    /// Put a tracker's rendered files back to their committed values for the
    /// duration of a capture, so the lane records what every instance should
    /// see rather than what this one is running on.
    ///
    /// The workspace file is written in place and restored afterwards rather
    /// than captured from a copy, because capture walks the workspace: a
    /// second tree would be a second thing to keep honest.
    fn unrender_for_capture(
        &self,
        branch: &BranchInstance,
        tracker: &TrackerDefinition,
    ) -> Result<RenderedRestore> {
        let mut restore = RenderedRestore::default();
        for binding in branch.resources.values() {
            for record in &binding.rendered {
                if record.tracker.as_deref() != Some(tracker.name.as_str()) {
                    continue;
                }
                let path = branch.workspace_path.join(&record.path);
                let Ok(current) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let reversed = render::reverse(&current, &record.applied);
                if reversed == current {
                    continue;
                }
                std::fs::write(&path, &reversed)
                    .map_err(|source| NewgitError::io(path.clone(), source))?;
                restore.files.push((path, current));
            }
        }
        Ok(restore)
    }

    /// Render one resource's `[exports]` against its ports, its dependencies'
    /// exports, and each other. Returns what resolved, and every export that
    /// did not with the placeholder that stopped it.
    ///
    /// Composing a sibling is the obvious thing to write — `HEALTH_URL =
    /// "{{exports.BASE_URL}}/health"` two lines under `BASE_URL` — and
    /// refusing it while accepting the same line pointed at a *dependency*
    /// would be a rule nobody could guess. There is no declaration order to
    /// lean on, though: the table is a map, sorted by key, so `HEALTH_URL`
    /// renders before `BASE_URL` exists no matter how the file is written.
    /// So this resolves to a fixed point instead — each pass renders what it
    /// can, and a pass that resolves nothing new is the end. A definition's
    /// exports are a handful of short strings; the passes are not a cost.
    ///
    /// What survives that is unresolvable by construction: a typo, or a cycle
    /// (`A = "{{exports.B}}"`, `B = "{{exports.A}}"`), which stalls rather
    /// than looping forever and is reported like any other unresolved
    /// placeholder.
    fn resolve_exports(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        resolved_ports: &BTreeMap<String, u16>,
        bound: &BTreeMap<String, String>,
    ) -> (BTreeMap<String, String>, Vec<(String, String)>) {
        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        let mut pending: Vec<(&String, &String)> = definition.exports.iter().collect();

        loop {
            // This resource's own exports layer over its dependencies', the
            // same way a dependent's binding wins in `assemble_env`.
            let mut visible = bound.clone();
            visible.extend(
                resolved
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            let bound = self.bound_vars(branch);
            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: Some(resolved_ports),
                exports: Some(&visible),
                bound: Some(&bound),
                ..RenderContext::default()
            };

            let mut still_pending = Vec::new();
            let mut progressed = false;
            for (key, template) in pending {
                let rendered = render(template, &context);
                if unresolved_placeholder(&rendered).is_some() {
                    still_pending.push((key, template));
                } else {
                    resolved.insert(key.clone(), rendered);
                    progressed = true;
                }
            }
            pending = still_pending;
            if pending.is_empty() || !progressed {
                break;
            }
        }

        // Reported all at once, and against everything that did resolve, so
        // each one names what actually stopped it — a typo'd port, or the
        // sibling that stalled first. Reporting only the first would mean
        // fixing it, re-spawning, and meeting the next.
        let mut visible = bound.clone();
        visible.extend(
            resolved
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        let bound = self.bound_vars(branch);
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: Some(resolved_ports),
            exports: Some(&visible),
            bound: Some(&bound),
            ..RenderContext::default()
        };
        let unresolved = pending
            .into_iter()
            .map(|(key, template)| {
                let rendered = render(template, &context);
                let placeholder = unresolved_placeholder(&rendered)
                    .unwrap_or(template.as_str())
                    .to_owned();
                (key.clone(), placeholder)
            })
            .collect();
        (resolved, unresolved)
    }

    /// Every export bound so far, in dependency order — dependents win, the
    /// same layering [`Self::assemble_env`] applies.
    fn bound_exports(&self, branch: &BranchInstance) -> BTreeMap<String, String> {
        let mut exports = BTreeMap::new();
        for name in &self.graph.bind_order {
            if let Some(binding) = branch.resources.get(name) {
                for (key, value) in &binding.resolved_exports {
                    exports.insert(key.clone(), value.clone());
                }
            }
        }
        exports
    }

    /// The tracker owning a path, if any. Decides where committed content
    /// comes from and whether `capture` has to reverse the render.
    fn tracker_owning(&self, path: &Utf8Path) -> Option<String> {
        self.trackers
            .iter()
            .find(|definition| {
                definition
                    .paths
                    .iter()
                    .any(|owned| path == owned || path.starts_with(owned))
            })
            .map(|definition| definition.name.clone())
    }

    /// What a render substitutes into: the bound lane rev for a tracker-owned
    /// path, HEAD for a source-owned one. Never the working file.
    fn committed_content(
        &self,
        branch: &BranchInstance,
        path: &Utf8Path,
        tracker: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(tracker) = tracker else {
            return GitSource::workspace_show_head(&branch.workspace_path, path);
        };
        let Some(rev) = branch
            .trackers
            .get(tracker)
            .and_then(|binding| binding.content_rev.clone())
        else {
            return Ok(None);
        };
        let lane = self.lane(tracker);
        if !lane.has_rev(&rev) {
            return Ok(None);
        }
        lane.read_file(&rev, path)
    }

    /// Rendered paths across every resource — what export must take from HEAD
    /// and what a checkpoint's dirty commit must leave alone.
    fn rendered_source_paths(&self, branch: &BranchInstance) -> Vec<Utf8PathBuf> {
        branch
            .resources
            .values()
            .flat_map(|binding| binding.rendered.iter())
            .filter(|record| record.tracker.is_none())
            .map(|record| record.path.clone())
            .collect()
    }

    /// Re-render every resource's targets, in dependency order.
    ///
    /// Undo restores source and tracker content underneath the rendered
    /// files, so the values have to be put back. That costs nothing, because
    /// a render is a pure function of committed content and the binding
    /// record — both of which undo has just settled.
    /// Rendered files whose content on disk is not what this instance's
    /// render produces — that is, hand edits a re-render will discard.
    ///
    /// A render is a pure function of committed content and the binding
    /// record, so the expected bytes are recomputable at any time. Comparing
    /// against them turns the generic caveat "edits to a rendered file do not
    /// survive" into the specific one: *this file has changes, and this is
    /// the moment they are about to go.* It is silent when there is nothing
    /// to say, which a bind-time warning cannot be — at bind the edit does
    /// not exist yet.
    ///
    /// A recompute that fails (the committed content moved under the
    /// definition) is not drift and is not reported here; the re-render that
    /// follows reports it.
    ///
    /// Nor is a file holding exactly its committed content: a re-render
    /// loses none of it. That is what a tracker restore leaves — undo,
    /// `tracker checkout` and `tracker pull` write lane content, then
    /// re-render — so counting it would name an edit nobody made.
    fn render_drift(&self, branch: &BranchInstance) -> Vec<String> {
        let mut drifted = Vec::new();
        for (resource, binding) in &branch.resources {
            for record in &binding.rendered {
                let Ok(Some(committed)) =
                    self.committed_content(branch, &record.path, record.tracker.as_deref())
                else {
                    continue;
                };
                let Ok(expected) =
                    render::substitute(resource, &record.path, &committed, &record.applied)
                else {
                    continue;
                };
                let Ok(actual) = std::fs::read_to_string(branch.workspace_path.join(&record.path))
                else {
                    continue;
                };
                if actual != expected && actual != committed {
                    drifted.push(format!(
                        "`{}` has changes that render will discard: it is rendered by resource \
                         `{resource}`, so newgit rewrites it from committed content and this \
                         instance's values ({} line(s) differ). Move the edit into the \
                         committed file in the store repo to keep it.",
                        record.path,
                        differing_lines(&expected, &actual)
                    ));
                }
            }
        }
        drifted
    }

    fn rerender_all(&self, branch: &mut BranchInstance) -> Vec<String> {
        // Checked before the re-render, not after: afterwards the edit is
        // already gone and there is nothing left to name.
        let mut warnings = self.render_drift(branch);
        for name in self.graph.bind_order.clone() {
            let Ok(definition) = self.resource_definition(&name) else {
                continue;
            };
            if definition.render.is_empty() {
                continue;
            }
            let definition = definition.clone();
            if let Err(error) = self.render_resource(branch, &definition) {
                warnings.push(format!("re-render for resource `{name}` failed: {error}"));
            }
        }
        warnings
    }

    /// Run `<resource>.<action>` for an instance.
    pub fn run_action(&self, instance: &str, spec: &str) -> Result<ActionOutcome> {
        let (_lock, _) = self.locked_branch(instance)?;
        let (mut branch, definition, action, action_name) = self.resolve_action(instance, spec)?;
        let supervisor = self.supervisor(&branch);

        // Signal-only action (e.g. stop): signal the supervised process.
        if action.command.is_none() {
            let signal = action_signal(definition, action);
            return Ok(ActionOutcome::Stopped(
                supervisor.stop(&definition.name, &signal)?,
            ));
        }
        self.refuse_if_blocked(&branch, definition)?;

        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));

        if action.long_running {
            let (command, cwd) =
                self.action_invocation(&branch, definition, action, action_name)?;
            let env = self.assemble_env(&branch)?;
            let pid = supervisor.start(&definition.name, &command, &cwd, &env, &log)?;
            return Ok(ActionOutcome::Started { pid, log });
        }

        let (code, captured) =
            self.run_one_shot(&branch, definition, action, action_name, &log, false)?;
        let missing_captures =
            Self::missing_capture_warnings(&definition.name, action_name, &captured, &log);
        let mut dirty = Self::apply_captures(&mut branch, &definition.name, captured.found);
        if action_name == "prepare"
            && let Some(binding) = branch.resources.get_mut(&definition.name)
        {
            binding.status = if code == 0 {
                ResourceStatus::Ready
            } else {
                ResourceStatus::Failed
            };
            dirty = true;
        }
        if dirty {
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
        }
        // An explicit run of the producing action always runs — the store
        // never stands in for a command someone asked for by name — but what
        // it built is worth publishing for the next instance.
        let install = (code == 0 && action_name == definition.producing_action())
            .then(|| match self.install_key(&branch, definition) {
                Some(Ok(key)) => self.admit_to_install_store(&branch, definition, &key),
                Some(Err(report)) => Some(report),
                None => None,
            })
            .flatten();
        Ok(ActionOutcome::Ran {
            code,
            log,
            missing_captures,
            install,
        })
    }

    /// What [`Self::run_action`] would do, refusing for exactly the reasons
    /// it would refuse, and running nothing. The command is rendered by the
    /// same code that renders it for a real run; the environment it gets is
    /// [`Self::command_env`].
    pub fn plan_action(&self, instance: &str, spec: &str) -> Result<ActionPlan> {
        let (branch, definition, action, action_name) = self.resolve_action(instance, spec)?;
        if action.command.is_none() {
            return Ok(ActionPlan::Signal {
                signal: action_signal(definition, action),
            });
        }
        self.refuse_if_blocked(&branch, definition)?;
        let (command, cwd) = self.action_invocation(&branch, definition, action, action_name)?;
        Ok(ActionPlan::Command {
            command,
            cwd,
            long_running: action.long_running,
            captures: action.captures.clone(),
        })
    }

    /// Look up `<resource>.<action>` for an instance, with every refusal a
    /// run makes before it reaches the command.
    fn resolve_action<'s>(
        &self,
        instance: &str,
        spec: &'s str,
    ) -> Result<(
        BranchInstance,
        &ResourceDefinition,
        &crate::resource::ActionSpec,
        &'s str,
    )> {
        self.require_resolvable_graph()?;
        let (resource_name, action_name) = spec.split_once('.').ok_or_else(|| {
            NewgitError::Unsupported(format!("`{spec}` is not of the form <resource>.<action>"))
        })?;
        let branch = self.store.find_branch(instance)?;
        self.require_workspace(&branch)?;
        let definition = self.resource_definition(resource_name)?;
        let action =
            definition
                .actions
                .get(action_name)
                .ok_or_else(|| NewgitError::UnknownAction {
                    resource: resource_name.to_owned(),
                    action: action_name.to_owned(),
                })?;
        Ok((branch, definition, action, action_name))
    }

    /// Refusing to run is the right call, but it is not this resource's own
    /// outcome — it is derived from a dependency, and re-derivable the
    /// moment that dependency changes. Nothing is written to the record.
    fn refuse_if_blocked(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<()> {
        let blocked_by = self.blocked_dependencies(branch, definition);
        if blocked_by.is_empty() {
            return Ok(());
        }
        Err(NewgitError::Unsupported(format!(
            "resource `{}` is blocked by failed dependency/dependencies: {}",
            definition.name,
            blocked_by.join(", ")
        )))
    }

    /// The rendered command an action runs and the directory it runs in —
    /// shared by real runs and [`Self::plan_action`], so a dry run cannot
    /// describe a different command than the one that would run.
    fn action_invocation(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
        action_name: &str,
    ) -> Result<(String, Utf8PathBuf)> {
        let command = self.rendered_command(branch, definition, action)?;
        let cwd = resource_cwd(
            &branch.workspace_path,
            &definition.name,
            action_name,
            definition.workdir_for(Some(action)),
        )?;
        Ok((command, cwd))
    }

    /// Run an arbitrary command inside the instance with the full export
    /// environment loaded. Returns the exit code.
    pub fn run_command(
        &self,
        instance: &str,
        command_line: &[String],
    ) -> Result<(i32, Utf8PathBuf)> {
        let branch = self.store.find_branch(instance)?;
        self.require_workspace(&branch)?;
        let env = self.assemble_env(&branch)?;
        let log = self.store.action_log_path(&branch.slug, "run");
        let code = run_foreground(command_line, &branch.workspace_path, &env, &log)?;
        Ok((code, log))
    }

    /// Run a one-shot action, returning its exit code and whatever values it
    /// declared in `captures`. An action with captures runs captured (its
    /// output reaches the log but not the terminal), because newgit has to
    /// read stdout to find the handle the command just minted.
    fn run_one_shot(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
        action_name: &str,
        log: &Utf8Path,
        quiet: bool,
    ) -> Result<(i32, Captures)> {
        let (command, cwd) = self.action_invocation(branch, definition, action, action_name)?;
        let env = self.assemble_env(branch)?;

        if action.captures.is_empty() {
            let command_line = ["sh".to_owned(), "-c".to_owned(), command];
            let code = if quiet {
                crate::supervisor::run_quiet(&command_line, &cwd, &env, log)?
            } else {
                run_foreground(&command_line, &cwd, &env, log)?
            };
            return Ok((code, Captures::default()));
        }

        let (code, stdout) = run_captured(&command, &cwd, &env, log)?;
        Ok((code, parse_captures(&stdout, &action.captures)))
    }

    /// One warning per declared capture the command never emitted. The log
    /// path is included because the answer is nearly always in the command's
    /// own output — typically its stdout carrying something other than the
    /// captures.
    fn missing_capture_warnings(
        resource: &str,
        action: &str,
        captures: &Captures,
        log: &Utf8Path,
    ) -> Vec<String> {
        captures
            .missing
            .iter()
            .map(|name| {
                format!(
                    "resource `{resource}` declared capture `{name}` on `{action}`, not found \
                     in stdout (log: {log}); when `captures` is set, stdout belongs to newgit — \
                     send everything else to stderr"
                )
            })
            .collect()
    }

    /// Merge values an action captured into the resource's binding exports,
    /// so later commands, hooks, and `newgit run` all see the handle. The
    /// binding record is the single source of truth for a resource instance,
    /// including the parts another system named.
    fn apply_captures(
        branch: &mut BranchInstance,
        resource: &str,
        captured: BTreeMap<String, String>,
    ) -> bool {
        if captured.is_empty() {
            return false;
        }
        let Some(binding) = branch.resources.get_mut(resource) else {
            return false;
        };
        binding.resolved_exports.extend(captured);
        true
    }

    fn rendered_command(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
    ) -> Result<String> {
        let command = action.command.clone().ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "resource `{}` action has no command",
                definition.name
            ))
        })?;
        Ok(self.render_as_action_command(branch, definition, &command))
    }

    /// Render `template` in an action command's scope: branch, workspace,
    /// scripts, this resource's ports, and what the instance binds. No
    /// `{{exports.*}}` — a command reads those from its environment.
    ///
    /// Shared with `[identity] key_command`, which is a command like any
    /// other. The binding is already mixed into every key
    /// (`installs::identity_key`); `{{bound.paths}}` lets the command pass it
    /// to what it inspects. Its output should not carry per-instance values
    /// such as `{{workspace}}`, or no two instances share an entry.
    fn render_as_action_command(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        template: &str,
    ) -> String {
        let bound = self.bound_vars(branch);
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: branch
                .resources
                .get(&definition.name)
                .map(|binding| &binding.resolved_ports),
            bound: Some(&bound),
            ..RenderContext::default()
        };
        render(template, &context)
    }

    /// The layered environment `newgit run` and every hook see: actions,
    /// `key_command`, checkpoint, restore, and cleanup alike. Later layers
    /// win: resource exports in dependency order → port env vars → newgit
    /// context vars. Trackers own content; command environment wiring lives
    /// outside the tracker primitive. The variables are added on top of the
    /// environment of whoever ran newgit; nothing inherited is removed.
    pub fn assemble_env(&self, branch: &BranchInstance) -> Result<Vec<(String, String)>> {
        Ok(self
            .command_env(branch)?
            .into_iter()
            .map(|var| (var.name, var.value))
            .collect())
    }

    /// [`Self::assemble_env`] with each variable's origin attached — what
    /// `newgit env` prints. This *is* the assembly; `assemble_env` only drops
    /// the origins, so what is printed cannot drift from what a hook gets.
    pub fn command_env(&self, branch: &BranchInstance) -> Result<Vec<EnvVar>> {
        // Layering depends on dependency order, so the order has to be real.
        self.require_resolvable_graph()?;
        Ok(self.layered_env(branch).0)
    }

    /// [`Self::command_env`] without the gate: the same layers, in whatever
    /// order the graph has, and the names a layer gave a different value
    /// than one before it did. Only teardown may skip the gate — see
    /// [`Self::run_cleanup_hooks`].
    fn layered_env(&self, branch: &BranchInstance) -> (Vec<EnvVar>, BTreeSet<String>) {
        let mut env: BTreeMap<String, (String, EnvSource)> = BTreeMap::new();
        // Where one declaration overwrote another's different value, the
        // order chose what a command sees. newgit's own context vars are not
        // counted: they win by design, not by order.
        let mut clashing = BTreeSet::new();

        // Layer 1: resource exports, dependency order (dependents win).
        for name in &self.graph.bind_order {
            if let Some(binding) = branch.resources.get(name) {
                let definition = self.resource_definition(name).ok();
                for (key, value) in &binding.resolved_exports {
                    let source = export_source(name, definition, key);
                    if let Some((previous, _)) = env.insert(key.clone(), (value.clone(), source))
                        && previous != *value
                    {
                        clashing.insert(key.clone());
                    }
                }
            }
        }

        // Layer 2: port env vars.
        for name in &self.graph.bind_order {
            let Some(binding) = branch.resources.get(name) else {
                continue;
            };
            let Ok(definition) = self.resource_definition(name) else {
                continue;
            };
            for (port_name, request) in &definition.ports {
                if let (Some(env_name), Some(port)) =
                    (&request.env, binding.resolved_ports.get(port_name))
                {
                    let source = EnvSource::Port {
                        resource: name.clone(),
                        port: port_name.clone(),
                    };
                    let port = port.to_string();
                    if let Some((previous, _)) =
                        env.insert(env_name.clone(), (port.clone(), source))
                        && previous != port
                    {
                        clashing.insert(env_name.clone());
                    }
                }
            }
        }

        // Layer 3: context vars.
        env.insert(
            "NEWGIT_BRANCH".to_owned(),
            (branch.name.clone(), EnvSource::Newgit),
        );
        env.insert(
            "NEWGIT_WORKSPACE".to_owned(),
            (branch.workspace_path.to_string(), EnvSource::Newgit),
        );
        // A hook run under the instance's lock is told which locks it runs
        // under, so a `newgit` it starts that needs one of them can refuse
        // instead of waiting for its own ancestor (see `crate::lock`).
        if self.held_lock(&branch.slug).is_some() {
            let held = self.held_locks();
            env.insert(
                LOCKED_ENV.to_owned(),
                (
                    locked_env_value(held.iter().map(|lock| (lock.slug(), lock.token()))),
                    EnvSource::Newgit,
                ),
            );
        }

        let vars = env
            .into_iter()
            .map(|(name, (value, source))| EnvVar {
                name,
                value,
                source,
            })
            .collect();
        (vars, clashing)
    }

    pub fn add_resource(&self, name: &str, template_name: &str) -> Result<AddResourceOutcome> {
        validate_name(name)?;
        let template = resource_template(template_name)
            .ok_or_else(|| NewgitError::UnknownTemplate(template_name.to_owned()))?;
        let path = self
            .store
            .write_resource_file(name, &instantiate(template.contents, name))?;

        // Companions the template depends on, created only when absent so an
        // existing definition is never overwritten.
        let mut companions_created = Vec::new();
        for companion in template.companions {
            let companion_path = self
                .store
                .paths()
                .resources
                .join(format!("{}.toml", companion.name));
            if !companion_path.exists() {
                companions_created.push(self.store.write_resource_file(
                    companion.name,
                    &instantiate(companion.contents, companion.name),
                )?);
            }
        }

        // Lanes the template deposits into. A checkpoint whose `into_tracker`
        // names a tracker that does not exist fails at checkpoint time, so a
        // template that deposits has to bring its lane with it.
        let mut trackers_created = Vec::new();
        for companion in template.companion_trackers {
            let tracker_path = self
                .store
                .paths()
                .trackers
                .join(format!("{}.toml", companion.name));
            if !tracker_path.exists() {
                trackers_created.push(
                    self.create_tracker(
                        companion.name,
                        companion.audience,
                        Storage::Local,
                        companion.merge_with_source,
                    )?
                    .path,
                );
            }
        }

        Ok(AddResourceOutcome {
            path,
            companions_created,
            trackers_created,
        })
    }

    pub fn resource_definitions(&self) -> &[ResourceDefinition] {
        &self.resources
    }

    /// Resolve every `[[render]]` against the project's *working tree*
    /// instead of committed content — the dry run `render --check` is built
    /// for.
    ///
    /// Every other render reads `HEAD`, or a tracker's bound rev, on
    /// purpose: that is what makes `undo`, `tracker pull`, and re-renders
    /// idempotent, and it must not change. But it also means a `find` you
    /// just wrote is invisible to the tool that would validate it until you
    /// commit it — the adoption loop was edit, commit, spawn, read the
    /// failure, edit again. This is the one place that deliberately reads
    /// whatever is on disk, because it exists only to shorten that loop and
    /// never substitutes or writes anything.
    pub fn render_check(&self) -> Vec<RenderCheckTarget> {
        let root = &self.store().paths().project_root;
        let mut targets = Vec::new();
        for definition in &self.resources {
            for spec in &definition.render {
                let tracker = self.tracker_owning(&spec.path);
                let in_built_tree = definition.renders_into_produced(spec);
                let full = root.join(&spec.path);
                let checks = match std::fs::read_to_string(&full) {
                    Ok(content) if !in_built_tree => Some(render::check(spec, &content)),
                    _ => None,
                };
                targets.push(RenderCheckTarget {
                    resource: definition.name.clone(),
                    path: spec.path.clone(),
                    tracker,
                    checks,
                    in_built_tree,
                });
            }
        }
        targets
    }

    /// The inverse of `resource add`: delete a resource definition.
    ///
    /// Refuses rather than leaving a broken graph or an orphaned binding
    /// behind — `rm .newgit/resources/<name>.toml` already does the deletion;
    /// what it cannot do is notice a dependent or a live instance.
    pub fn remove_resource(&self, name: &str, force: bool) -> Result<RemoveResourceOutcome> {
        let definition = self
            .resources
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| {
                NewgitError::Unsupported(format!(
                    "no resource named `{name}`; defined resources: {}",
                    self.resource_names_label()
                ))
            })?;

        // Refused unconditionally: `--force` answers "what about a bound
        // instance", not "what about the rest of the graph". Removing a
        // dependency out from under a resource that still names it leaves
        // `depends_on` pointing at nothing, which is exactly the graph damage
        // this command exists to prevent.
        let dependents: Vec<&str> = self
            .resources
            .iter()
            .filter(|other| other.depends_on.iter().any(|dep| dep == name))
            .map(|other| other.name.as_str())
            .collect();
        if !dependents.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "resource `{name}` is still depended on by {}; remove {} first, or edit its \
                 `depends_on`",
                dependents.join(", "),
                if dependents.len() == 1 { "it" } else { "them" }
            )));
        }

        let branches = self.store.load_branches()?;
        let bound: Vec<BranchInstance> = branches
            .into_iter()
            .filter(|branch| branch.resources.contains_key(name))
            .collect();

        if !bound.is_empty() && !force {
            return Err(NewgitError::Unsupported(format!(
                "resource `{name}` is bound to {}; remove {} first with `newgit remove \
                 <instance>`, or pass --force to drop the binding and release its ports",
                bound
                    .iter()
                    .map(|branch| branch.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if bound.len() == 1 { "it" } else { "them" }
            )));
        }

        let mut unbound_instances = Vec::new();
        for branch in bound {
            let (_lock, mut branch) = self.locked_branch(&branch.slug)?;
            if !branch.resources.contains_key(name) {
                continue;
            }
            let supervisor = self.supervisor(&branch);
            if supervisor.running_pid(name).is_some() {
                // Best-effort: a `--force` remove should not leave a process
                // running that nothing can reach any more (its definition is
                // about to disappear too), but a failed stop must not block
                // the removal — the definition file going away is the part
                // the caller actually asked for.
                let _ = supervisor.stop(name, &definition.stop_signal());
            }
            branch.resources.remove(name);
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
            unbound_instances.push(branch.name);
        }

        let path = self.store.delete_resource_definition(name)?;
        Ok(RemoveResourceOutcome {
            path,
            unbound_instances,
        })
    }

    fn resource_names_label(&self) -> String {
        if self.resources.is_empty() {
            "none defined".to_owned()
        } else {
            self.resources
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    fn resource_definition(&self, name: &str) -> Result<&ResourceDefinition> {
        self.resources
            .iter()
            .find(|definition| definition.name == name)
            .ok_or_else(|| NewgitError::UnknownResource(name.to_owned()))
    }

    fn supervisor(&self, branch: &BranchInstance) -> Supervisor {
        Supervisor::new(self.store.instance_state_dir(&branch.slug))
    }

    /// The dependencies of `definition` that are not `Ready` in `branch`'s
    /// record as it stands. Worked out on every call rather than stored, so
    /// it cannot go stale (see `ResourceStatus::Pending`).
    pub fn blocked_dependencies(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Vec<String> {
        definition
            .depends_on
            .iter()
            .filter_map(|dependency| {
                branch.resources.get(dependency).and_then(|binding| {
                    (binding.status != ResourceStatus::Ready).then(|| dependency.clone())
                })
            })
            .collect()
    }

    /// Bind a tracker into an instance workspace. If the lane has captured
    /// content, project the lane head; otherwise the tracker starts empty.
    /// `refresh` allows rebinding an already-bound tracker.
    fn bind_tracker(
        &self,
        branch: &mut BranchInstance,
        definition: &TrackerDefinition,
        refresh: bool,
    ) -> Result<TrackerBindOutcome> {
        let lane = self.lane(&definition.name);
        let workspace = branch.workspace_path.clone();

        // Safety net when re-materializing over existing content — with
        // renders reversed, as in `checkout_tracker`, so the safety rev is a
        // lane rev like any other rather than one instance's ports.
        if refresh && !definition.paths.is_empty() {
            let existing = collect_owned_files(&workspace, definition)?;
            if !existing.is_empty() {
                let bound = branch
                    .trackers
                    .get(&definition.name)
                    .and_then(|binding| binding.content_rev.clone());
                let reversed = self.unrender_for_capture(branch, definition)?;
                let safety = lane.capture(
                    &workspace,
                    definition,
                    CaptureAt {
                        parent: bound.as_deref(),
                        branch: Branch::Instance(&branch.slug),
                        message: &format!(
                            "{} in `{}`, before re-binding",
                            definition.name, branch.name
                        ),
                    },
                );
                reversed.restore()?;
                safety?;
            }
        }

        let (origin, content_rev, files) = match lane.main() {
            Some(rev) => {
                let files = lane.restore(&workspace, definition, &rev)?;
                (BindOrigin::LaneHead, Some(rev), files)
            }
            None => (BindOrigin::Nothing, None, 0),
        };

        branch.trackers.insert(
            definition.name.clone(),
            TrackerBinding {
                definition_rev: definition.definition_rev.clone(),
                content_rev: content_rev.clone(),
            },
        );
        branch.updated_at = Utc::now();

        Ok(TrackerBindOutcome {
            warnings: Vec::new(),
            name: definition.name.clone(),
            content_rev,
            files,
            origin,
        })
    }

    /// `{{bound.trackers}}` and `{{bound.paths}}` for `branch`.
    fn bound_vars(&self, branch: &BranchInstance) -> crate::exports::Bound {
        let bound: Vec<&TrackerDefinition> = self
            .selectable()
            .into_iter()
            .filter(|definition| !branch.dropped.contains(&definition.name))
            .collect();
        crate::exports::Bound {
            trackers: bound
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            paths: bound
                .iter()
                .filter(|definition| !definition.secret)
                .flat_map(|definition| definition.paths.iter().map(|path| path.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// Trackers an instance may bind or drop: every code and secret tracker
    /// but `root`, `project`, and `access`, which are always bound.
    fn selectable(&self) -> Vec<&TrackerDefinition> {
        self.code
            .iter()
            .filter(|definition| {
                definition.name != crate::tracker::PROJECT_TRACKER
                    && definition.name != crate::tracker::ACCESS_TRACKER
            })
            .chain(self.trackers.iter())
            .collect()
    }

    /// What a new instance binds — returned as what it drops. One tracker per
    /// role: the one `choice` names, else this person's own default for the
    /// role (`~/.newgit/config.toml`), else the highest `priority` (negative
    /// means only when asked). A role-less tracker is bound unless its
    /// priority is negative or `choice` drops it. Then everything a bound
    /// tracker `requires` is bound too.
    pub fn choose_binding(&self, choice: &BindingChoice) -> Result<BTreeSet<String>> {
        let selectable = self.selectable();
        let known = |name: &String| selectable.iter().any(|definition| &definition.name == name);
        for name in choice.uses.iter().chain(&choice.drops) {
            if !known(name) {
                return Err(NewgitError::UnknownTracker(name.clone()));
            }
        }
        if let Some(both) = choice.uses.iter().find(|name| choice.drops.contains(name)) {
            return Err(NewgitError::Unsupported(format!(
                "`{both}` is both used and dropped"
            )));
        }
        let defaults = crate::config::personal_role_defaults(&self.config.project.name);
        let mut bound: BTreeSet<String> = BTreeSet::new();
        let mut roles: BTreeMap<&str, Vec<&TrackerDefinition>> = BTreeMap::new();
        for definition in &selectable {
            match &definition.role {
                Some(role) => roles.entry(role.as_str()).or_default().push(definition),
                None => {
                    let wanted = choice.uses.contains(&definition.name)
                        || (definition.priority >= 0 && !choice.drops.contains(&definition.name));
                    if wanted {
                        bound.insert(definition.name.clone());
                    }
                }
            }
        }
        for (role, members) in &roles {
            if let Some(chosen) = self.fill_role(role, members, choice, &defaults)? {
                bound.insert(chosen);
            }
        }
        // What they require, transitively.
        let mut queue: Vec<String> = bound.iter().cloned().collect();
        while let Some(name) = queue.pop() {
            let Some(definition) = selectable.iter().find(|definition| definition.name == name)
            else {
                continue;
            };
            for needed in &definition.requires {
                let target = if known(needed) {
                    if choice.drops.contains(needed) {
                        return Err(NewgitError::Unsupported(format!(
                            "`{name}` requires `{needed}`, so it cannot be dropped"
                        )));
                    }
                    Some(needed.clone())
                } else if let Some(members) = roles.get(needed.as_str()) {
                    if members.iter().any(|member| bound.contains(&member.name)) {
                        None
                    } else {
                        Some(self.fill_role(needed, members, choice, &defaults)?.ok_or_else(|| {
                            NewgitError::Unsupported(format!(
                                "`{name}` requires a `{needed}`, and none is chosen by default: \
                                 `--use` one of {}",
                                members
                                    .iter()
                                    .map(|member| format!("`{}`", member.name))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        })?)
                    }
                } else {
                    return Err(NewgitError::Unsupported(format!(
                        "`{name}` requires `{needed}`, which is neither a tracker nor a role"
                    )));
                };
                if let Some(target) = target
                    && bound.insert(target.clone())
                {
                    queue.push(target);
                }
            }
        }
        Ok(selectable
            .iter()
            .filter(|definition| !bound.contains(&definition.name))
            .map(|definition| definition.name.clone())
            .collect())
    }

    /// The tracker a role gets: the one asked for, else this person's
    /// default, else the highest priority that is not negative.
    fn fill_role(
        &self,
        role: &str,
        members: &[&TrackerDefinition],
        choice: &BindingChoice,
        defaults: &BTreeMap<String, String>,
    ) -> Result<Option<String>> {
        let asked: Vec<&&TrackerDefinition> = members
            .iter()
            .filter(|member| choice.uses.contains(&member.name))
            .collect();
        if asked.len() > 1 {
            return Err(NewgitError::Unsupported(format!(
                "{} all fill the role `{role}`; use one",
                asked
                    .iter()
                    .map(|member| format!("`{}`", member.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if let Some(asked) = asked.first() {
            return Ok(Some(asked.name.clone()));
        }
        if let Some(default) = defaults.get(role)
            && members.iter().any(|member| &member.name == default)
            && !choice.drops.contains(default)
        {
            return Ok(Some(default.clone()));
        }
        let mut candidates: Vec<&&TrackerDefinition> = members
            .iter()
            .filter(|member| member.priority >= 0 && !choice.drops.contains(&member.name))
            .collect();
        candidates.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(candidates.first().map(|member| member.name.clone()))
    }

    /// Whether `resource` binds in `branch`: not brought by any tracker, or
    /// brought by one the instance binds.
    fn resource_active(&self, branch: &BranchInstance, resource: &str) -> bool {
        let bringers: Vec<&TrackerDefinition> = self
            .selectable()
            .into_iter()
            .filter(|definition| definition.resources.iter().any(|name| name == resource))
            .collect();
        bringers.is_empty()
            || bringers
                .iter()
                .any(|definition| !branch.dropped.contains(&definition.name))
    }

    /// Stop binding `tracker` in `instance` (`newgit tracker drop`). A code
    /// tracker leaves the working tree (sparse checkout) but not the history;
    /// uncommitted work in it refuses the drop unless `uncommitted` says to
    /// stash it (`use` brings it back) or discard it (after a checkpoint, so
    /// it can be undone). A secret tracker's content is captured first, then
    /// its files removed; `use` projects that content back.
    pub fn drop_tracker(
        &self,
        instance: &str,
        tracker: &str,
        uncommitted: Uncommitted,
    ) -> Result<BindingChange> {
        // Held through the rebuild too, which saves the record it loads;
        // the inner change takes the same lock again.
        let (_lock, _) = self.locked_branch(instance)?;
        let before = self.binding_before(instance)?;
        let mut change = self.drop_tracker_inner(instance, tracker, uncommitted)?;
        change.rebuilt = self.rebuild_for_binding(instance, &before)?;
        Ok(change)
    }

    /// The drop itself, without the rebuild: `use` drops whatever filled the
    /// role it is taking, and rebuilds once for the binding it ends with.
    fn drop_tracker_inner(
        &self,
        instance: &str,
        tracker: &str,
        uncommitted: Uncommitted,
    ) -> Result<BindingChange> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        self.require_workspace(&branch)?;
        let mut change = BindingChange {
            tracker: tracker.to_owned(),
            ..BindingChange::default()
        };
        if [
            self.root.name.as_str(),
            crate::tracker::PROJECT_TRACKER,
            crate::tracker::ACCESS_TRACKER,
        ]
        .contains(&tracker)
        {
            return Err(NewgitError::Unsupported(format!(
                "`{tracker}` cannot be dropped: it holds {}",
                if tracker == self.root.name {
                    "everything no other tracker claims"
                } else {
                    "the project's definitions"
                }
            )));
        }
        if branch.dropped.contains(tracker) {
            change.already = true;
            return Ok(change);
        }
        // Nothing bound may be left without what it requires.
        let role = self
            .selectable()
            .into_iter()
            .find(|definition| definition.name == tracker)
            .and_then(|definition| definition.role.clone());
        let others_in_role = |role: &str| {
            self.selectable().into_iter().any(|definition| {
                definition.name != tracker
                    && definition.role.as_deref() == Some(role)
                    && !branch.dropped.contains(&definition.name)
            })
        };
        for definition in self.selectable() {
            if branch.dropped.contains(&definition.name) || definition.name == tracker {
                continue;
            }
            let needs = definition.requires.iter().any(|needed| {
                needed == tracker
                    || (role.as_deref() == Some(needed.as_str()) && !others_in_role(needed))
            });
            if needs {
                return Err(NewgitError::Unsupported(format!(
                    "`{}` requires `{tracker}`, so it cannot be dropped while `{}` is bound",
                    definition.name, definition.name
                )));
            }
        }
        let workspace = branch.workspace_path.clone();
        if let Some(definition) = self
            .code
            .iter()
            .find(|definition| definition.name == tracker)
        {
            let paths: Vec<&str> = definition.paths.iter().map(|path| path.as_str()).collect();
            let mut status_args = vec![
                "-C",
                workspace.as_str(),
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--",
            ];
            status_args.extend(&paths);
            let dirty = !crate::source::run_git_env(&status_args, &[])?
                .trim()
                .is_empty();
            if dirty {
                match uncommitted {
                    Uncommitted::Refuse => {
                        return Err(NewgitError::Unsupported(format!(
                            "`{tracker}` has uncommitted changes in this workspace: commit them, \
                             or drop it with `--stash` (kept for when you use it again) or \
                             `--discard` (after a checkpoint, so `newgit undo` gets them back)"
                        )));
                    }
                    Uncommitted::Stash => {
                        let message = stash_message(tracker);
                        let mut args = vec![
                            "-C",
                            workspace.as_str(),
                            "stash",
                            "push",
                            "--include-untracked",
                            "-m",
                            message.as_str(),
                            "--",
                        ];
                        args.extend(&paths);
                        crate::source::run_git_env(&args, &[])?;
                        change.stashed = true;
                    }
                    Uncommitted::Discard => {
                        let checkpoint = self.checkpoint_branch(
                            &mut branch,
                            Some(&format!("before dropping `{tracker}`")),
                            CheckpointReason::Explicit,
                        )?;
                        change.checkpoint = Some(checkpoint.record.id);
                        let mut restore = vec!["-C", workspace.as_str(), "checkout", "HEAD", "--"];
                        restore.extend(&paths);
                        let _ = crate::source::run_git_env(&restore, &[]);
                        let mut clean = vec!["-C", workspace.as_str(), "clean", "-fdq", "--"];
                        clean.extend(&paths);
                        crate::source::run_git_env(&clean, &[])?;
                    }
                }
            }
            branch.dropped.insert(tracker.to_owned());
            self.apply_sparse(&branch)?;
        } else {
            let definition = self.definition(tracker)?.clone();
            let report = self.capture_tracker(instance, tracker)?;
            branch = self.store.find_branch(instance)?;
            change.captured = Some(report.rev);
            crate::tracker_repo::clear_owned_paths(&workspace, &definition)?;
            branch.dropped.insert(tracker.to_owned());
        }
        // The resources it brought stop with it (stopped, not cleaned up:
        // using it again starts them on what they left).
        let supervisor = self.supervisor(&branch);
        let gone: Vec<String> = branch
            .resources
            .keys()
            .filter(|name| !self.resource_active(&branch, name))
            .cloned()
            .collect();
        for name in gone {
            if supervisor.running_pid(&name).is_some() {
                let _ = supervisor.stop(&name, "term");
            }
            branch.resources.remove(&name);
            change.stopped.push(name);
        }
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;
        Ok(change)
    }

    /// Bind `tracker` in `instance` again (`newgit tracker use`): a code
    /// tracker's files come back into the working tree, with whatever a drop
    /// stashed; a secret tracker's last content in this instance is projected
    /// back.
    pub fn use_tracker(&self, instance: &str, tracker: &str) -> Result<BindingChange> {
        // Held through the rebuild too, which saves the record it loads;
        // the inner change takes the same lock again.
        let (_lock, _) = self.locked_branch(instance)?;
        let before = self.binding_before(instance)?;
        let mut change = self.use_tracker_inner(instance, tracker, true)?;
        change.rebuilt = self.rebuild_for_binding(instance, &before)?;
        // A drop stopped what the tracker brings; using it again starts
        // them, after the rebuild, so they start on the binding they now
        // have. Only those: what this instance runs otherwise is left as
        // it is, stopped or not.
        if !change.started.is_empty() {
            let branch = self.store.find_branch(instance)?;
            let only: BTreeSet<&str> = change.started.iter().map(String::as_str).collect();
            change.autostarted = self.autostart(&branch, Some(&only));
        }
        Ok(change)
    }

    /// The use itself, without the rebuild: a tracker's `requires` are
    /// bound through here, so one `use` rebuilds once, for the binding it
    /// ends with, rather than once per tracker it brought.
    ///
    /// `outermost` binds the resources the change brings; a nested call for a
    /// requirement leaves that to the `use` that asked for it. A resource is
    /// prepared under the binding at the moment it binds, and with
    /// `requires = ["b", "c"]` the one `b` brings would otherwise be built
    /// while `c` is still dropped — and, being new to the instance, not
    /// rebuilt afterwards either.
    fn use_tracker_inner(
        &self,
        instance: &str,
        tracker: &str,
        outermost: bool,
    ) -> Result<BindingChange> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        self.require_workspace(&branch)?;
        let mut change = BindingChange {
            tracker: tracker.to_owned(),
            ..BindingChange::default()
        };
        let definition = if let Some(code) = self.code.iter().find(|d| d.name == tracker) {
            code.clone()
        } else {
            self.definition(tracker)?.clone()
        };
        if !branch.dropped.contains(tracker) {
            change.already = true;
            return Ok(change);
        }
        // One tracker per role: another bound in it makes way.
        if let Some(role) = definition.role.clone() {
            let swapped: Vec<String> = self
                .selectable()
                .into_iter()
                .filter(|other| {
                    other.name != tracker
                        && other.role.as_deref() == Some(role.as_str())
                        && !branch.dropped.contains(&other.name)
                })
                .map(|other| other.name.clone())
                .collect();
            // The incoming tracker counts as bound while the others go, so a
            // tracker that requires this role is not left without it.
            branch.dropped.remove(tracker);
            self.store.save_branch_record(&branch)?;
            for other in swapped {
                if let Err(error) = self.drop_tracker_inner(instance, &other, Uncommitted::Refuse) {
                    let mut branch = self.store.find_branch(instance)?;
                    branch.dropped.insert(tracker.to_owned());
                    self.store.save_branch_record(&branch)?;
                    return Err(error);
                }
                change.replaced.push(other);
            }
            branch = self.store.find_branch(instance)?;
        }
        branch.dropped.remove(tracker);
        let workspace = branch.workspace_path.clone();
        if !definition.secret {
            self.apply_sparse(&branch)?;
            let listing = crate::source::run_git_env(
                &[
                    "-C",
                    workspace.as_str(),
                    "stash",
                    "list",
                    "--format=%gd%x09%s",
                ],
                &[],
            )
            .unwrap_or_default();
            let wanted = stash_message(tracker);
            if let Some(stash) = listing.lines().find_map(|line| {
                let (reference, subject) = line.split_once('\t')?;
                subject.ends_with(&wanted).then(|| reference.to_owned())
            }) {
                crate::source::run_git_env(
                    &["-C", workspace.as_str(), "stash", "pop", "-q", &stash],
                    &[],
                )?;
                change.restored_stash = true;
            }
        } else if let Some(rev) = branch
            .trackers
            .get(tracker)
            .and_then(|binding| binding.content_rev.clone())
        {
            change.files = self.lane(tracker).restore(&workspace, &definition, &rev)?;
        }
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;

        // What it requires comes with it.
        for needed in &definition.requires {
            let branch = self.store.find_branch(instance)?;
            let selectable = self.selectable();
            let target = if selectable.iter().any(|d| &d.name == needed) {
                Some(needed.clone())
            } else {
                let members: Vec<&TrackerDefinition> = selectable
                    .into_iter()
                    .filter(|d| d.role.as_deref() == Some(needed.as_str()))
                    .collect();
                if members.iter().any(|m| !branch.dropped.contains(&m.name)) {
                    None
                } else {
                    let defaults = crate::config::personal_role_defaults(&self.config.project.name);
                    self.fill_role(needed, &members, &BindingChoice::default(), &defaults)?
                }
            };
            if let Some(target) = target
                && branch.dropped.contains(&target)
            {
                self.use_tracker_inner(instance, &target, false)?;
                change.brought.push(target);
            }
        }
        // And the resources it brings start, once everything it brings is
        // bound.
        if !outermost {
            return Ok(change);
        }
        let mut branch = self.store.find_branch(instance)?;
        let outcomes = self.bind_resources(&mut branch)?;
        self.store.save_branch_record(&branch)?;
        change.started = outcomes.into_iter().map(|outcome| outcome.name).collect();
        Ok(change)
    }

    /// What a `use` or `drop` starts from, for [`Self::rebuild_for_binding`]
    /// to compare against afterwards.
    fn binding_before(&self, instance: &str) -> Result<BindingBefore> {
        let branch = self.store.find_branch(instance)?;
        Ok(BindingBefore {
            bound: self.bound_vars(&branch),
            resources: branch.resources.keys().cloned().collect(),
        })
    }

    /// Rebuild every bound install whose tree the binding keys, after a
    /// `use` or `drop` moved the binding from `before`.
    ///
    /// The install store keys a tree on the trackers the instance binds: a
    /// shared lockfile lists every member, bound or not, and the install
    /// builds the bound ones, named by `{{bound.paths}}` or found on disk.
    /// So the tree an instance holds was built for the binding it had, and
    /// once that moves it is the tree of some other binding — whatever the
    /// lockfile says. Starting and stopping what the tracker *brings* does
    /// not reach it, because a shared install is brought by `root`, which is
    /// always bound. The binding here is the store key's: secret trackers
    /// count (`{{bound.trackers}}` names them), so binding or dropping one
    /// rebuilds too, through the store like any other.
    ///
    /// This is the same job a spawn does for the same tree, so it goes the
    /// same way: filled from the store when another instance built this
    /// binding already, else the producing action runs and publishes what it
    /// built. Only a tree that is there is rebuilt: a resource whose install
    /// never ran holds nothing the binding made stale, and one the change
    /// itself just bound was prepared under the new binding.
    ///
    /// The binding has moved by the time this runs, so nothing here fails
    /// the command; each install says how it went. One that fails — its
    /// action exits non-zero, or newgit cannot run it at all — is left
    /// `failed`, as a failed `prepare` at spawn is, with
    /// `newgit action <resource>.prepare` as the retry. One whose dependency
    /// is not ready is not rebuilt: it is reported as blocked and left
    /// `pending` behind it, its tree still the old binding's. The record is
    /// saved after each, so what finished is kept whatever comes next.
    ///
    /// Processes are left running: rebuilding beneath a live dev server is
    /// what `newgit action deps.prepare` does too.
    fn rebuild_for_binding(
        &self,
        instance: &str,
        before: &BindingBefore,
    ) -> Result<Vec<RebuiltInstall>> {
        let mut branch = self.store.find_branch(instance)?;
        if self.bound_vars(&branch) == before.bound {
            return Ok(Vec::new());
        }
        let mut rebuilt = Vec::new();
        for name in &self.graph.bind_order {
            if !branch.resources.contains_key(name) || !before.resources.contains(name) {
                continue;
            }
            let mut report = RebuiltInstall {
                name: name.clone(),
                ok: false,
                install: None,
                log: None,
                error: None,
                blocked_by: Vec::new(),
            };
            let definition = match self.resource_definition(name) {
                Ok(definition) => definition,
                Err(error) => {
                    report.error = Some(error.to_string());
                    rebuilt.push(report);
                    continue;
                }
            };
            if definition.identity_produces().is_empty()
                || !self.produced_present(&branch, definition)
            {
                continue;
            }
            let action_name = definition.producing_action();
            let Some(action) = definition.actions.get(action_name) else {
                continue;
            };
            report.blocked_by = self.blocked_dependencies(&branch, definition);
            let status = if !report.blocked_by.is_empty() {
                ResourceStatus::Pending
            } else {
                match self.rebuild_install(&mut branch, definition, action, action_name) {
                    Ok((ok, install, log)) => {
                        report.ok = ok;
                        report.install = install;
                        report.log = log;
                    }
                    Err(error) => report.error = Some(error.to_string()),
                }
                if report.ok {
                    ResourceStatus::Ready
                } else {
                    ResourceStatus::Failed
                }
            };
            if let Some(binding) = branch.resources.get_mut(name) {
                binding.status = status;
            }
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
            rebuilt.push(report);
        }
        Ok(rebuilt)
    }

    /// One install's rebuild for [`Self::rebuild_for_binding`]: filled from
    /// the store, else the producing action run and its tree stored. Whether
    /// it succeeded, what the store did, and the log if the action ran.
    fn rebuild_install(
        &self,
        branch: &mut BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
        action_name: &str,
    ) -> Result<(bool, Option<InstallReport>, Option<Utf8PathBuf>)> {
        let install_key = match self.install_key(branch, definition) {
            Some(Ok(key)) => Ok(key),
            Some(Err(report)) => Err(Some(report)),
            None => Err(None),
        };
        let filled = install_key
            .as_deref()
            .ok()
            .and_then(|key| self.refill_from_install_store(branch, definition, key));
        if let Some(InstallReport::Filled { key, method }) = filled {
            return Ok((true, Some(InstallReport::Filled { key, method }), None));
        }
        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));
        let (code, captured) =
            self.run_one_shot(branch, definition, action, action_name, &log, true)?;
        Self::apply_captures(branch, &definition.name, captured.found);
        let ok = code == 0;
        let install = match (&install_key, filled) {
            (Ok(key), _) if ok => self.admit_to_install_store(branch, definition, key),
            (Ok(_), unavailable) => unavailable,
            (Err(report), _) => report.clone(),
        };
        Ok((ok, install, Some(log)))
    }

    /// The working tree follows the binding: every dropped code tracker's
    /// paths left out (Git's sparse checkout), with any bound tracker nested
    /// inside one put back.
    fn apply_sparse(&self, branch: &BranchInstance) -> Result<()> {
        let workspace = branch.workspace_path.as_str();
        let dropped: Vec<&TrackerDefinition> = self
            .code
            .iter()
            .filter(|definition| branch.dropped.contains(&definition.name))
            .collect();
        if dropped.is_empty() {
            crate::source::run_git_env(&["-C", workspace, "sparse-checkout", "disable"], &[])?;
            return Ok(());
        }
        let mut patterns = vec!["/*".to_owned()];
        for definition in &dropped {
            for path in &definition.paths {
                patterns.push(format!("!/{path}/"));
                patterns.push(format!("!/{path}"));
            }
        }
        for definition in &self.code {
            if branch.dropped.contains(&definition.name) {
                continue;
            }
            for path in &definition.paths {
                let inside = dropped.iter().any(|outer| {
                    outer
                        .paths
                        .iter()
                        .any(|outer| path != outer && path.starts_with(outer))
                });
                if inside {
                    patterns.push(format!("/{path}/"));
                    patterns.push(format!("/{path}"));
                }
            }
        }
        let mut args = vec!["-C", workspace, "sparse-checkout", "set", "--no-cone"];
        args.extend(patterns.iter().map(String::as_str));
        crate::source::run_git_env(&args, &[])?;
        Ok(())
    }

    pub fn capture_tracker(&self, instance: &str, tracker: &str) -> Result<CaptureReport> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        // A lane is shared by every instance, so this instance's rendered
        // values must not enter it — but the file cannot simply be skipped
        // either, or a key added beside them would never reach the lane.
        // Reversing the substitution keeps the edits and drops the values.
        let reversed = self.unrender_for_capture(&branch, definition)?;

        let previous = branch
            .trackers
            .get(&definition.name)
            .and_then(|binding| binding.content_rev.clone());
        let lane = self.lane(&definition.name);
        let capture = lane.capture(
            &branch.workspace_path,
            definition,
            CaptureAt {
                parent: previous.as_deref(),
                branch: Branch::Instance(&branch.slug),
                message: &format!("{} in `{}`", definition.name, branch.name),
            },
        );
        reversed.restore()?;
        let capture = capture?;

        let changed = previous.as_deref() != Some(capture.rev.as_str());
        branch.trackers.insert(
            definition.name.clone(),
            TrackerBinding {
                definition_rev: definition.definition_rev.clone(),
                content_rev: Some(capture.rev.clone()),
            },
        );
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;

        Ok(CaptureReport {
            rev: capture.rev,
            files: capture.files,
            changed,
        })
    }

    /// Seed a lane from the store repo's working tree and make it the lane
    /// head.
    ///
    /// A lane starts empty and [`Self::capture_tracker`] reads from an instance
    /// workspace, so the first instance of an env-carrying tracker is
    /// guaranteed to come up without its files. In a project adopting newgit
    /// that content already exists in the store repo at the same relative
    /// paths, so read it from there rather than round-tripping it through an
    /// instance. Seeding sets the lane head directly: there is no binding
    /// record to promote from, and the point is that the next `spawn` works.
    pub fn seed_tracker_from_store(&self, tracker: &str) -> Result<SeedReport> {
        // `definition_or_load`, like `track_paths`: seeding usually follows
        // `tracker track` closely enough to beat a reopened manager.
        let definition = &self.definition_or_load(tracker)?;
        if definition.paths.is_empty() {
            return Err(NewgitError::TrackerHasNoPaths(definition.name.clone()));
        }

        let root = self.store.paths().project_root.clone();
        let (present, missing): (Vec<_>, Vec<_>) = definition
            .paths
            .iter()
            .cloned()
            .partition(|path| root.join(path).exists());
        if present.is_empty() {
            return Err(NewgitError::NothingToSeed {
                tracker: definition.name.clone(),
                paths: missing
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }

        let lane = self.lane(&definition.name);
        let main = lane.main();
        let capture = lane.capture(
            &root,
            definition,
            CaptureAt {
                parent: main.as_deref(),
                branch: Branch::Main,
                message: &format!("{} seeded from the project", definition.name),
            },
        )?;
        let changed = capture.committed;

        Ok(SeedReport {
            rev: capture.rev,
            files: capture.files,
            changed,
            missing_paths: missing,
        })
    }

    pub fn checkout_tracker(
        &self,
        instance: &str,
        tracker: &str,
        rev: Option<&str>,
    ) -> Result<RestoreReport> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;
        let lane = self.lane(&definition.name);

        let target_rev = match rev {
            Some(rev) => rev.to_owned(),
            None => branch
                .trackers
                .get(&definition.name)
                .and_then(|binding| binding.content_rev.clone())
                .ok_or_else(|| {
                    NewgitError::Unsupported(format!(
                        "tracker `{}` has no bound content for `{}`; pass --rev",
                        definition.name, branch.name
                    ))
                })?,
        };
        if !lane.has_rev(&target_rev) {
            return Err(NewgitError::NoSnapshot {
                tracker: definition.name.clone(),
                rev: target_rev,
            });
        }

        // Restoring never loses state: current content is captured first —
        // with renders reversed, so the safety rev is a lane rev like any
        // other rather than one instance's ports.
        let reversed = self.unrender_for_capture(&branch, definition)?;
        let bound = branch
            .trackers
            .get(&definition.name)
            .and_then(|binding| binding.content_rev.clone());
        let safety = lane.capture(
            &branch.workspace_path,
            definition,
            CaptureAt {
                parent: bound.as_deref(),
                branch: Branch::Instance(&branch.slug),
                message: &format!(
                    "{} in `{}`, before checking out {target_rev}",
                    definition.name, branch.name
                ),
            },
        );
        reversed.restore()?;
        let safety = safety?;
        let safety_rev = (safety.rev != target_rev).then_some(safety.rev);

        let files = lane.restore(&branch.workspace_path, definition, &target_rev)?;

        branch.trackers.insert(
            definition.name.clone(),
            TrackerBinding {
                definition_rev: definition.definition_rev.clone(),
                content_rev: Some(target_rev.clone()),
            },
        );
        // The checked-out content is committed content; this instance's
        // values go back on top of it.
        let warnings = self.rerender_all(&mut branch);
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;

        Ok(RestoreReport {
            rev: target_rev,
            files,
            safety_rev,
            warnings,
        })
    }

    /// Pull a tracker's lane head into an existing instance.
    pub fn pull_tracker(&self, instance: &str, tracker: &str) -> Result<TrackerBindOutcome> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        if self.lane(&definition.name).main().is_none() {
            return Err(NewgitError::Unsupported(format!(
                "tracker `{}` has no merged content to pull",
                definition.name
            )));
        }

        let mut outcome = self.bind_tracker(&mut branch, definition, true)?;
        outcome.warnings = self.rerender_all(&mut branch);
        self.store.save_branch_record(&branch)?;
        Ok(outcome)
    }

    /// Promote this branch instance's bound tracker revision to the lane head.
    pub fn merge_tracker(&self, instance: &str, tracker: &str) -> Result<MergeTrackerOutcome> {
        let branch = self.store.find_branch(instance)?;
        let definition = self.definition(tracker)?;
        let rev = branch
            .trackers
            .get(&definition.name)
            .and_then(|binding| binding.content_rev.clone())
            .ok_or_else(|| {
                NewgitError::Unsupported(format!(
                    "tracker `{}` has no captured content for `{}`; run `newgit tracker capture {}` first",
                    definition.name, branch.name, definition.name
                ))
            })?;
        let lane = self.lane(&definition.name);
        if !lane.has_rev(&rev) {
            return Err(NewgitError::NoSnapshot {
                tracker: definition.name.clone(),
                rev,
            });
        }
        lane.set_main(&rev)?;
        Ok(MergeTrackerOutcome {
            tracker: definition.name.clone(),
            rev,
        })
    }

    pub fn create_tracker(
        &self,
        name: &str,
        audience: &str,
        storage: Storage,
        merge_with_source: bool,
    ) -> Result<AddTrackerOutcome> {
        validate_name(name)?;
        self.refuse_root(name, "it always exists")?;
        // A remote tracker with nowhere to sync would behave exactly like a
        // local one while claiming otherwise. Refuse it here, where the claim
        // is made; definitions written by hand are not bricked on load.
        if storage == Storage::Remote && self.config.remote.is_none() {
            return Err(NewgitError::Unsupported(format!(
                "tracker `{name}` would use `storage = \"remote\"`, but .newgit/config.toml has no \
                 [remote] to sync it to; add one (`[remote] url = \"https://...\"`), or create \
                 the tracker with `--storage local`"
            )));
        }
        let definition = TrackerDefinition::new(
            name,
            audience.to_owned(),
            storage,
            merge_with_source,
            Vec::new(),
        )?;
        let path = self.store.create_tracker_definition(&definition)?;
        Ok(AddTrackerOutcome {
            path,
            ignored_patterns: Vec::new(),
        })
    }

    pub fn track_paths(&self, tracker: &str, paths: &[Utf8PathBuf]) -> Result<TrackPathsOutcome> {
        // As the definition spells them: Git reads the ignore line `/./.env`
        // as no file at all, so `./.env` has to become `/.env`.
        let paths: Vec<Utf8PathBuf> = paths.iter().map(|path| normalize_path(path)).collect();
        let definition = self.definition_or_load(tracker)?;
        let updated = definition.with_added_paths(&paths)?;
        let mut all = self.trackers.clone();
        all.push(self.root.clone());
        validate_ownership_with_replacement(&all, &updated)?;
        let path = self.store.save_tracker_definition(&updated)?;

        let mut patterns = Vec::new();
        for owned in &paths {
            if !self.source.is_ignored(owned.as_str())? {
                patterns.push(format!("/{owned}"));
            }
        }
        self.store.append_gitignore(tracker, &patterns)?;

        // If the content is already sitting in the store repo, seeding the lane
        // from it is the next thing you want; the caller says so.
        let root = &self.store.paths().project_root;
        let seedable = paths.iter().any(|owned| root.join(owned).exists());

        Ok(TrackPathsOutcome {
            path,
            added_paths: paths,
            ignored_patterns: patterns,
            seedable,
        })
    }

    /// The inverse of `tracker create` — undoes exactly what `tracker
    /// create`/`tracker track` did: the definition file, the store
    /// `.gitignore` block, and each live workspace's `.git/info/exclude`
    /// entries. The lane's captured revs in the lanes repo are
    /// left in place; see the caller for why that is safe to leave to
    /// `newgit cleanup`.
    pub fn remove_tracker(&self, name: &str) -> Result<RemoveTrackerOutcome> {
        self.refuse_root(name, "it cannot be removed")?;

        let definition = self
            .trackers
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| {
                NewgitError::Unsupported(format!(
                    "no tracker named `{name}`; defined trackers: {}",
                    if self.trackers.is_empty() {
                        "none defined".to_owned()
                    } else {
                        self.trackers
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ))
            })?;

        let branches = self.store.load_branches()?;
        let bound: Vec<&str> = branches
            .iter()
            .filter(|branch| branch.trackers.contains_key(name))
            .map(|branch| branch.name.as_str())
            .collect();
        if !bound.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "tracker `{name}` is bound to {}; remove {} first with `newgit remove <instance>`",
                bound.join(", "),
                if bound.len() == 1 { "it" } else { "them" }
            )));
        }

        // Reverse `tracker track` in every live workspace, not just the ones
        // currently bound to this tracker: a tracker created after a
        // workspace was spawned never reached that workspace's
        // `.git/info/exclude`, so this is a no-op there, but it is the
        // mechanical inverse of what `spawn` writes rather than a guess at
        // which workspaces need it.
        let mut workspaces_cleared = Vec::new();
        for branch in &branches {
            if branch.workspace_path.is_dir()
                && unexclude_tracker_paths(&branch.workspace_path, &definition.paths)?
            {
                workspaces_cleared.push(branch.name.clone());
            }
        }
        let gitignore_removed = self.store.remove_gitignore_block(name)?;

        let path = self.store.delete_tracker_definition(name)?;
        Ok(RemoveTrackerOutcome {
            path,
            gitignore_removed,
            workspaces_cleared,
        })
    }

    /// How far `branch`'s recorded base has moved, read fresh from the
    /// store repo's own refs — never cached, so the answer is exactly as
    /// current as the store's last fetch of upstream. `None` when the
    /// instance has no recorded base (its source branch already existed at
    /// spawn time) or the base branch has since been deleted in the store —
    /// in either case there is nothing left to compare against, so `status`
    /// says nothing rather than guess.
    fn base_report(&self, branch: &BranchInstance) -> Option<BaseReport> {
        let base_ref = branch.base_ref.as_ref()?;
        let base_rev = branch.base_rev.as_ref()?;
        let current_tip = self.source.ref_rev(&format!("refs/heads/{base_ref}"))?;
        let ahead = self
            .source
            .commit_count(&format!("{base_rev}..{current_tip}"))
            .ok()?;
        Some(BaseReport {
            base_ref: base_ref.clone(),
            ahead,
        })
    }

    pub fn statuses(&self) -> Result<Vec<InstanceReport>> {
        let lane_heads: Vec<(String, Option<String>)> = self
            .trackers
            .iter()
            .map(|definition| (definition.name.clone(), self.lane(&definition.name).main()))
            .collect();

        self.store
            .load_branches()?
            .into_iter()
            .map(|branch| {
                let workspace_exists = branch.workspace_path.is_dir();
                let live_rev = workspace_exists
                    .then(|| GitSource::workspace_short_head(&branch.workspace_path).ok())
                    .flatten();
                let trackers = self
                    .trackers
                    .iter()
                    .map(|definition| {
                        let content_rev = branch
                            .trackers
                            .get(&definition.name)
                            .and_then(|binding| binding.content_rev.clone());
                        let head = lane_heads
                            .iter()
                            .find(|(name, _)| name == &definition.name)
                            .and_then(|(_, head)| head.clone());
                        TrackerReport {
                            name: definition.name.clone(),
                            content_rev,
                            lane_head: head,
                        }
                    })
                    .collect();
                let supervisor = self.supervisor(&branch);
                let resources = self
                    .resources
                    .iter()
                    .map(|definition| {
                        let state = match branch.resources.get(&definition.name) {
                            None => "—".to_owned(),
                            Some(binding) => {
                                if supervisor.running_pid(&definition.name).is_some() {
                                    // Something newgit started is alive right now:
                                    // the one signal worth trusting over anything
                                    // in the record.
                                    "running".to_owned()
                                } else if supervisor.has_ever_started(&definition.name) {
                                    // A supervised process existed and is not
                                    // alive now — "stopped" is honest here in a
                                    // way it is not for an action newgit never
                                    // ran. Which of possibly several
                                    // `long_running` actions it was is not
                                    // something the supervisor records (it
                                    // tracks one process per resource, not per
                                    // action), so this can't and doesn't claim
                                    // more than "it ran, and isn't running now."
                                    "stopped".to_owned()
                                } else if binding.status == ResourceStatus::Failed {
                                    // Its own failure (export, render or
                                    // prepare) comes first, as it does in
                                    // `spawn` and `spawn --json`: repairing a
                                    // dependency would not fix it.
                                    "failed".to_owned()
                                } else {
                                    let blocked_by = self.blocked_dependencies(&branch, definition);
                                    if !blocked_by.is_empty() {
                                        format!("blocked({})", blocked_by.join(","))
                                    } else if binding.status == ResourceStatus::Pending {
                                        "pending".to_owned()
                                    } else {
                                        "ready".to_owned()
                                    }
                                }
                            }
                        };
                        ResourceReport {
                            name: definition.name.clone(),
                            state,
                        }
                    })
                    .collect();
                let base = self.base_report(&branch);
                let readiness = self.readiness(&branch);
                Ok(InstanceReport {
                    branch,
                    workspace_exists,
                    live_rev,
                    trackers,
                    resources,
                    base,
                    readiness,
                })
            })
            .collect()
    }

    /// Deletes the workspace (plain `rm -rf`; clones have no registration)
    /// and archives the binding record. The source branch in the store is
    /// kept — removal disposes of the workspace, not the history.
    ///
    /// Resource cleanup hooks run first, dependents before dependencies and
    /// while the workspace still exists. Without that, a resource newgit
    /// does not own — a cloud preview, a database — would outlive every
    /// trace of the instance that asked for it.
    /// Deliberately not gated on [`Self::require_resolvable_graph`]: teardown
    /// must stay reachable from a broken graph, and cleanup hooks run for every
    /// bound resource regardless of how they are ordered relative to each other.
    /// The one graph problem it refuses is a name declared twice that reaches
    /// this instance's environment with two values, which a hook could act
    /// on either of — see `run_cleanup_hooks`.
    ///
    /// Checkpoints outlive removal by default — they pin the lane revs their
    /// undo would need, and the instance may be re-created. `checkpoints =
    /// Purge` says that undo will never be wanted, and drops them so the next
    /// `cleanup` can reclaim what they held.
    ///
    /// The source branch goes too when it holds nothing unique (see
    /// [`SourceBranchPolicy::Auto`]); the decision is made before anything
    /// is torn down, so a `Delete` that cannot be honoured refuses the whole
    /// removal rather than half of it.
    pub fn remove(
        &self,
        name: &str,
        cwd: &Utf8Path,
        checkpoints: ArchivedCheckpoints,
        source_branch: SourceBranchPolicy,
    ) -> Result<RemoveOutcome> {
        let (_lock, branch) = self.locked_branch(name)?;

        if cwd.starts_with(&branch.workspace_path) {
            return Err(NewgitError::Unsupported(format!(
                "the current directory is inside the workspace of `{}`; step out of it before \
                 removing",
                branch.name
            )));
        }
        // Whatever would make the cleanup hooks refuse refuses here, before
        // anything is stopped or deleted: a dry run checks each hook and runs
        // none.
        self.run_cleanup_hooks(&branch, true)?;

        let policy = source_branch;
        let (mut source_branch, via_outbox) = self.decide_source_branch(&branch, policy)?;
        let unsaved_workspace_head = self.unsaved_workspace_head(&branch);

        // Stop anything still running before the workspace disappears.
        let supervisor = self.supervisor(&branch);
        for definition in self.running_resources(&branch) {
            supervisor.stop(&definition.name, &definition.stop_signal())?;
        }

        let hooks = self.run_cleanup_hooks(&branch, false)?;

        let state_dir = self.store.instance_state_dir(&branch.slug);
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir)
                .map_err(|source| NewgitError::io(state_dir, source))?;
        }

        // Off the remote first, while the record still says where it is: this
        // deletes the fork and revokes the workspace's token and the Git
        // passwords it holds.
        let remote_warning = match (&self.remote, &branch.remote) {
            (Some(link), Some(binding)) => link
                .connect(Acting::Login)
                .and_then(|remote| remote.remove_instance(&binding.project, &branch.slug))
                .err()
                .map(|error| error.to_string()),
            _ => None,
        };

        self.materializer().remove(&branch)?;
        // Its branches go with it; pins keep whatever its archived
        // checkpoints still name.
        for definition in &self.trackers {
            self.lane(&definition.name).drop_instance(&branch.slug)?;
        }
        for definition in self.code_definitions() {
            self.lane(&definition.name).drop_instance(&branch.slug)?;
            let _ = self.source.delete_ref(&local_tracker_ref(
                &definition.name,
                &format!("instances/{}", branch.slug),
            ));
        }
        let archived_record = self.store.archive_branch_record(&branch)?;
        // Either way, count what the instance leaves behind: checkpoints kept
        // for an unreachable instance are disk nobody will reclaim by accident.
        let keeping = checkpoints == ArchivedCheckpoints::Keep;
        let found = self.purge_checkpoints(&branch.slug, keeping)?;
        let (purged_checkpoints, kept_checkpoints) = match (keeping, found) {
            (true, found) => (None, found.map_or(0, |plan| plan.checkpoints)),
            (false, purged) => (purged, 0),
        };

        // Last, so an error anywhere above leaves the branch untouched. By
        // now the record is archived and the removal has happened, so
        // nothing below may fail it: every way the deletion can go wrong
        // becomes a kept branch with the reason, never an error.
        if let SourceBranchOutcome::Deleted { tip, .. } = &source_branch {
            let tip = tip.clone();
            source_branch = self.delete_source_branch(&branch, policy, tip, via_outbox);
        }

        Ok(RemoveOutcome {
            branch,
            archived_record,
            hooks,
            purged_checkpoints,
            kept_checkpoints,
            remote_warning,
            source_branch,
            unsaved_workspace_head,
        })
    }

    /// Carry out a decided deletion. Cleanup hooks ran between the decision
    /// and here, and may have taken long enough for the ref that held the
    /// tip to move or go — two parallel removals of branches sharing a tip
    /// would otherwise each count the other and both delete. So `Auto`
    /// asks again, and keeps the branch if the answer changed.
    fn delete_source_branch(
        &self,
        branch: &BranchInstance,
        policy: SourceBranchPolicy,
        tip: String,
        via_outbox: bool,
    ) -> SourceBranchOutcome {
        let own = format!("refs/heads/{}", branch.source_ref);
        let mut found_on = None;
        if policy == SourceBranchPolicy::Auto {
            // Re-read the outbox only if the decision came from a fresh
            // read of it; otherwise it may be as old as the last push.
            let outbox = if via_outbox {
                OutboxRead::AsIs
            } else {
                OutboxRead::Skip
            };
            match self.ref_holding(&tip, &own, outbox) {
                Ok(Holding {
                    found_on: Some(name),
                    ..
                }) => found_on = Some(name),
                _ => return SourceBranchOutcome::Kept(KeptBranch::Unique { tip }),
            }
        } else if let Ok(holding) = self.ref_holding(&tip, &own, OutboxRead::Skip) {
            found_on = holding.found_on;
        }

        if self.source.delete_branch(&branch.source_ref, &tip).is_err() {
            return SourceBranchOutcome::Kept(KeptBranch::Moved {
                expected: tip,
                now: self.source.ref_rev(&own),
            });
        }
        let pinned_by_checkpoint = crate::source::refs_containing_in(
            self.source.root(),
            &tip,
            &["refs/newgit/checkpoints"],
        )
        .is_ok_and(|refs| !refs.is_empty());
        SourceBranchOutcome::Deleted {
            tip,
            found_on,
            pinned_by_checkpoint,
        }
    }

    /// The workspace's HEAD, when no ref in the store contains it — work
    /// since the last checkpoint that removing the workspace destroys.
    fn unsaved_workspace_head(&self, branch: &BranchInstance) -> Option<String> {
        if !branch.workspace_path.is_dir() {
            return None;
        }
        let head = GitSource::workspace_head(&branch.workspace_path).ok()?;
        // An object the store never received is an error here, which is
        // the same answer as "no ref contains it".
        let saved = crate::source::refs_containing_in(self.source.root(), &head, &["refs"])
            .is_ok_and(|refs| !refs.is_empty());
        (!saved).then_some(head)
    }

    /// Decide what `remove` does with the source branch, without doing it.
    ///
    /// "Nothing unique" means the tip is in the history of some other
    /// branch, tag, or remote-tracking ref in the store, or of a branch the
    /// publish remote has right now — read by refreshing the outbox, since
    /// that is where a workspace's pushes land and the store's own
    /// `refs/remotes` never hears about them. newgit's checkpoint refs do
    /// not count: they are undo history, not a place a person would look
    /// for their work.
    ///
    /// Also returns whether the answer came from the outbox, so the
    /// re-check before deleting knows whether it may read it again.
    fn decide_source_branch(
        &self,
        branch: &BranchInstance,
        policy: SourceBranchPolicy,
    ) -> Result<(SourceBranchOutcome, bool)> {
        let own = format!("refs/heads/{}", branch.source_ref);
        let Some(tip) = self.source.ref_rev(&own) else {
            return Ok((SourceBranchOutcome::Missing, false));
        };
        if policy == SourceBranchPolicy::Keep {
            return Ok((SourceBranchOutcome::Kept(KeptBranch::Asked), false));
        }
        if let Some(path) = self.source.checked_out_at(&branch.source_ref)? {
            if policy == SourceBranchPolicy::Delete {
                return Err(NewgitError::Unsupported(format!(
                    "cannot delete source branch `{}`: it is checked out in the store at {path}. \
                     Switch that checkout to another branch first, or remove with \
                     `--keep-branch`",
                    branch.source_ref
                )));
            }
            return Ok((
                SourceBranchOutcome::Kept(KeptBranch::CheckedOut(path)),
                false,
            ));
        }
        if policy == SourceBranchPolicy::Auto && !branch.created_source_branch {
            return Ok((SourceBranchOutcome::Kept(KeptBranch::Adopted), false));
        }
        if policy == SourceBranchPolicy::Delete {
            // Deleting regardless: where else the tip lives is reported
            // after the fact, from local refs, with no network.
            return Ok((
                SourceBranchOutcome::Deleted {
                    tip,
                    found_on: None,
                    pinned_by_checkpoint: false,
                },
                false,
            ));
        }

        let holding = self.ref_holding(&tip, &own, OutboxRead::Refresh)?;
        let outcome = match (holding.found_on, holding.remote_error) {
            (Some(found_on), _) => SourceBranchOutcome::Deleted {
                tip,
                found_on: Some(found_on),
                pinned_by_checkpoint: false,
            },
            (None, Some(error)) => {
                SourceBranchOutcome::Kept(KeptBranch::RemoteUnconfirmed { tip, error })
            }
            (None, None) => SourceBranchOutcome::Kept(KeptBranch::Unique { tip }),
        };
        Ok((outcome, holding.via_outbox))
    }

    /// One ref other than `own` whose history contains `tip`, named for a
    /// reader: local branches first, then remote-tracking refs, then tags,
    /// then the publish remote itself. The remote is asked only when no
    /// local ref answers, since asking costs a fetch.
    fn ref_holding(&self, tip: &str, own: &str, outbox: OutboxRead) -> Result<Holding> {
        let mut store = self.source.refs_containing(tip)?;
        store.retain(|name| name != own);
        let named = |prefix: &str| {
            store
                .iter()
                .find_map(|name| name.strip_prefix(prefix))
                .map(ToOwned::to_owned)
        };
        let local = named("refs/heads/")
            .map(|name| format!("`{name}`"))
            .or_else(|| named("refs/remotes/").map(|name| format!("`{name}`")))
            .or_else(|| named("refs/tags/").map(|name| format!("tag `{name}`")));
        if local.is_some() {
            return Ok(Holding {
                found_on: local,
                via_outbox: false,
                remote_error: None,
            });
        }

        // The outbox mirrors the publish remote only as of its last sync,
        // and a branch can vanish from the remote after it — a PR closed
        // unmerged, a force-push. Counting a stale mirror would delete the
        // only copy, so it is refreshed first, and a refresh that fails
        // means the remote is not counted at all.
        let outbox_dir = self.outbox();
        let mut remote_error = None;
        let readable = match outbox {
            OutboxRead::Skip => false,
            OutboxRead::AsIs => outbox_dir.join("HEAD").is_file(),
            OutboxRead::Refresh => match self.publish_url()? {
                None => false,
                Some(url) => match self
                    .source
                    .ensure_outbox(&outbox_dir)
                    .and_then(|()| GitSource::sync_outbox(&outbox_dir, &url, None))
                {
                    Ok(()) => true,
                    Err(error) => {
                        // Git's own reason, not the whole fetch command line.
                        let text = error.to_string();
                        let reason = text
                            .split_once("fatal: ")
                            .map_or(text.as_str(), |(_, reason)| reason);
                        remote_error = Some(reason.lines().next().unwrap_or("").trim().to_owned());
                        false
                    }
                },
            },
        };
        // The outbox borrows the store's objects, so `tip` resolves there;
        // a read that fails anyway only means one fewer place to find it.
        let pushed = if readable {
            crate::source::refs_containing_in(&outbox_dir, tip, &["refs/heads"])
                .unwrap_or_default()
                .iter()
                .find_map(|name| name.strip_prefix("refs/heads/"))
                .map(|name| format!("`{PUBLISH_REMOTE}/{name}`"))
        } else {
            None
        };
        Ok(Holding {
            via_outbox: pushed.is_some(),
            found_on: pushed,
            remote_error,
        })
    }

    /// Drop one instance's checkpoint log and the store refs it held.
    ///
    /// This is the only operation that can break an undo, so nothing calls it
    /// implicitly: a checkpoint is what `newgit undo` restores, and the lane
    /// revs it names are pinned for exactly that reason. Once the binding
    /// record is archived the undo is unreachable anyway — but "unreachable"
    /// is still the user's call to make, not a garbage collector's.
    ///
    /// Returns `None` when the instance has no checkpoints at all.
    fn purge_checkpoints(&self, slug: &str, dry_run: bool) -> Result<Option<PurgedCheckpoints>> {
        let dir = self.store.checkpoint_dir(slug);
        if !dir.is_dir() {
            return Ok(None);
        }
        let checkpoints = CheckpointLog::new(dir.clone(), slug).list()?.len();
        let refs = self
            .source
            .refs_under(&format!("refs/newgit/checkpoints/{slug}"))?;

        if !dry_run {
            for name in &refs {
                self.source.delete_ref(name)?;
            }
            std::fs::remove_dir_all(&dir).map_err(|source| NewgitError::io(&dir, source))?;
        }

        Ok(Some(PurgedCheckpoints {
            slug: slug.to_owned(),
            checkpoints,
            source_refs: refs.len(),
            dir,
        }))
    }

    /// Run each bound resource's `[cleanup] command`, dependents before
    /// dependencies. Ownership decides whether a hook may run at all —
    /// `project` and `user` resources are shared beyond this instance, so
    /// per-branch teardown leaves them alone even when they define a hook.
    ///
    /// Reverses the *lifecycle* order, not the bind order: reading one string
    /// out of a resource says nothing about what has to be torn down first
    /// (#43). Only `depends_on` claims that.
    fn run_cleanup_hooks(
        &self,
        branch: &BranchInstance,
        dry_run: bool,
    ) -> Result<Vec<HookOutcome>> {
        let mut outcomes = Vec::new();
        for name in self.graph.lifecycle_order.iter().rev() {
            if !branch.resources.contains_key(name) {
                continue;
            }
            let definition = self.resource_definition(name)?;
            let ownership = definition.ownership;

            if !may_tear_down(ownership) {
                outcomes.push(HookOutcome {
                    resource: name.clone(),
                    ownership,
                    detail: HookDetail::SkippedOwnership,
                });
                continue;
            }

            let Some(template) = definition
                .cleanup
                .as_ref()
                .and_then(|spec| spec.command.as_deref())
            else {
                outcomes.push(HookOutcome {
                    resource: name.clone(),
                    ownership,
                    detail: HookDetail::NoHook,
                });
                continue;
            };

            let binding = branch.resources.get(name);
            let state_ref = self.checkpointed_state_ref(branch, name)?;
            let bound = self.bound_vars(branch);
            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: binding.map(|binding| &binding.resolved_ports),
                exports: binding.map(|binding| &binding.resolved_exports),
                snapshot_path: None,
                state_ref: state_ref.as_deref(),
                bound: Some(&bound),
            };
            let command = render(template, &context);

            if let Some(placeholder) = unresolved_placeholder(&command) {
                outcomes.push(HookOutcome {
                    resource: name.clone(),
                    ownership,
                    detail: HookDetail::SkippedUnresolved {
                        command: command.clone(),
                        placeholder: placeholder.to_owned(),
                    },
                });
                continue;
            }

            // Not `assemble_env`: it refuses a graph that does not resolve,
            // and teardown is the way out of one. An edge that does not is
            // skipped rather than fatal, so every bound resource is still
            // layered and the hook gets every variable. The order decides
            // only a name two declarations claim, which a resolvable graph
            // refuses. Where both reach this instance with different values,
            // the hook would get whichever is layered last and could tear
            // down the wrong thing with it, so it is refused, in a dry run
            // too. Every hook gets the same environment, so the first one to
            // get here refuses before any has run.
            let (env, clashing) = self.layered_env(branch);
            let collision = self
                .graph
                .problems
                .iter()
                .find_map(|problem| match problem {
                    GraphProblem::EnvNameCollision { name: variable, .. }
                        if clashing.contains(variable) =>
                    {
                        Some((variable, problem))
                    }
                    _ => None,
                });
            if let Some((variable, problem)) = collision {
                return Err(NewgitError::Unsupported(format!(
                    "refusing to tear down `{}`, whose `[cleanup]` hooks would get either of two \
                     values for `{variable}`: {problem}",
                    branch.name
                )));
            }

            if dry_run {
                outcomes.push(HookOutcome {
                    resource: name.clone(),
                    ownership,
                    detail: HookDetail::WouldRun(command),
                });
                continue;
            }

            // The workspace is usually still here; when cleanup is finishing
            // an instance whose workspace is already gone, the hook runs
            // from the store root so an external teardown can still reach
            // its own API. `workdir` is workspace-relative, so it has
            // nothing to resolve against once the workspace itself is gone.
            let cwd = if branch.workspace_path.is_dir() {
                resource_cwd(
                    &branch.workspace_path,
                    name,
                    "cleanup",
                    definition.workdir_for(None),
                )?
            } else {
                self.store.paths().project_root.clone()
            };

            let log = self
                .store
                .action_log_path(&branch.slug, &format!("{name}.cleanup"));
            let env: Vec<(String, String)> =
                env.into_iter().map(|var| (var.name, var.value)).collect();
            let (code, _) = run_captured(&command, &cwd, &env, &log)?;
            outcomes.push(HookOutcome {
                resource: name.clone(),
                ownership,
                detail: HookDetail::Ran {
                    command,
                    ok: code == 0,
                    log,
                },
            });
        }
        Ok(outcomes)
    }

    /// The most recent checkpointed state reference for one resource, which
    /// is what a cleanup hook's `{{state_ref}}` means: the handle newgit last
    /// recorded. Deposited content resolves to its path, like restore, and a
    /// `hash:` ref resolves to nothing — see
    /// [`ResourceState::consumable_state_ref`].
    fn checkpointed_state_ref(
        &self,
        branch: &BranchInstance,
        resource: &str,
    ) -> Result<Option<String>> {
        let records = self.checkpoint_log(branch).list()?;
        for record in records.iter().rev() {
            if let Some(state) = record
                .resource_states
                .iter()
                .find(|state| state.name == resource)
                && let Some(resolved) = self.consumable_state_ref(state)?
            {
                return Ok(Some(resolved));
            }
        }
        Ok(None)
    }

    /// Garbage collection across everything: finish instances whose
    /// workspace is gone, delete unclaimed workspaces and dead process
    /// state, and prune lane revs nothing references.
    ///
    /// `remove` targets one instance; this is the sweep. It never deletes a
    /// checkpoint record, and never a lane rev a checkpoint still points at —
    /// unless `archived = Purge`, which discards the checkpoint logs of
    /// instances whose binding record is already gone, and so releases the
    /// revs those logs were the only claim on.
    pub fn cleanup(&self, dry_run: bool, archived: ArchivedCheckpoints) -> Result<CleanupOutcome> {
        let mut outcome = CleanupOutcome {
            dry_run,
            ..CleanupOutcome::default()
        };
        let branches = self.store.load_branches()?;

        // An instance with no workspace cannot run, checkpoint, or undo, and
        // its name stays taken — so finishing the teardown is the only move
        // that helps. Its binding record is archived, not deleted.
        let (live, stale): (Vec<_>, Vec<_>) = branches
            .iter()
            .partition(|branch| branch.workspace_path.is_dir());

        // Stale when listed, live again by the time this got its lock.
        let mut revived = Vec::new();
        for branch in &stale {
            let _lock = self.lock_instance(&branch.slug, &branch.name)?;
            // Read again under the lock: a `remove` that finished while this
            // waited has already torn it down and archived the record, and a
            // `spawn` may have brought the name back with a new workspace.
            let branch = match self.store.find_branch(&branch.slug) {
                Ok(branch) if branch.workspace_path.is_dir() => {
                    revived.push(branch);
                    continue;
                }
                Ok(branch) => branch,
                Err(NewgitError::UnknownBranchInstance(_)) => continue,
                Err(error) => return Err(error),
            };
            let branch = &branch;
            let hooks = self.run_cleanup_hooks(branch, dry_run)?;
            let archived_record = if dry_run {
                None
            } else {
                let state_dir = self.store.instance_state_dir(&branch.slug);
                if state_dir.exists() {
                    std::fs::remove_dir_all(&state_dir)
                        .map_err(|source| NewgitError::io(state_dir, source))?;
                }
                Some(self.store.archive_branch_record(branch)?)
            };
            outcome.finalized.push(FinalizedInstance {
                name: branch.name.clone(),
                workspace: branch.workspace_path.clone(),
                hooks,
                archived_record,
            });
        }

        let live: Vec<&BranchInstance> = live.into_iter().chain(&revived).collect();

        // Unclaimed workspace directories: a failed spawn, or a record
        // archived while its directory survived.
        let workspace_root = self.config.workspace_root(&self.store.paths().project_root);
        let claimed = [branches.as_slice(), revived.as_slice()].concat();
        let (orphans, unrecognized) = orphan_workspaces(&workspace_root, &claimed)?;
        outcome.warnings.extend(unrecognized);
        for orphan in orphans {
            if !dry_run {
                std::fs::remove_dir_all(&orphan)
                    .map_err(|source| NewgitError::io(&orphan, source))?;
            }
            outcome.orphan_workspaces.push(orphan);
        }

        // Stale pid numbers: retired to `stopped` in place, not removed —
        // `status` reads a pid file's mere presence as "newgit started this
        // once," and deleting it here would erase that the moment gc ran.
        for branch in &live {
            outcome
                .retired_pids
                .extend(self.supervisor(branch).retire_dead_pids(dry_run)?);
        }
        // State directories belonging to no live instance at all: these are
        // actually removed, unlike the pid retirement above — there is no
        // instance left for a `status` to ask.
        let live_state_dirs: BTreeSet<Utf8PathBuf> = live
            .iter()
            .map(|branch| self.store.instance_state_dir(&branch.slug))
            .collect();
        for state_dir in self.store.state_dirs()? {
            if live_state_dirs.contains(&state_dir) {
                continue;
            }
            if !dry_run {
                std::fs::remove_dir_all(&state_dir)
                    .map_err(|source| NewgitError::io(&state_dir, source))?;
            }
            outcome.dead_state.push(state_dir);
        }

        let surviving: Vec<BranchInstance> = live.into_iter().cloned().collect();

        // Checkpoint logs of instances that no longer have a binding record —
        // including any this pass just finalized. Only when asked: these are
        // undo history, not garbage.
        if archived == ArchivedCheckpoints::Purge {
            let live_slugs: BTreeSet<&str> = surviving
                .iter()
                .map(|branch| branch.slug.as_str())
                .collect();
            for slug in self.store.checkpointed_slugs()? {
                if live_slugs.contains(slug.as_str()) {
                    continue;
                }
                if let Some(purged) = self.purge_checkpoints(&slug, dry_run)? {
                    outcome.purged_checkpoints.push(purged);
                }
            }
        }

        // Lane pruning. Roots come from the records that survive this pass,
        // so a dry run reports exactly what a real run would remove.
        let roots = SnapshotRoots::collect(&self.store, &surviving, archived)?;
        let lanes = self.store.paths().tracker_store.clone();
        for lane_rev in pinned_revs(&lanes)? {
            if roots.contains(&lane_rev.tracker, &lane_rev.rev) {
                continue;
            }
            if !dry_run {
                self.lane(&lane_rev.tracker).forget(&lane_rev.rev)?;
            }
            outcome.pruned.push(PrunedRev {
                tracker: lane_rev.tracker,
                rev: lane_rev.rev,
            });
        }
        if !dry_run && !outcome.pruned.is_empty() {
            prune_unreferenced(&lanes)?;
        }
        outcome.pinned_by_checkpoints = roots.pinned_only_by_checkpoints().count();
        outcome.pinned_by_archived = roots.pinned_only_by_archived_checkpoints().count();

        self.prune_install_store(&surviving, dry_run, &mut outcome)?;

        // Lock files of instances with no record any more. Bookkeeping, not
        // something a person would look for, so not reported; one held right
        // now is skipped, and one whose instance is spawned meanwhile is
        // simply made again (see `crate::lock`).
        if !dry_run {
            crate::lock::prune(&self.store.locks_dir(), |slug| {
                self.store.branch_record_path(slug).exists()
            })?;
        }

        Ok(outcome)
    }

    /// Drop install-store entries no surviving instance keys to.
    ///
    /// Reachability is recomputed rather than recorded: an entry is live if
    /// some instance's identity hashes to it *right now*. A recorded
    /// reference would be one more thing to keep in sync, and this is the
    /// same computation a spawn already does — so an entry is kept exactly
    /// when a spawn of that instance would have found it.
    ///
    /// The cost is real: one `key_command` per store-eligible resource per
    /// surviving instance. That is why it runs here, in the command whose
    /// whole job is to be thorough, and nowhere else.
    ///
    /// `--dry-run` will not run a `key_command`. A dry run is the thing
    /// people reach for precisely because it observes without acting, and
    /// spawning a user-supplied shell command is acting. Resources that need
    /// one are reported as unevaluated instead; resources keyed on file
    /// content alone are still reported exactly.
    fn prune_install_store(
        &self,
        surviving: &[BranchInstance],
        dry_run: bool,
        outcome: &mut CleanupOutcome,
    ) -> Result<()> {
        let store = self.installs();

        // A staging directory is a tree an interrupted `admit` was still
        // copying. Nothing will ever adopt it — the publish it belonged to is
        // gone — and at several gigabytes it is the single largest thing gc
        // can reclaim, so it is garbage unconditionally rather than by
        // reachability.
        for (resource, name, path) in store.staging_entries()? {
            if !dry_run {
                store.remove(&resource, &name)?;
            }
            outcome.pruned_installs.push(PrunedInstall {
                resource,
                key: format!("{name} (interrupted publish)"),
                path,
            });
        }

        let entries = store.entries()?;
        if entries.is_empty() {
            return Ok(());
        }

        // Reachability is per (resource, instance): a key that cannot be
        // computed for one resource says nothing about any other, so it
        // withholds that resource's entries and nothing else. Making one
        // unkeyable instance disable pruning for the whole project would turn
        // a cache into an unbounded disk leak with no way out.
        let mut reachable: BTreeSet<(String, String)> = BTreeSet::new();
        let mut unkeyed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for branch in surviving {
            for definition in &self.resources {
                if definition.identity_produces().is_empty() {
                    continue;
                }
                if dry_run && definition.identity_key_command().is_some() {
                    unkeyed
                        .entry(definition.name.clone())
                        .or_default()
                        .push(format!("`{}` (--dry-run runs no key_command)", branch.name));
                    continue;
                }
                match self.install_key(branch, definition) {
                    None => {}
                    Some(Ok(key)) => {
                        reachable.insert((definition.name.clone(), key));
                    }
                    Some(Err(report)) => unkeyed
                        .entry(definition.name.clone())
                        .or_default()
                        .push(format!("`{}`: {}", branch.name, report.summary())),
                }
            }
        }

        for (resource, reasons) in &unkeyed {
            outcome.warnings.push(format!(
                "install store entries for `{resource}` left alone — {}",
                reasons.join("; ")
            ));
        }

        for (resource, key) in entries {
            if unkeyed.contains_key(&resource)
                || reachable.contains(&(resource.clone(), key.clone()))
            {
                continue;
            }
            let path = store.root().join(&resource).join(&key);
            if !dry_run {
                store.remove(&resource, &key)?;
            }
            outcome.pruned_installs.push(PrunedInstall {
                resource,
                key,
                path,
            });
        }
        Ok(())
    }

    /// Write a branch instance's content out as an ordinary Git repository.
    ///
    /// Tracker audience is the default filter and it fails closed: only
    /// `public` lanes ship unless `--include` names a path. This is
    /// path-level filtering and nothing more — no hunk privacy, no
    /// concealment claim.
    pub fn export(
        &self,
        instance: &str,
        destination: &Utf8Path,
        filter: &ExportFilter,
    ) -> Result<ExportOutcome> {
        let branch = self.store.find_branch(instance)?;
        self.require_workspace(&branch)?;
        prepare_destination(destination)?;

        let workspace = branch.workspace_path.clone();
        let source_files = GitSource::workspace_tracked_files(&workspace)?;
        let plan = export::plan(&workspace, &source_files, &self.trackers, filter)?;

        if plan.files.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "nothing to export from `{}`: every candidate path was withheld by audience or \
                 excluded",
                branch.name
            )));
        }

        // Rendered source paths come from HEAD, not from disk. `--skip-worktree`
        // does not remove a path from `git ls-files`, and export otherwise
        // copies tracked files as they stand — which would ship this
        // instance's ports in a repo whose whole point is being clean.
        let rendered = self.rendered_source_paths(&branch);
        for file in &plan.files {
            let target = destination.join(&file.path);
            if rendered.contains(&file.path) {
                let committed =
                    GitSource::workspace_show_head(&workspace, &file.path)?.unwrap_or_default();
                if let Some(parent) = target.parent() {
                    crate::materializer::create_dir_all(parent)?;
                }
                std::fs::write(&target, committed)
                    .map_err(|source| NewgitError::io(target, source))?;
                continue;
            }
            copy_file(&workspace.join(&file.path), &target)?;
        }

        let head_rev = GitSource::workspace_head(&workspace)?;
        let commit = GitSource::init_export_repo(
            destination,
            &branch.source_ref,
            &format!(
                "Export of `{}` at {}",
                branch.name,
                &head_rev[..8.min(head_rev.len())]
            ),
        )?;
        let still_gitignored = GitSource::export_ignored_but_committed(destination)?;

        Ok(ExportOutcome {
            destination: destination.to_path_buf(),
            branch: branch.source_ref.clone(),
            instance: branch.name.clone(),
            source_head: head_rev,
            commit,
            plan,
            still_gitignored,
        })
    }

    /// Push a composed projection of the trackers' `main`s to export target
    /// `name` (`.newgit/exports/<name>.toml`), history and all.
    ///
    /// Fails closed: only `public` code trackers and the ones the target
    /// names ship, a secret tracker cannot be named, and a composed history
    /// that ever adds a path a secret tracker owns is refused before anything
    /// is pushed. Composing is deterministic, so exporting again
    /// fast-forwards the target; a different tracker set is a different
    /// history, which needs `force`.
    pub fn export_to_remote(
        &self,
        name: &str,
        options: RemoteExportOptions,
    ) -> Result<RemoteExportOutcome> {
        let targets = self.export_targets()?;
        let target = targets
            .iter()
            .find(|target| target.name == name)
            .cloned()
            .ok_or_else(|| {
                let known: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
                NewgitError::Unsupported(if known.is_empty() {
                    format!(
                        "no export target `{name}`: define one in .newgit/exports/{name}.toml \
                         (`newgit reference export`)"
                    )
                } else {
                    format!(
                        "no export target `{name}` (defined: {}); targets live in \
                         .newgit/exports/<name>.toml",
                        known.join(", ")
                    )
                })
            })?;

        self.prepare_projection()?;
        // Only to say what the export leaves behind, so its failing is said,
        // not mistaken for the export failing.
        let project_root_error = self
            .sync_project_root()
            .err()
            .map(|error| error.to_string());
        let client = match &self.remote {
            Some(link) => Some((link.connect(Acting::Login)?, link.project.clone())),
            None => None,
        };
        let list = self.branch_list(
            client
                .as_ref()
                .map(|(client, project)| (client.as_ref(), project.as_str())),
            PROJECT_ROOT_INSTANCE,
        )?;

        let (mut trackers, mut view) =
            self.compose_export(&target, &list, PROJECT_ROOT_INSTANCE)?;
        let pulled = if options.force {
            0
        } else {
            self.pull_export_target(&target, &view.tip, options.dry_run)?
        };
        if pulled > 0 && !options.dry_run {
            let list = self.branch_list(
                client
                    .as_ref()
                    .map(|(client, project)| (client.as_ref(), project.as_str())),
                PROJECT_ROOT_INSTANCE,
            )?;
            (trackers, view) = self.compose_export(&target, &list, PROJECT_ROOT_INSTANCE)?;
            // Composing is deterministic, so `main` recomposed is the target's
            // commits, plus whatever `main` had besides; anything else would
            // be refused by the push for a reason it could not name.
            let theirs = self
                .source
                .rev_parse(&export_ref(&target.name, "upstream"))?;
            if !self.source.is_ancestor(&theirs, &view.tip)? {
                return Err(NewgitError::Unsupported(format!(
                    "brought `{}` on `{}` into `main`, but `main` composed for it does not \
                     descend from {}, so it was not pushed. `newgit export --remote {} --dry-run` \
                     shows what would be",
                    target.branch,
                    target.name,
                    &theirs[..theirs.len().min(12)],
                    target.name
                )));
            }
        }
        for tracker in &mut trackers {
            tracker.unmerged = self
                .source
                .rev_parse(&local_tracker_ref(
                    &tracker.name,
                    &format!("instances/{PROJECT_ROOT_INSTANCE}"),
                ))
                .ok()
                .is_some_and(|ours| match &tracker.commit {
                    Some(main) => !self.source.is_ancestor(&ours, main).unwrap_or(true),
                    None => true,
                });
        }

        let url = target.push_url();
        if !options.dry_run {
            self.push_export(&target, &view.tip, &target.branch, options.force)
                .map_err(|error| match error {
                    NewgitError::SourceCommand { stderr, .. } if rejected(&stderr) => {
                        NewgitError::Unsupported(format!(
                            "`{}` holds commits the export does not descend from, so the push \
                             was refused. That is expected the first time, when the target has \
                             history of its own, or after the target's tracker set changed (a \
                             different set composes a different history). `--force` replaces \
                             the branch:\n{stderr}",
                            target.branch
                        ))
                    }
                    other => other,
                })?;
        }

        Ok(RemoteExportOutcome {
            target,
            url,
            trackers,
            tip: view.tip,
            commits: view.commits,
            pushed: !options.dry_run,
            project_root_error,
            pulled,
        })
    }

    /// When `target`'s branch is ahead of `tip`, the export about to be
    /// pushed — a pull request merged there — bring its commits into `main`
    /// first, as `git pull` then `newgit merge main` in the project root
    /// would: the project root's branch fast-forwards to them, and they
    /// merge. Returns how many commits that is (counted, not merged, on a
    /// dry run); zero when the target is not ahead.
    ///
    /// Only a fast-forward of the project root's own branch: when the
    /// target's commits are not ones it can move to — it has work of its
    /// own, or the target withholds a tracker, so its commits are not the
    /// project's — this refuses, saying how to bring them in by hand. A
    /// target that diverged from the export is left to the push to refuse.
    fn pull_export_target(&self, target: &ExportTarget, tip: &str, dry_run: bool) -> Result<u32> {
        let root = self.store.paths().project_root.clone();
        let upstream = export_ref(&target.name, "upstream");
        let fetched = crate::source::run_git_env(
            &[
                "-C",
                root.as_str(),
                "fetch",
                "--quiet",
                "--no-tags",
                &target.push_url(),
                &format!("+refs/heads/{}:{upstream}", target.branch),
            ],
            &[],
        );
        match fetched {
            Ok(_) => {}
            // Not exported yet: nothing there to bring in.
            Err(NewgitError::SourceCommand { stderr, .. })
                if stderr.contains("couldn't find remote ref") =>
            {
                return Ok(0);
            }
            Err(error) => return Err(error),
        }
        let theirs = self.source.rev_parse(&upstream)?;
        if theirs == tip
            || self.source.is_ancestor(&theirs, tip)?
            || !self.source.is_ancestor(tip, &theirs)?
        {
            return Ok(0);
        }
        let commits = self.source.commit_count(&format!("{tip}..{theirs}"))?;

        let branch = std::fs::read_to_string(self.store.paths().project_branch_file())
            .map(|branch| branch.trim().to_owned())
            .unwrap_or_else(|_| PROJECT_ROOT_INSTANCE.to_owned());
        let ours = self.source.rev_parse(&format!("refs/heads/{branch}"))?;
        if !self.source.is_ancestor(&ours, &theirs)? {
            return Err(NewgitError::Unsupported(format!(
                "`{}` on `{}` has {commits} commit(s) `main` does not (merged there?), and the \
                 project root's `{branch}` cannot fast-forward to them: it has commits of its \
                 own, or the target withholds a tracker, so its commits are not the project's. \
                 Bring them in by hand — `git pull {} {}` in the project root, then `newgit merge \
                 main` — or replace the branch with `--force`",
                target.branch,
                target.name,
                target.push_url(),
                target.branch
            )));
        }
        if dry_run {
            return Ok(commits);
        }

        let checked_out =
            crate::source::run_git_env(&["-C", root.as_str(), "symbolic-ref", "-q", "HEAD"], &[])
                .is_ok_and(|head| head.trim() == format!("refs/heads/{branch}"));
        if checked_out {
            crate::source::run_git_env(
                &["-C", root.as_str(), "merge", "--ff-only", "-q", &theirs],
                &[],
            )
            .map_err(|error| {
                NewgitError::Unsupported(format!(
                    "`{}` on `{}` has {commits} commit(s) `main` does not, but the project \
                     root's `{branch}` could not fast-forward to them (uncommitted changes in \
                     the way?): {error}",
                    target.branch, target.name
                ))
            })?;
        } else {
            self.source
                .update_ref(&format!("refs/heads/{branch}"), &theirs)?;
        }
        self.merge(PROJECT_ROOT_INSTANCE, "main")?;
        Ok(commits)
    }

    /// Every export target, each checked against this project's trackers:
    /// a target naming a secret tracker, or none at all, is an error.
    fn export_targets(&self) -> Result<Vec<ExportTarget>> {
        let targets = self.store.load_export_targets()?;
        let code = self.code_definitions();
        for target in &targets {
            for listed in &target.trackers {
                if code.iter().any(|definition| &definition.name == listed) {
                    continue;
                }
                let reason = if self
                    .trackers
                    .iter()
                    .any(|definition| &definition.name == listed)
                {
                    "is secret, and a secret tracker never leaves this machine through Git"
                } else {
                    "is not a tracker in this project"
                };
                return Err(NewgitError::InvalidDefinition {
                    kind: "export",
                    name: target.name.clone(),
                    reason: format!("`trackers` names `{listed}`, which {reason}"),
                });
            }
        }
        Ok(targets)
    }

    /// Compose `list` (tracker → commit) as `target` sees it — its trackers
    /// only — at `refs/newgit/exports/<target>/<branch>`, and refuse a
    /// history that ever adds a path a secret tracker owns: a path made
    /// secret later stays in the commits that once held it.
    fn compose_export(
        &self,
        target: &ExportTarget,
        list: &BTreeMap<String, String>,
        branch: &str,
    ) -> Result<(Vec<ExportedTracker>, ComposedExport)> {
        let code = self.code_definitions();
        let trackers: Vec<ExportedTracker> = code
            .iter()
            .map(|definition| ExportedTracker {
                name: definition.name.clone(),
                audience: definition.audience.clone(),
                reason: if definition.audience == "public" {
                    ExportReason::Public
                } else if target.trackers.contains(&definition.name) {
                    ExportReason::Listed
                } else {
                    ExportReason::Withheld
                },
                commit: list.get(&definition.name).cloned(),
                unmerged: false,
            })
            .collect();
        let tips: Vec<crate::compose::TrackerTip<'_>> = code
            .iter()
            .zip(&trackers)
            .filter(|(_, tracker)| tracker.reason != ExportReason::Withheld)
            .filter_map(|(definition, tracker)| {
                tracker
                    .commit
                    .clone()
                    .map(|commit| crate::compose::TrackerTip { definition, commit })
            })
            .collect();
        if tips.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "nothing to export to `{}`: no public code tracker has history on `{branch}`, \
                 and the target names none. Name the trackers to ship in \
                 .newgit/exports/{}.toml — `trackers = [\"root\"]` mirrors the project's own \
                 code",
                target.name, target.name
            )));
        }

        let root = self.store.paths().project_root.clone();
        let all = self.all_definitions();
        let view = crate::compose::compose(&root, &export_ref(&target.name, branch), &tips, &all)?;
        let tip = view.tip.ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "nothing to export to `{}`: its trackers' histories are empty",
                target.name
            ))
        })?;
        if let Some((commit, path, tracker)) =
            crate::compose::find_secret(&root, &[&tip], &[], &all)?
        {
            return Err(NewgitError::SplitRefused(format!(
                "the export to `{}` would carry commit {}, which adds `{path}`, and `{path}` \
                 belongs to the secret tracker `{tracker}`. Secrets never leave this machine \
                 through Git, so this history cannot be exported as it is",
                target.name,
                &commit[..commit.len().min(12)]
            )));
        }
        Ok((
            trackers,
            ComposedExport {
                tip,
                commits: view.commits,
            },
        ))
    }

    /// Push `tip` to `branch` on `target`, with plain `git push` and the
    /// user's own credentials. `--no-verify`: the project's own pre-push
    /// hooks are for its work, not for a mirror of it.
    fn push_export(
        &self,
        target: &ExportTarget,
        tip: &str,
        branch: &str,
        force: bool,
    ) -> Result<()> {
        let refspec = format!("{}{tip}:refs/heads/{branch}", if force { "+" } else { "" });
        crate::source::run_git_env(
            &[
                "-C",
                self.store.paths().project_root.as_str(),
                "push",
                "--quiet",
                "--no-verify",
                &target.push_url(),
                &refspec,
            ],
            &[],
        )
        .map(|_| ())
    }

    /// After a push from instance `branch` reached its remote: its branch,
    /// composed as each `branches = true` target sees it, pushed there too,
    /// and recorded in the workspace as `refs/remotes/<target>/<branch>` —
    /// the commit the target now really holds, so `gh` can tell whether the
    /// agent's `HEAD` is on it. A failure is reported per target; the push
    /// itself already succeeded.
    fn export_instance(
        &self,
        branch: &BranchInstance,
        head: &str,
        forced: bool,
    ) -> Vec<BranchExport> {
        let targets = match self.export_targets() {
            Ok(targets) => targets,
            Err(error) => {
                return vec![BranchExport {
                    target: String::new(),
                    url: String::new(),
                    branch: branch.source_ref.clone(),
                    result: Err(error.to_string()),
                }];
            }
        };
        let mut list = BTreeMap::new();
        for definition in self.code_definitions() {
            let commit = [format!("instances/{}", branch.slug), "main".to_owned()]
                .iter()
                .find_map(|name| {
                    self.source
                        .rev_parse(&local_tracker_ref(&definition.name, name))
                        .ok()
                });
            if let Some(commit) = commit {
                list.insert(definition.name.clone(), commit);
            }
        }
        targets
            .iter()
            .filter(|target| target.branches)
            .map(|target| {
                let result = (|| {
                    let (trackers, view) =
                        self.compose_export(target, &list, &format!("instances/{}", branch.slug))?;
                    self.push_export(target, &view.tip, &branch.source_ref, forced)?;
                    crate::source::run_git_env(
                        &[
                            "-C",
                            branch.workspace_path.as_str(),
                            "fetch",
                            "--quiet",
                            "--no-tags",
                            self.store.paths().project_root.as_str(),
                            &format!(
                                "+{}:refs/remotes/{}/{}",
                                export_ref(&target.name, &format!("instances/{}", branch.slug)),
                                target.name,
                                branch.source_ref
                            ),
                        ],
                        &[],
                    )?;
                    Ok::<_, NewgitError>(BranchExported {
                        same_commits: view.tip == head,
                        withheld: trackers
                            .into_iter()
                            .filter(|tracker| tracker.reason == ExportReason::Withheld)
                            .map(|tracker| tracker.name)
                            .collect(),
                    })
                })();
                BranchExport {
                    target: target.name.clone(),
                    url: target.push_url(),
                    branch: branch.source_ref.clone(),
                    result: result.map_err(|error| match error {
                        NewgitError::SourceCommand { stderr, .. } if rejected(&stderr) => format!(
                            "`{}` there has commits this branch does not descend from (someone \
                             pushed to it directly?), so it was left as it is",
                            branch.source_ref
                        ),
                        other => other.to_string(),
                    }),
                }
            })
            .collect()
    }

    /// Record one coherent snapshot across source, trackers, and resources.
    pub fn checkpoint(&self, instance: &str, message: Option<&str>) -> Result<CheckpointOutcome> {
        self.require_resolvable_graph()?;
        let (_lock, mut branch) = self.locked_branch(instance)?;
        self.checkpoint_branch(&mut branch, message, CheckpointReason::Explicit)
    }

    /// Where a `git push` from a workspace publishes: the store's own
    /// `origin`, the project's real remote. `None` when there is none.
    pub fn publish_url(&self) -> Result<Option<String>> {
        self.source.remote_url(PUBLISH_REMOTE, true)
    }

    /// The first half of a `git push` from a workspace, run before Git
    /// advertises anything to the pusher: bring the outbox up to date with
    /// the real remote, so the pushing Git decides fast-forward, and checks
    /// `--force-with-lease`, against what that remote has right now.
    /// Returns the outbox, for `git receive-pack` to serve.
    ///
    /// On a newgit remote there is nothing to mirror: a push publishes the
    /// instance's work into its trackers' forks, not one history somewhere.
    pub fn serve_push(&self, instance: &str) -> Result<Utf8PathBuf> {
        let branch = self.store.find_branch(instance)?;
        let outbox = self.outbox();
        self.source.ensure_outbox(&outbox)?;
        if branch.remote.is_none() || self.remote.is_none() {
            GitSource::sync_outbox(&outbox, &self.require_publish_url()?, None)?;
        }
        Ok(outbox)
    }

    /// The second half, run from the outbox's `pre-receive` hook while the
    /// push waits.
    ///
    /// Checkpoint first, so what reaches the real remote is always something
    /// `undo` can return to — `undo` cannot take back a push, so the push is
    /// the one moment a checkpoint has to exist. Then forward to the store's
    /// `origin`. Any error refuses the push, and the pusher sees why: a push
    /// succeeds exactly when it reached the real remote.
    ///
    /// `admitting` is the directory Git is holding the push's objects in
    /// until this returns; see [`GitSource::forward_push`].
    pub fn receive_push(
        &self,
        instance: &str,
        updates: &[RefUpdate],
        admitting: Option<&Utf8Path>,
    ) -> Result<PushOutcome> {
        self.require_resolvable_graph()?;
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let on_remote = self.remote.is_some() && branch.remote.is_some();
        let url = match &branch.remote {
            Some(binding) if on_remote => binding.endpoint.clone(),
            _ => self.require_publish_url()?,
        };

        let own_ref = format!("refs/heads/{}", branch.source_ref);
        if updates
            .iter()
            .any(|update| update.name == own_ref && update.new.is_none())
        {
            return Err(NewgitError::Unsupported(format!(
                "refusing to delete `{}`: it is the source branch instance `{}` is bound to. \
                 Remove the instance with `newgit remove {}`",
                branch.source_ref, branch.name, branch.name
            )));
        }

        // Every ref this push sends, whatever branch it is: the commits the
        // real remote (mirrored in the outbox) does not have yet. Checked
        // before the checkpoint, so a refused push leaves nothing behind.
        //
        // New work only: root's `main` — the project's history from before
        // newgit (merges will move it, in step 5) — is exempt. A `.env`
        // committed years ago is already in that history, and removing it
        // is a history rewrite (`git filter-repo`), not something to block
        // every first push to a fresh remote over.
        let outbox = self.outbox();
        let quarantine: Vec<(String, String)> = admitting
            .map(|dir| {
                vec![(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES".to_owned(),
                    dir.to_string(),
                )]
            })
            .unwrap_or_default();
        let mut rev_list: Vec<&str> = updates
            .iter()
            .filter_map(|update| update.new.as_deref())
            .collect();
        let history = self.source.rev_parse(ADOPTED_REF).ok();
        if !rev_list.is_empty() {
            rev_list.extend(["--not", "--all"]);
            rev_list.extend(history.as_deref());
            self.refuse_secrets(&outbox, &rev_list, &quarantine)?;
        }

        let mut checkpoint = self.checkpoint_branch(
            &mut branch,
            Some(&format!("before `git push` to {PUBLISH_REMOTE}")),
            CheckpointReason::Push,
        )?;
        // The work leaves the machine here, so it is split into its trackers
        // here (a no-op when the checkpoint's sync already did it). A refusal
        // (a path whose tracker is not bound) stops the push; any other
        // failure to split is reported and retried at the next push.
        match self.split_instance(&branch.slug, &checkpoint.record.source.head_rev) {
            Ok(_) => {}
            Err(refused @ NewgitError::SplitRefused(_)) => return Err(refused),
            Err(error) => checkpoint.warnings.push(format!(
                "this push's commits were not split into their trackers: {error}"
            )),
        }
        if on_remote {
            // On a newgit remote, the checkpoint's sync *is* the push: the
            // instance's work went to its trackers' forks. It succeeds only
            // if that reached the remote.
            if let Some(error) = &checkpoint.sync_error {
                return Err(NewgitError::Unsupported(format!(
                    "the push did not reach {url}: {error}"
                )));
            }
        } else {
            GitSource::forward_push(&self.outbox(), &url, None, updates, admitting)?;
        }

        // The instance's own branch goes on to each target that takes
        // branches. A force-push there follows a force-push here.
        let exports = match updates
            .iter()
            .find(|update| update.name == own_ref)
            .and_then(|update| {
                update
                    .new
                    .as_deref()
                    .map(|new| (update.old.as_deref(), new))
            }) {
            Some((old, new)) => {
                let forced =
                    old.is_some_and(|old| !self.source.is_ancestor(old, new).unwrap_or(true));
                self.export_instance(&branch, new, forced)
            }
            None => Vec::new(),
        };

        Ok(PushOutcome {
            checkpoint,
            remote: PUBLISH_REMOTE.to_owned(),
            url,
            exports,
        })
    }

    fn require_publish_url(&self) -> Result<String> {
        self.publish_url()?.ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "the store has no `{PUBLISH_REMOTE}` remote, so this push has nowhere to go: a \
                 push from a workspace publishes to the store's `{PUBLISH_REMOTE}`. Add one \
                 with `git -C {} remote add {PUBLISH_REMOTE} <url>`",
                self.store.paths().project_root
            ))
        })
    }

    pub fn list_checkpoints(&self, instance: &str) -> Result<Vec<CheckpointRecord>> {
        let branch = self.store.find_branch(instance)?;
        self.checkpoint_log(&branch).list()
    }

    /// Prove a restore actually works instead of trusting it: checkpoint the
    /// current state, restore it with a real `undo`, checkpoint again, and
    /// compare the two checkpoints' resource state refs. Agreement across
    /// every exercised resource is the difference between a checkpoint that
    /// looks like a backup and one that has actually been one.
    ///
    /// Destructive and expensive on purpose, which is why it is a separate,
    /// explicitly named entry point rather than something `checkpoint` does
    /// on its own: the middle step is a real `undo`, stopping and
    /// restarting whatever each resource runs, and if `[restore]` really is
    /// broken, this is where that gets discovered — on an instance someone
    /// chose to spend, not the one they actually needed to roll back.
    pub fn checkpoint_verify(
        &self,
        instance: &str,
        message: Option<&str>,
    ) -> Result<VerifyOutcome> {
        self.require_resolvable_graph()?;
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let before = self.checkpoint_branch(&mut branch, message, CheckpointReason::Explicit)?;

        let undo = self.undo(
            instance,
            &UndoOptions {
                to: Some(before.record.id.clone()),
                ..UndoOptions::default()
            },
        )?;

        // `undo` reloaded and saved its own copy of the branch record;
        // `branch` above is stale past this point.
        let mut branch = self.store.find_branch(instance)?;
        let after = self.checkpoint_branch(
            &mut branch,
            Some(&format!("verify: restored from {}", before.record.id)),
            CheckpointReason::Explicit,
        )?;

        let failed_restores = undo.failed_resources();
        let mut resources = Vec::new();
        for before_state in &before.record.resource_states {
            let after_ref = after
                .record
                .resource_states
                .iter()
                .find(|state| state.name == before_state.name)
                .and_then(|state| state.state_ref.clone());
            let agree = before_state.state_ref == after_ref;
            // `before` went ahead without it, so there is no state to compare
            // `after` against, and two empty refs agreeing is not a proof.
            let degraded = before.degraded.contains(&before_state.name);
            let mismatch = before_state.restore_exercisable && !degraded && !agree;
            let cause = mismatch.then(|| {
                if failed_restores.contains(&before_state.name.as_str()) {
                    MismatchCause::RestoreFailed
                } else {
                    self.control_checkpoint(&branch, &before_state.name, after_ref.as_deref())
                }
            });
            let diff = match &cause {
                None => None,
                Some(MismatchCause::CheckpointUnstable { control_state_ref }) => self.deposit_diff(
                    DiffPair::AfterControl,
                    after_ref.as_deref(),
                    control_state_ref.as_deref(),
                ),
                Some(_) => self.deposit_diff(
                    DiffPair::BeforeAfter,
                    before_state.state_ref.as_deref(),
                    after_ref.as_deref(),
                ),
            };
            resources.push(VerifyResource {
                name: before_state.name.clone(),
                exercised: before_state.restore_exercisable,
                degraded,
                before_state_ref: before_state.state_ref.clone(),
                after_state_ref: after_ref,
                agree,
                cause,
                diff,
            });
        }

        Ok(VerifyOutcome {
            before: before.record,
            undo,
            after: after.record,
            resources,
        })
    }

    /// Checkpoint one resource again, right after verify's `after`, and say
    /// whether it reproduced `after`. Only this resource, and only on a
    /// mismatch: a verify that agrees never pays for it, and a mismatch is
    /// already a failure worth one more `pg_dump` to explain. Not recorded as
    /// a checkpoint — it is a measurement, and a third record in the log
    /// would be one nobody asked to return to. A deposit it makes is left
    /// unreferenced for `cleanup` to reclaim.
    fn control_checkpoint(
        &self,
        branch: &BranchInstance,
        name: &str,
        after_ref: Option<&str>,
    ) -> MismatchCause {
        let control = self.resource_definition(name).and_then(|definition| {
            self.checkpoint_resource(branch, definition, "checkpoint-control")
        });
        match control {
            Ok(control) if control.state_ref.as_deref() == after_ref => {
                MismatchCause::RestoreLandedElsewhere
            }
            Ok(control) => MismatchCause::CheckpointUnstable {
                control_state_ref: control.state_ref,
            },
            Err(error) => MismatchCause::ControlFailed(error.to_string()),
        }
    }

    /// The line-level difference between two `tracker:<name>@<rev>` deposits,
    /// or `None` when either ref is not a deposit (an opaque command ref or a
    /// hash is all the content there is).
    fn deposit_diff(
        &self,
        pair: DiffPair,
        old_ref: Option<&str>,
        new_ref: Option<&str>,
    ) -> Option<VerifyDiff> {
        let (old_lane, old_rev) = parse_tracker_state_ref(old_ref?)?;
        let (new_lane, new_rev) = parse_tracker_state_ref(new_ref?)?;
        Some(VerifyDiff {
            pair,
            result: self
                .lane(old_lane)
                .checkout_dir(old_rev)
                .and_then(|before| {
                    let after = self.lane(new_lane).checkout_dir(new_rev)?;
                    diff_deposits(&before, &after)
                })
                .map_err(|error| error.to_string()),
        })
    }

    fn checkpoint_branch(
        &self,
        branch: &mut BranchInstance,
        message: Option<&str>,
        reason: CheckpointReason,
    ) -> Result<CheckpointOutcome> {
        debug_assert!(
            self.held_lock(&branch.slug).is_some(),
            "a checkpoint is taken under the instance's lock"
        );
        self.require_workspace(branch)?;
        // A checkpoint is the promise that this state can be returned to.
        // Hand edits to a rendered file are the one thing it cannot carry —
        // they are excluded from the capture by design — so this is exactly
        // the moment to name them, while they still exist.
        let mut warnings = self.render_drift(branch);
        let checkpoint_log = self.checkpoint_log(branch);
        let id = checkpoint_log.next_id()?;
        let workspace = branch.workspace_path.clone();

        // Source: the committed state plus a dangling commit for anything
        // uncommitted, fetched into the store. The store learns about
        // workspace commits only here — checkpoint is the blessing boundary,
        // and a checkpoint protects the worktree as it stands, not just what
        // the agent remembered to commit.
        // Re-sync the workspace's local-ignore rules first. `spawn` writes
        // them, but a definition that gains a tracker path or an `[identity]
        // produces` afterwards leaves every instance that already exists
        // without the rule — and the `git add -A` below is exactly where
        // that costs something, sweeping a whole install into source. Cheap
        // and idempotent, so the re-sync belongs on the path that depends
        // on it rather than only at spawn.
        self.ensure_workspace_excludes(branch)?;
        // Same reasoning for the push route: a workspace spawned before it
        // existed would otherwise push silently into the store forever.
        self.ensure_workspace_push(branch)?;

        let head_rev = GitSource::workspace_head(&workspace)?;
        let dirty_rev = GitSource::workspace_dirty_commit(
            &workspace,
            &format!("newgit {id}: uncommitted state of `{}`", branch.name),
            &self.rendered_source_paths(branch),
        )?;
        let tip = dirty_rev.clone().unwrap_or_else(|| head_rev.clone());
        let workspace_ref = format!("refs/newgit/checkpoints/{id}");
        let store_ref = format!("refs/newgit/checkpoints/{}/{id}", branch.slug);
        GitSource::workspace_update_ref(&workspace, &workspace_ref, &tip)?;
        let fetched = self
            .source
            .fetch_ref(&workspace, &workspace_ref, &store_ref);
        GitSource::workspace_delete_ref(&workspace, &workspace_ref)?;
        fetched?;
        self.bless_store_branch(branch, &head_rev, None, &mut warnings)?;

        // Resources, dependents first: a dependent's state may be derived
        // from its dependency, so it is captured before the dependency moves.
        // Lifecycle order, for the same reason cleanup uses it — a template
        // that reads an export is not a claim about capture order (#43).
        let mut resource_states = Vec::new();
        let mut degraded = Vec::new();
        let mut deposits: Vec<(String, String)> = Vec::new();
        for name in self.graph.lifecycle_order.iter().rev() {
            let Some(binding) = branch.resources.get(name) else {
                continue;
            };
            let definition = self.resource_definition(name)?;
            let was_running = self.supervisor(branch).running_pid(name).is_some();
            // A safety checkpoint minted ahead of an undo captures whatever
            // is there so redo has something to restore to. But a resource
            // that is already failed — its last prepare or restore did not
            // succeed — is known-broken, not merely unobserved: running its
            // checkpoint command (a `pg_dump`, say) against it spends real
            // time and I/O dumping rubble nobody is likely to ask to go back
            // to. Explicit checkpoints still run it: a person asked for that
            // one on purpose (#58).
            //
            // But a resource that is not up — its prepare or last restore
            // failed, or a dependency of it did — must not take the whole
            // checkpoint down with it when its capture fails: the checkpoint
            // goes ahead without it, degraded, and says so. An undo to it
            // leaves that resource as it finds it. A resource that *was* up
            // and fails to capture still aborts: that is unexpected.
            let captured = if reason == CheckpointReason::BeforeUndo
                && binding.status == ResourceStatus::Failed
            {
                warnings.push(format!(
                    "resource `{name}` is in a failed state (its last prepare or restore did not \
                     succeed); skipped its checkpoint capture rather than run an expensive \
                     command against known-broken state"
                ));
                degraded.push(name.clone());
                CapturedResource::none()
            } else if binding.status != ResourceStatus::Ready {
                match self.checkpoint_resource(branch, definition, "checkpoint") {
                    Ok(captured) => captured,
                    Err(error) => {
                        warnings.push(format!(
                            "resource `{name}` is not up and its capture failed ({error}), so \
                             this checkpoint is degraded — undoing to it leaves `{name}` as it is"
                        ));
                        degraded.push(name.clone());
                        CapturedResource::none()
                    }
                }
            } else {
                self.checkpoint_resource(branch, definition, "checkpoint")?
            };
            if let Some(deposit) = captured.deposit {
                deposits.push(deposit);
            }
            let restore_exercisable = matches!(
                definition.restore.as_ref().map(|spec| spec.mode),
                Some(RestoreMode::Command | RestoreMode::Recompute)
            );
            resource_states.push(ResourceState {
                name: name.clone(),
                definition_rev: definition.definition_rev.clone(),
                mode: captured.mode,
                state_ref: captured.state_ref,
                state_path: captured.state_path,
                was_running,
                resolved_ports: binding.resolved_ports.clone(),
                resolved_exports: binding.resolved_exports.clone(),
                restore_exercisable,
                restore_proven: binding.restore_proven,
            });
        }
        // Recorded in dependency order for readability.
        resource_states.reverse();
        for (tracker, rev) in deposits {
            let definition = self.definition(&tracker)?;
            branch.trackers.insert(
                tracker,
                TrackerBinding {
                    definition_rev: definition.definition_rev.clone(),
                    content_rev: Some(rev),
                },
            );
        }

        // Trackers: capture every owned path (dedupes in the lane).
        // Deposit-only lanes record whatever rev the deposit or an earlier
        // capture bound.
        let mut tracker_states = Vec::new();
        for definition in &self.trackers {
            // A dropped tracker's files are gone from the workspace on
            // purpose: its binding's content stands, uncaptured.
            let content_rev =
                if definition.paths.is_empty() || branch.dropped.contains(&definition.name) {
                    branch
                        .trackers
                        .get(&definition.name)
                        .and_then(|binding| binding.content_rev.clone())
                } else {
                    let bound = branch
                        .trackers
                        .get(&definition.name)
                        .and_then(|binding| binding.content_rev.clone());
                    // Renders reversed, as in `capture_tracker`: the lane is
                    // shared, and the rev recorded here is what an undo
                    // re-renders on top of, so it has to hold the committed
                    // values a render's `find` matches.
                    let reversed = self.unrender_for_capture(branch, definition)?;
                    let capture = self.lane(&definition.name).capture(
                        &workspace,
                        definition,
                        CaptureAt {
                            parent: bound.as_deref(),
                            branch: Branch::Instance(&branch.slug),
                            message: &format!("{} in `{}` at {id}", definition.name, branch.name),
                        },
                    );
                    reversed.restore()?;
                    Some(capture?.rev)
                };
            branch.trackers.insert(
                definition.name.clone(),
                TrackerBinding {
                    definition_rev: definition.definition_rev.clone(),
                    content_rev: content_rev.clone(),
                },
            );
            tracker_states.push(TrackerState {
                name: definition.name.clone(),
                definition_rev: definition.definition_rev.clone(),
                content_rev,
            });
        }

        let record = CheckpointRecord {
            id,
            branch: branch.name.clone(),
            created_at: Utc::now(),
            message: message.map(ToOwned::to_owned),
            reason,
            undo_completed: None,
            source: SourceState {
                head_rev,
                dirty_rev,
                store_ref,
            },
            tracker_states,
            resource_states,
            code: Default::default(),
            dropped: branch.dropped.clone(),
        };
        let record_path = checkpoint_log.save(&record)?;
        branch.updated_at = Utc::now();
        self.store.save_branch_record(branch)?;

        // The checkpoint exists locally now, whatever happens next; syncing
        // it is what lets another machine spawn it.
        let sync_error = match self.sync_checkpoint(branch, &record) {
            Ok(()) => None,
            Err(error) => {
                warnings.push(format!(
                    "checkpoint {} is saved here but not on the remote: {error}",
                    record.id
                ));
                Some(error.to_string())
            }
        };

        Ok(CheckpointOutcome {
            record,
            record_path,
            warnings,
            sync_error,
            degraded,
        })
    }

    /// Create this project on a remote as `<you>/<name>`: one repo per code
    /// tracker, each seeded with its history, and branch `main` naming each
    /// tracker's commit. Its first publication, so it publishes the project
    /// root's work as it stands — split into its trackers first — and each
    /// tracker's `main` here becomes what was pushed. After this, `main`
    /// moves only through `newgit merge`.
    pub fn create_on_remote(&self, client: &dyn Remote, name: &str) -> Result<PublishedProject> {
        // Here first: the project root merged into `main` — work already
        // merged locally stays, and the project root's commits join it.
        if self.source.rev_parse("HEAD").is_err() {
            return Err(NewgitError::Unsupported(
                "commit something first: a project reaches the remote as its history".to_owned(),
            ));
        }
        self.merge(PROJECT_ROOT_INSTANCE, "main")?;
        let root = self.store.paths().project_root.clone();
        let mut list = BTreeMap::new();
        for definition in self.code_definitions() {
            if let Ok(tip) = self
                .source
                .rev_parse(&local_tracker_ref(&definition.name, "main"))
            {
                list.insert(definition.name.clone(), tip);
            }
        }
        if !list.contains_key(crate::tracker::ACCESS_TRACKER) {
            return Err(NewgitError::Unsupported(
                "commit `.newgit/` first: the remote reads every tracker's audience from the \
                 `access` tracker (`.newgit/trackers/`), so it has to be in the history it \
                 publishes"
                    .to_owned(),
            ));
        }
        // Exactly what will be pushed: each tracker's `main`, which may hold
        // work merged here that the project root's branch never had.
        let commits: Vec<&str> = list.values().map(String::as_str).collect();
        self.refuse_unpublished(&commits)?;
        let trackers: Vec<String> = list.keys().cloned().collect();
        let (key, endpoint, seeds) = match client.create_project(name, &trackers) {
            Ok(created) => (created.key, created.endpoint, created.seeds),
            // Made before but never finished (a seed push or `main` failed):
            // pick up where it stopped. A project that has `main` is joined.
            Err(NewgitError::Remote {
                status: 409,
                message,
            }) => {
                let key = format!("{}/{name}", client.me()?.login);
                let view = client.project(&key)?;
                if view.default_list().is_some() {
                    return Err(NewgitError::Remote {
                        status: 409,
                        message,
                    });
                }
                let mut seeds = Vec::new();
                for tracker in &trackers {
                    seeds.push(client.create_tracker_repo(&key, tracker)?);
                }
                (key, view.endpoint, seeds)
            }
            Err(error) => return Err(error),
        };
        for seed in &seeds {
            let Some(commit) = list.get(&seed.tracker) else {
                continue;
            };
            GitSource::push_refs(
                &["-C", root.as_str()],
                seed.credential.remote.as_deref().unwrap_or_default(),
                &[format!("+{commit}:refs/heads/main")],
                Some(&basic_auth_header(&seed.credential)),
            )?;
        }
        client.create_branch(&key, "main", &list)?;
        Ok(PublishedProject {
            key,
            endpoint,
            list,
        })
    }

    /// Branch `branch`'s list — tracker → commit — with every commit in the
    /// project repo: on the remote, its list there (commits fetched from each
    /// tracker's repo); otherwise each code tracker's branch here.
    fn branch_list(
        &self,
        client: Option<(&dyn Remote, &str)>,
        branch: &str,
    ) -> Result<BTreeMap<String, String>> {
        let root = self.store.paths().project_root.clone();
        let Some((client, project)) = client else {
            self.fetch_code_trackers()?;
            return Ok(self
                .code_definitions()
                .iter()
                .filter_map(|definition| {
                    self.source
                        .rev_parse(&local_tracker_ref(&definition.name, branch))
                        .ok()
                        .map(|commit| (definition.name.clone(), commit))
                })
                .collect());
        };
        let view = client.project(project)?;
        let list = view
            .branches
            .iter()
            .find(|candidate| candidate.name == branch)
            .map(|candidate| candidate.list.clone())
            .ok_or_else(|| {
                NewgitError::Unsupported(format!("{project} has no branch `{branch}`"))
            })?;
        for (tracker, commit) in &list {
            if self
                .source
                .rev_parse(&format!("{commit}^{{commit}}"))
                .is_ok()
            {
                continue;
            }
            let repo = format!("{}.t.{tracker}", project.replace('/', "."));
            let credential = client.credential(project, &repo, Scope::Read)?;
            GitSource::fetch_refs(
                &["-C", root.as_str()],
                credential.remote.as_deref().unwrap_or_default(),
                &[format!("+refs/heads/*:refs/newgit/remote/{tracker}/*")],
                Some(&basic_auth_header(&credential)),
            )?;
        }
        Ok(list)
    }

    /// Merge an instance's committed work into branch `into` (docs/design/trackers.md,
    /// *Checkpoints, merges, and coherence*), tracker by tracker: a tracker the
    /// instance did not change stays; one `into` has not moved on since takes
    /// the instance's commit; one both changed gets a real Git merge in that
    /// tracker's own history. A conflict anywhere moves nothing. Then the
    /// branch's list moves in one step — on the remote by compare-and-swap, so
    /// a merge onto a list that moved meanwhile is refused, not mixed.
    ///
    /// What is committed in the workspace now is what merges: a checkpoint is
    /// taken first.
    ///
    /// `newgit merge main` merges the project root — the instance `main` — so
    /// its own commits reach the shared branch the same way.
    pub fn merge(&self, instance: &str, into: &str) -> Result<MergeOutcome> {
        self.require_resolvable_graph()?;
        // The instance's lock, held to the end; the project root has none.
        let mut _lock = None;
        let mut dropped = BTreeSet::new();
        let (name, checkpoint, head) = if instance == PROJECT_ROOT_INSTANCE {
            self.sync_project_root()?;
            let branch = std::fs::read_to_string(self.store.paths().project_branch_file())
                .map(|branch| branch.trim().to_owned())
                .unwrap_or_else(|_| PROJECT_ROOT_INSTANCE.to_owned());
            let head = self.source.rev_parse(&format!("refs/heads/{branch}"))?;
            (PROJECT_ROOT_INSTANCE.to_owned(), String::new(), head)
        } else {
            let (lock, mut branch) = self.locked_branch(instance)?;
            _lock = Some(lock);
            let checkpoint = self.checkpoint_branch(
                &mut branch,
                Some(&format!("before `newgit merge` into {into}")),
                CheckpointReason::Explicit,
            )?;
            if self.remote.is_some()
                && let Some(error) = &checkpoint.sync_error
            {
                return Err(NewgitError::Unsupported(format!(
                    "`{instance}`'s work did not reach the remote, so it cannot merge there: {error}"
                )));
            }
            let head = checkpoint.record.source.head_rev.clone();
            self.split_instance(&branch.slug, &head)?;
            // Only what the instance binds: a dropped tracker keeps the
            // branch's commit, whatever the instance's history says about it.
            dropped = branch.dropped;
            (branch.name, checkpoint.record.id, head)
        };
        if self.remote.is_some() {
            self.refuse_unpublished(&[&head])?;
        }
        let root = self.store.paths().project_root.clone();
        let code = self.code_definitions();
        let theirs: BTreeMap<String, String> = crate::compose::read_state(&root, &head)?
            .into_iter()
            .filter(|(tracker, _)| code.iter().any(|definition| &definition.name == tracker))
            .filter(|(tracker, _)| !dropped.contains(tracker))
            .collect();

        // What `into` is now: on the remote, its list (its commits brought
        // here); otherwise each tracker's branch here.
        let client = match &self.remote {
            Some(link) => Some((link.connect(Acting::Login)?, link.project.clone())),
            None => None,
        };
        let base = self.branch_list(
            client
                .as_ref()
                .map(|(client, project)| (client.as_ref(), project.as_str())),
            into,
        )?;

        let mut list = base.clone();
        let mut trackers = Vec::new();
        let mut conflicts = Vec::new();
        let names: BTreeSet<&String> = base.keys().chain(theirs.keys()).collect();
        for tracker in names {
            let (action, commit) = match (base.get(tracker), theirs.get(tracker)) {
                (Some(ours), Some(incoming))
                    if ours == incoming || self.source.is_ancestor(incoming, ours)? =>
                {
                    (TrackerMergeAction::Unchanged, ours.clone())
                }
                (Some(ours), Some(incoming)) if self.source.is_ancestor(ours, incoming)? => {
                    (TrackerMergeAction::FastForward, incoming.clone())
                }
                (Some(ours), Some(incoming)) => {
                    match self.merge_commits(
                        ours,
                        incoming,
                        &format!("Merge `{instance}` into {into}"),
                    )? {
                        Ok(merged) => (TrackerMergeAction::Merged, merged),
                        Err(files) => {
                            conflicts.push(format!("`{tracker}`: {}", files.join(", ")));
                            continue;
                        }
                    }
                }
                (None, Some(incoming)) => (TrackerMergeAction::New, incoming.clone()),
                (Some(ours), None) => (TrackerMergeAction::Unchanged, ours.clone()),
                (None, None) => continue,
            };
            list.insert(tracker.clone(), commit.clone());
            trackers.push(TrackerMerge {
                tracker: tracker.clone(),
                action,
                commit,
            });
        }
        if !conflicts.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "`{instance}` conflicts with `{into}`, so nothing moved — {}. Bring `{into}` into \
                 the instance's workspace, resolve there, and merge again",
                conflicts.join("; ")
            )));
        }

        let moved: Vec<&TrackerMerge> = trackers
            .iter()
            .filter(|merge| merge.action != TrackerMergeAction::Unchanged)
            .collect();
        if !moved.is_empty()
            && let Some((client, project)) = &client
        {
            let pushed: Vec<&str> = moved.iter().map(|merge| merge.commit.as_str()).collect();
            self.refuse_unpublished(&pushed)?;
            // Each tracker's branch moves only from the commit this merge
            // started from (or is it already, on a retry): never over
            // another merge that landed meanwhile.
            let branch_ref = format!("refs/heads/{into}");
            let push = |remote: &str,
                        auth: &crate::remote::RepoCredential,
                        tracker: &str,
                        commit: &str| {
                GitSource::push_leased(
                    &["-C", root.as_str()],
                    remote,
                    commit,
                    &branch_ref,
                    base.get(tracker).map(String::as_str),
                    Some(&basic_auth_header(auth)),
                )
            };
            for merge in moved
                .iter()
                .filter(|merge| merge.action == TrackerMergeAction::New)
            {
                let seed = client.create_tracker_repo(project, &merge.tracker)?;
                push(
                    seed.credential.remote.as_deref().unwrap_or_default(),
                    &seed.credential,
                    &merge.tracker,
                    &merge.commit,
                )?;
            }
            // A new tracker came in through the repo just seeded for it; the
            // merge moves the trackers the branch already has.
            let names: Vec<String> = moved
                .iter()
                .filter(|merge| merge.action != TrackerMergeAction::New)
                .map(|merge| merge.tracker.clone())
                .collect();
            if names.is_empty() {
                return Err(NewgitError::Unsupported(format!(
                    "a new code tracker joins `{into}` with the change to the definitions that \
                     made it: merge that too (`newgit merge main` from the project root)"
                )));
            }
            let credentials = client.begin_merge(project, into, &base, &names)?;
            for credential in &credentials {
                let Some(merge) = moved
                    .iter()
                    .find(|merge| merge.tracker == credential.tracker)
                else {
                    continue;
                };
                push(
                    credential.credential.remote.as_deref().unwrap_or_default(),
                    &credential.credential,
                    &merge.tracker,
                    &merge.commit,
                )?;
            }
            client.finish_merge(project, into, &base, &list)?;
        }
        for (tracker, commit) in &list {
            self.source
                .update_ref(&local_tracker_ref(tracker, into), commit)?;
            self.publish_code_refs(tracker, &[into])?;
        }
        // The merge is done; bringing the project root along is a courtesy,
        // and its failing is said, not mistaken for the merge failing.
        let project_root = if name != PROJECT_ROOT_INSTANCE && into == PROJECT_ROOT_INSTANCE {
            match self.catch_up_project_root(&list) {
                Ok(note) => note,
                Err(error) => Some(format!(
                    "the project root was not brought along to the merge: {error}"
                )),
            }
        } else {
            None
        };
        Ok(MergeOutcome {
            instance: name,
            into: into.to_owned(),
            checkpoint,
            project_root,
            trackers,
            remote: client.map(|(_, project)| project),
        })
    }

    /// After a merge into `main`, bring the project root along when it can be
    /// — a fast-forward to `main` composed — so the next spawn starts from
    /// the merged work, as `git pull` would. A project root with commits of
    /// its own is left alone. Says what happened.
    fn catch_up_project_root(&self, list: &BTreeMap<String, String>) -> Result<Option<String>> {
        let root = self.store.paths().project_root.clone();
        let code = self.code_definitions();
        let tips: Vec<crate::compose::TrackerTip<'_>> = code
            .iter()
            .filter_map(|definition| {
                list.get(&definition.name)
                    .map(|commit| crate::compose::TrackerTip {
                        definition,
                        commit: commit.clone(),
                    })
            })
            .collect();
        let Some(view) = crate::compose::compose(
            &root,
            "refs/newgit/views/branches/main",
            &tips,
            &self.all_definitions(),
        )?
        .tip
        else {
            return Ok(None);
        };
        let branch = std::fs::read_to_string(self.store.paths().project_branch_file())
            .map(|branch| branch.trim().to_owned())
            .unwrap_or_else(|_| PROJECT_ROOT_INSTANCE.to_owned());
        let Ok(tip) = self.source.rev_parse(&format!("refs/heads/{branch}")) else {
            return Ok(None);
        };
        if tip == view {
            return Ok(None);
        }
        if !self.source.is_ancestor(&tip, &view)? {
            return Ok(Some(format!(
                "the project root's own branch, `{branch}` (checked out at `newgit init`), has \
                 commits `main` does not, so it was left as it is: `git merge {}` there brings \
                 the merge in, and `newgit merge main` publishes those commits",
                &view[..view.len().min(12)]
            )));
        }
        let checked_out =
            crate::source::run_git_env(&["-C", root.as_str(), "symbolic-ref", "-q", "HEAD"], &[])
                .is_ok_and(|head| head.trim() == format!("refs/heads/{branch}"));
        if checked_out {
            if crate::source::run_git_env(
                &["-C", root.as_str(), "merge", "--ff-only", "-q", &view],
                &[],
            )
            .is_err()
            {
                return Ok(Some(format!(
                    "the project root could not fast-forward to the merge (uncommitted changes in \
                     the way?): `git merge --ff-only {}` there when ready",
                    &view[..view.len().min(12)]
                )));
            }
        } else {
            self.source
                .update_ref(&format!("refs/heads/{branch}"), &view)?;
        }
        Ok(Some(format!(
            "the project root's `{branch}` fast-forwarded to the merge"
        )))
    }

    /// A real merge of two commits of one tracker: the merge commit, or the
    /// files that conflict.
    fn merge_commits(
        &self,
        ours: &str,
        theirs: &str,
        message: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let root = self.store.paths().project_root.clone();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_str())
            .args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                ours,
                theirs,
            ])
            .output()
            .map_err(|source| NewgitError::io(root.clone(), source))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().to_owned();
        match output.status.code() {
            Some(0) => {}
            Some(1) => {
                return Ok(Err(lines
                    .take_while(|line| !line.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()));
            }
            _ => {
                return Err(NewgitError::SourceCommand {
                    command: format!("git merge-tree --write-tree {ours} {theirs}"),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
        }
        let commit = crate::source::run_git_env(
            &[
                "-C",
                root.as_str(),
                "commit-tree",
                &tree,
                "-p",
                ours,
                "-p",
                theirs,
                "-m",
                message,
            ],
            &[],
        )?;
        Ok(Ok(commit.trim().to_owned()))
    }

    /// Publish an instance's work for checkpoint `record`, tracker by tracker:
    /// its committed work (split already) and its uncommitted work (split
    /// here, off the branch) as one commit per code tracker. A tracker the
    /// instance changed — its commit is not the default branch's — is forked
    /// the first time and gets its branch (`refs/heads/<instance>`) and the
    /// checkpoint's commit pushed there. Returns the checkpoint as lists.
    fn publish_code(
        &self,
        remote: &dyn Remote,
        project: &str,
        branch: &BranchInstance,
        record: &CheckpointRecord,
    ) -> Result<crate::checkpoint::CodeState> {
        let root = self.store.paths().project_root.clone();
        let code = self.code_definitions();
        let code_only = |state: crate::compose::State| -> BTreeMap<String, String> {
            state
                .into_iter()
                .filter(|(tracker, _)| code.iter().any(|definition| &definition.name == tracker))
                .collect()
        };
        let head = code_only(crate::compose::read_state(&root, &record.source.head_rev)?);
        let tip = match &record.source.dirty_rev {
            None => head.clone(),
            Some(dirty) => {
                let bound: Vec<&TrackerDefinition> = code.iter().collect();
                let outcome = crate::compose::split(
                    &root,
                    &record.source.head_rev,
                    dirty,
                    &bound,
                    &self.all_definitions(),
                )
                .map_err(|error| match error {
                    crate::compose::SplitError::Refused(refusal) => {
                        NewgitError::SplitRefused(refusal.to_string())
                    }
                    crate::compose::SplitError::Git(error) => error,
                })?;
                code_only(outcome.state)
            }
        };

        let view = remote.project(project)?;
        let main = view.default_list().cloned().unwrap_or_default();
        for (tracker, tip_commit) in &tip {
            let Some(head_commit) = head.get(tracker) else {
                continue;
            };
            if main.get(tracker) == Some(head_commit) && head_commit == tip_commit {
                continue;
            }
            if !main.contains_key(tracker) {
                return Err(NewgitError::Unsupported(format!(
                    "`{tracker}` is not on {project}'s `{}` yet, so this instance has no fork of \
                     it to push to: a code tracker made after the project reached the remote \
                     joins it through `newgit merge`",
                    view.default_branch
                )));
            }
            let fork = remote.fork(project, &branch.slug, tracker)?;
            let credential = remote.credential(project, &fork.repo, Scope::Write)?;
            GitSource::push_refs(
                &["-C", root.as_str()],
                fork.remote.as_deref().unwrap_or_default(),
                &[
                    format!("+{head_commit}:refs/heads/{}", branch.slug),
                    format!("+{tip_commit}:{}", fork_checkpoint_ref(&record.id)),
                ],
                Some(&basic_auth_header(&credential)),
            )?;
        }
        Ok(crate::checkpoint::CodeState { head, tip })
    }

    /// Bring a checkpoint's code here and compose it: each code tracker this
    /// person may read is fetched — from the author's fork when the author
    /// changed it, from the tracker's own repo otherwise — and the lists
    /// composed into two views, the committed work and the checkpoint's tip.
    /// A tracker this person may not read is left out, and said so.
    fn import_code(
        &self,
        remote: &dyn Remote,
        project: &str,
        from_slug: &str,
        checkpoint_id: &str,
        code: &crate::checkpoint::CodeState,
        warnings: &mut Vec<String>,
    ) -> Result<ImportedCode> {
        if code.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "checkpoint {from_slug}/{checkpoint_id} carries no code trackers' commits, so it \
                 cannot be composed here; checkpoint it again with this newgit"
            )));
        }
        let root = self.store.paths().project_root.clone();
        let forks = remote.instance(project, from_slug)?.forks;
        let code_definitions = self.code_definitions();
        for tracker in code.tip.keys() {
            if !code_definitions
                .iter()
                .any(|definition| &definition.name == tracker)
            {
                warnings.push(format!(
                    "code tracker `{tracker}` is not defined here, so it is left out: bring this \
                     project's definitions up to date (`git pull` in the project root) and spawn \
                     again"
                ));
            }
        }
        let mut readable = Vec::new();
        for definition in &code_definitions {
            let tracker = &definition.name;
            if !code.tip.contains_key(tracker) {
                continue;
            }
            let forked = forks.contains(tracker);
            let repo = if forked {
                fork_repo(project, from_slug, tracker)
            } else {
                format!("{}.t.{tracker}", project.replace('/', "."))
            };
            let credential = match remote.credential(project, &repo, Scope::Read) {
                Ok(credential) => credential,
                Err(error) => {
                    warnings.push(format!("code tracker `{tracker}` is left out: {error}"));
                    continue;
                }
            };
            let url = credential.remote.as_deref().unwrap_or_default();
            let auth = basic_auth_header(&credential);
            GitSource::fetch_refs(
                &["-C", root.as_str()],
                url,
                &[format!(
                    "+refs/heads/*:refs/newgit/imported-trackers/{tracker}/heads/*"
                )],
                Some(&auth),
            )?;
            // A fork holds the checkpoint's own commit only when the tracker
            // had moved at that checkpoint; otherwise its heads have it.
            if forked {
                let _ = GitSource::fetch_refs(
                    &["-C", root.as_str()],
                    url,
                    &[format!(
                        "+{}:refs/newgit/imported-trackers/{tracker}/checkpoint",
                        fork_checkpoint_ref(checkpoint_id)
                    )],
                    Some(&auth),
                );
            }
            for commit in [code.head.get(tracker), code.tip.get(tracker)]
                .into_iter()
                .flatten()
            {
                if self
                    .source
                    .rev_parse(&format!("{commit}^{{commit}}"))
                    .is_err()
                {
                    return Err(NewgitError::Unsupported(format!(
                        "`{tracker}`'s commit {commit} for checkpoint {from_slug}/{checkpoint_id} \
                         is not on the remote"
                    )));
                }
            }
            readable.push(definition);
        }

        let all = self.all_definitions();
        let compose = |list: &BTreeMap<String, String>, view: &str| -> Result<String> {
            let tips: Vec<crate::compose::TrackerTip<'_>> = readable
                .iter()
                .filter_map(|definition| {
                    list.get(&definition.name)
                        .map(|commit| crate::compose::TrackerTip {
                            definition,
                            commit: commit.clone(),
                        })
                })
                .collect();
            crate::compose::compose(&root, view, &tips, &all)?
                .tip
                .ok_or_else(|| {
                    NewgitError::Unsupported(format!(
                        "checkpoint {from_slug}/{checkpoint_id} composes to nothing you may read"
                    ))
                })
        };
        let base = format!("refs/newgit/views/imported/{from_slug}/{checkpoint_id}");
        let head = compose(&code.head, &format!("{base}/head"))?;
        let tip = if code.tip == code.head {
            head.clone()
        } else {
            compose(&code.tip, &format!("{base}/tip"))?
        };
        let trackers = readable
            .iter()
            .filter_map(|definition| {
                code.head
                    .get(&definition.name)
                    .map(|commit| (definition.name.clone(), commit.clone()))
            })
            .collect();
        Ok(ImportedCode {
            head,
            tip,
            trackers,
        })
    }

    /// Put a checkpoint on the remote so any member can spawn it: its source
    /// commits to the instance's fork, each remote-stored tracker's rev to
    /// that tracker's repo, and then — last, so it never names a ref that
    /// is not there — the record itself.
    ///
    /// Registers the instance first if an earlier attempt failed. Runs on
    /// the workspace's own token, so it works where only that token exists.
    fn sync_checkpoint(
        &self,
        branch: &mut BranchInstance,
        record: &CheckpointRecord,
    ) -> Result<()> {
        let Some(link) = &self.remote else {
            return Ok(());
        };
        self.register_remote(branch)?;
        let Some(binding) = branch.remote.clone() else {
            return Ok(());
        };
        // Nothing is uploaded until everything it would carry is checked:
        // the checkpoint ref — the uncommitted-work commit included — and
        // every commit behind it, every parent of every merge, back to the
        // history newgit adopted (root's `main`). A secret committed after
        // that never reaches newgit's remote, even one already pushed
        // somewhere else by plain Git: it has to come out of the history.
        self.refuse_unpublished(&[&record.source.store_ref])?;
        self.split_instance(&branch.slug, &record.source.head_rev)?;

        let remote = link.connect(Acting::Workspace(&branch.workspace_path))?;
        let code = self.publish_code(remote.as_ref(), &binding.project, branch, record)?;
        let mut record = record.clone();
        record.code = code;

        let mut login = None;
        for state in &record.tracker_states {
            let Some(rev) = &state.content_rev else {
                continue;
            };
            let definition = self.definition(&state.name)?;
            if definition.storage != Storage::Remote {
                continue;
            }
            if login.is_none() {
                login = Some(remote.me()?.login);
            }
            let repo = tracker_repo(
                &binding.project,
                definition,
                login.as_deref().unwrap_or_default(),
            );
            let credential = remote.credential(&binding.project, &repo, Scope::Write)?;
            let url = credential.remote.clone().unwrap_or_default();
            let prefix = format!("refs/trackers/{}", state.name);
            let pin = format!("{prefix}/pins/{rev}");
            let instance = format!("{prefix}/instances/{}", branch.slug);
            // The pin keeps the checkpoint's commit; the instance's branch is
            // its history (forced: a checkout can move it back).
            let mut refspecs = vec![format!("{pin}:{pin}"), format!("+{instance}:{instance}")];
            // A per-user tracker's repo is the user's own, and its `main` is
            // what "your own content" means on another machine. A shared
            // tracker's `main` moves only by merge, never by a sync.
            if definition.audience == "user" && self.lane(&state.name).main().is_some() {
                let main = format!("{prefix}/main");
                refspecs.push(format!("+{main}:{main}"));
            }
            GitSource::push_refs(
                &[&format!("--git-dir={}", self.store.paths().tracker_store)],
                &url,
                &refspecs,
                Some(&basic_auth_header(&credential)),
            )?;
        }

        let value = serde_json::to_value(&record).map_err(|error| NewgitError::Remote {
            status: 0,
            message: format!("could not encode checkpoint {}: {error}", record.id),
        })?;
        remote.put_checkpoint(&binding.project, &branch.slug, &record.id, &value)?;
        remote.set_head(&binding.project, &branch.slug, &record.source.head_rev)
    }

    /// `log_name` labels the action log: `checkpoint` for a real checkpoint,
    /// `checkpoint-control` for verify's control run, so the log of the run
    /// that settled a MISMATCH can be told apart from the checkpoints it was
    /// compared against.
    fn checkpoint_resource(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        log_name: &str,
    ) -> Result<CapturedResource> {
        let Some(spec) = &definition.checkpoint else {
            return Ok(CapturedResource::none());
        };
        let binding = branch.resources.get(&definition.name);

        match spec.mode {
            CheckpointMode::None => Ok(CapturedResource::none()),
            CheckpointMode::Hash => {
                let files = collect_files(&branch.workspace_path, definition.identity_paths())?;
                let rev = content_rev(&files)?;
                Ok(CapturedResource {
                    mode: "hash".to_owned(),
                    state_ref: Some(format!("{HASH_STATE_REF_PREFIX}{rev}")),
                    state_path: None,
                    deposit: None,
                })
            }
            CheckpointMode::Command => {
                let template = spec.command.as_deref().expect("validated at parse time");

                // Checked here, not at manager open: erroring at open would
                // make `newgit tracker create <missing>` — the fix — fail too.
                if let Some(tracker) = &spec.into_tracker
                    && !self.trackers.iter().any(|t| &t.name == tracker)
                {
                    return Err(definition.invalid(format!(
                        "checkpoint `into_tracker = \"{tracker}\"` names a tracker that is \
                         not defined; create it with `newgit tracker create {tracker}`"
                    )));
                }

                // `into_tracker` commands write into a staging dir that is
                // deposited into the lane afterwards — the one seam between
                // resources and trackers.
                let staging = spec
                    .into_tracker
                    .as_ref()
                    .map(|_| {
                        tempfile::tempdir()
                            .map_err(|source| NewgitError::io(&branch.workspace_path, source))
                    })
                    .transpose()?;
                let staging_path = staging
                    .as_ref()
                    .map(|dir| {
                        Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
                            .map_err(|path| NewgitError::NonUtf8Path(path.display().to_string()))
                    })
                    .transpose()?;

                let bound = self.bound_vars(branch);
                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
                    snapshot_path: staging_path.as_deref().map(Utf8Path::as_str),
                    state_ref: None,
                    bound: Some(&bound),
                };
                let command = render(template, &context);
                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.{log_name}", definition.name));
                let env = self.assemble_env(branch)?;
                let cwd = resource_cwd(
                    &branch.workspace_path,
                    &definition.name,
                    "checkpoint",
                    definition.workdir_for(None),
                )?;
                let (code, stdout) = run_captured(&command, &cwd, &env, &log)?;
                if code != 0 {
                    return Err(NewgitError::CheckpointCommandFailed {
                        resource: definition.name.clone(),
                        code,
                        log,
                    });
                }

                match (&spec.into_tracker, &staging_path) {
                    (Some(tracker), Some(staging_path)) => {
                        let lane = self.lane(tracker);
                        let bound = branch
                            .trackers
                            .get(tracker)
                            .and_then(|binding| binding.content_rev.clone());
                        let deposit = lane.deposit(
                            staging_path,
                            CaptureAt {
                                parent: bound.as_deref(),
                                branch: Branch::Instance(&branch.slug),
                                message: &format!(
                                    "{} checkpoint of `{}`",
                                    definition.name, branch.name
                                ),
                            },
                        )?;
                        // A path the command echoed under {{snapshot.path}}
                        // maps to the same relative location inside the lane.
                        let checkout = lane.checkout_dir(&deposit.rev)?;
                        let state_path = Utf8Path::new(&stdout)
                            .strip_prefix(staging_path)
                            .map(|relative| checkout.join(relative))
                            .unwrap_or(checkout);
                        Ok(CapturedResource {
                            mode: "command".to_owned(),
                            state_ref: Some(format!("tracker:{tracker}@{}", deposit.rev)),
                            state_path: Some(state_path),
                            deposit: Some((tracker.clone(), deposit.rev)),
                        })
                    }
                    _ => Ok(CapturedResource {
                        mode: "command".to_owned(),
                        state_ref: (!stdout.is_empty()).then_some(stdout),
                        state_path: None,
                        deposit: None,
                    }),
                }
            }
            CheckpointMode::External => {
                let template = spec.state_ref.as_deref().expect("validated at parse time");
                let bound = self.bound_vars(branch);
                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
                    bound: Some(&bound),
                    ..RenderContext::default()
                };
                Ok(CapturedResource {
                    mode: "external".to_owned(),
                    state_ref: Some(render(template, &context)),
                    state_path: None,
                    deposit: None,
                })
            }
        }
    }

    /// Restore the branch instance to a checkpoint — the latest, unless
    /// `to` names one. The current state is checkpointed first, so undo is
    /// always undoable and running it twice is redo.
    pub fn undo(&self, instance: &str, options: &UndoOptions) -> Result<UndoOutcome> {
        self.require_resolvable_graph()?;
        let (_lock, mut branch) = self.locked_branch(instance)?;
        self.require_workspace(&branch)?;
        let checkpoint_log = self.checkpoint_log(&branch);
        let restored = match options.to.as_deref() {
            Some(id) => checkpoint_log.load(id)?,
            None => checkpoint_log.latest()?,
        };

        // `--only` names resources that must exist, or the undo silently does
        // nothing at all — the worst possible answer for a command someone
        // reaches for while already debugging.
        for name in &options.only {
            self.resource_definition(name)?;
        }
        let partial = !options.only.is_empty();
        let wanted = |name: &String| !partial || options.only.contains(name);
        let mut binding_moved = false;

        // The message describes *when* this checkpoint was taken, and a
        // reader takes that as a description of *what is in it*. Those come
        // apart precisely when the last thing that happened to this instance
        // was an undo that did not finish — the workspace it is about to
        // capture is whatever that failed restore left behind, not a state
        // anyone chose to be in. Say so, rather than let the timestamp imply
        // otherwise (#58).
        let last_operation_was_incomplete_undo = checkpoint_log
            .latest()
            .map(|record| {
                record.reason == CheckpointReason::BeforeUndo
                    && record.undo_completed == Some(false)
            })
            .unwrap_or(false);
        let mut safety_message = format!("state before undo to {}", restored.id);
        if last_operation_was_incomplete_undo {
            safety_message
                .push_str(" (captured after an incomplete undo; contents may be partial)");
        }

        let safety = self.checkpoint_branch(
            &mut branch,
            Some(&safety_message),
            CheckpointReason::BeforeUndo,
        )?;
        let mut warnings = safety.warnings.clone();

        // A checkpoint that went ahead without a resource that was not up —
        // degraded, see `checkpoint_branch` — records `mode = "none"` for it
        // and says an undo to it leaves that resource as it finds it. For a
        // resource whose restore reads what its checkpoint records, `none`
        // holds nothing to restore it from, whether it was left out or the
        // definition captured nothing yet when the checkpoint was taken.
        // Running that restore anyway can lose the database: it is handed a
        // literal `{{state_ref}}`, and the bundled templates drop or reset
        // the database before they read the dump. So the undo does not run
        // it, and the resource keeps its state and status. Any other restore
        // loses nothing by the ref's absence and runs as usual: a `recompute`
        // rebuilds from the inputs put back below.
        let left_alone: BTreeSet<&str> = restored
            .resource_states
            .iter()
            .filter(|state| {
                state.mode == "none"
                    && self
                        .resource_definition(&state.name)
                        .is_ok_and(ResourceDefinition::restore_reads_checkpoint)
            })
            .map(|state| state.name.as_str())
            .collect();

        // Nothing may keep running while content changes underneath it. A
        // partial undo leaves every other resource alone, so it must not stop
        // them either.
        let supervisor = self.supervisor(&branch);
        let mut stopped = BTreeSet::new();
        for definition in &self.resources {
            if wanted(&definition.name) && supervisor.running_pid(&definition.name).is_some() {
                supervisor.stop(&definition.name, &definition.stop_signal())?;
                stopped.insert(definition.name.as_str());
            }
        }

        // Source: content back exactly, uncommitted state uncommitted again.
        //
        // A partial undo does not touch it. `--only` exists for iterating on
        // one resource's restore command, where rewinding the working tree is
        // the cost being avoided — and a source rewind paired with a
        // restore of one resource is not a state this instance was ever in.
        let workspace = branch.workspace_path.clone();
        if !partial {
            // Fetched first: it only touches the object store, so a
            // checkpoint whose ref is gone fails here with nothing changed.
            GitSource::workspace_fetch_ref(
                &workspace,
                self.source.root(),
                &restored.source.store_ref,
            )?;

            // The binding before the source, as it was: what the checkpoint
            // did not bind is dropped again, and what it bound comes back —
            // its content with the source and trackers below, its resources
            // once the source is back. Nothing is lost: the safety checkpoint
            // above holds what was here.
            //
            // Before the source restore, not after: a dropped code tracker's
            // paths are skip-worktree entries, which the restore leaves alone
            // — so a file the checkpoint had deleted would stay in the index
            // and come back when the tracker is bound again (#210).
            let rebound = restored.dropped != branch.dropped;
            if rebound {
                binding_moved = true;
                for tracker in restored.dropped.difference(&branch.dropped) {
                    if let Some(definition) = self.trackers.iter().find(|d| &d.name == tracker) {
                        crate::tracker_repo::clear_owned_paths(&workspace, definition)?;
                    }
                    // A code tracker's paths go back to HEAD, as a discarding
                    // drop leaves them, so the sparse checkout below takes
                    // its files and their directories out of the tree.
                    if let Some(definition) = self.code.iter().find(|d| &d.name == tracker) {
                        let paths: Vec<&str> =
                            definition.paths.iter().map(|path| path.as_str()).collect();
                        let mut restore = vec!["-C", workspace.as_str(), "checkout", "HEAD", "--"];
                        restore.extend(&paths);
                        let _ = crate::source::run_git_env(&restore, &[]);
                        let mut clean = vec!["-C", workspace.as_str(), "clean", "-fdq", "--"];
                        clean.extend(&paths);
                        crate::source::run_git_env(&clean, &[])?;
                    }
                }
                branch.dropped = restored.dropped.clone();
                self.apply_sparse(&branch)?;
                let supervisor = self.supervisor(&branch);
                let gone: Vec<String> = branch
                    .resources
                    .keys()
                    .filter(|name| !self.resource_active(&branch, name))
                    .cloned()
                    .collect();
                for name in gone {
                    if supervisor.running_pid(&name).is_some() {
                        let _ = supervisor.stop(&name, "term");
                    }
                    branch.resources.remove(&name);
                }
                // The workspace now has the checkpoint's binding, so the
                // record says so too, before anything below can fail.
                self.store.save_branch_record(&branch)?;
            }

            GitSource::workspace_restore_to(
                &workspace,
                &restored.source.head_rev,
                restored.source.dirty_rev.as_deref(),
            )?;
            // The pre-undo tip stays reachable from the safety checkpoint's
            // ref, so moving the branch back to it is expected, not divergence.
            self.bless_store_branch(
                &mut branch,
                &restored.source.head_rev,
                Some(&safety.record.source.head_rev),
                &mut warnings,
            )?;

            // After the source: `prepare`, install keys and renders read
            // the restored content, not what was here before the undo.
            if rebound {
                self.bind_resources(&mut branch)?;
            }
        }

        // Trackers: plain content, restored exactly; should not partially
        // fail in interesting ways, so failures here are hard errors.
        let mut trackers = Vec::new();
        for state in restored
            .tracker_states
            .iter()
            .filter(|_| !partial)
            .filter(|state| !branch.dropped.contains(&state.name))
        {
            let Some(definition) = self
                .trackers
                .iter()
                .find(|definition| definition.name == state.name)
            else {
                warnings.push(format!(
                    "tracker `{}` from the checkpoint is no longer defined; its content was \
                     not restored",
                    state.name
                ));
                continue;
            };
            if definition.definition_rev != state.definition_rev {
                warnings.push(format!(
                    "tracker `{}` definition changed since the checkpoint; content was \
                     restored against the current definition",
                    state.name
                ));
            }
            let files = match &state.content_rev {
                Some(rev) => self
                    .lane(&definition.name)
                    .restore(&workspace, definition, rev)?,
                None => {
                    clear_owned_paths(&workspace, definition)?;
                    0
                }
            };
            branch.trackers.insert(
                state.name.clone(),
                TrackerBinding {
                    definition_rev: definition.definition_rev.clone(),
                    content_rev: state.content_rev.clone(),
                },
            );
            trackers.push(UndoTrackerOutcome {
                name: state.name.clone(),
                rev: state.content_rev.clone(),
                files,
            });
        }

        // Source and tracker content have both moved underneath the rendered
        // files, so the instance's values go back on top before any resource
        // is restored or restarted — a service must not come back up reading
        // the committed default port. Cheap, because a render is a pure
        // function of committed content and the binding record, and undo has
        // just settled both.
        if !partial {
            warnings.extend(self.rerender_all(&mut branch));
        }

        // Resources: dependencies before dependents, restarting what was
        // running. Failures are collected into a recovery record, not fatal.
        let mut resources = Vec::new();
        let mut failures: Vec<RestoreFailure> = Vec::new();
        for name in &self.graph.bind_order {
            let Some(state) = restored
                .resource_states
                .iter()
                .find(|state| &state.name == name)
            else {
                continue;
            };
            if !branch.resources.contains_key(name) || !wanted(name) {
                continue;
            }
            let definition = self.resource_definition(name)?;
            let was_left_alone = left_alone.contains(name.as_str());
            let (mut action_label, ok, exercised) = if was_left_alone {
                warnings.push(format!(
                    "checkpoint {} holds no state for resource `{name}`, so this undo left it as \
                     it is",
                    restored.id
                ));
                // Its process, though, was stopped above with everything
                // else, and comes back as it was found: below, after its
                // dependencies, so it starts on the config just re-rendered
                // and against their restored state. One this undo did not
                // stop, it did not touch, and does not report.
                if !stopped.contains(name.as_str()) {
                    continue;
                }
                ("not restored".to_owned(), true, false)
            } else {
                if definition.definition_rev != state.definition_rev {
                    warnings.push(format!(
                        "resource `{name}` definition changed since the checkpoint; restored \
                         with the current definition"
                    ));
                }

                let pre_undo = safety
                    .record
                    .resource_states
                    .iter()
                    .find(|pre| &pre.name == name)
                    .and_then(|pre| pre.state_ref.as_deref());
                self.restore_resource(
                    &mut branch,
                    definition,
                    state,
                    RestoreContext {
                        pre_undo_state_ref: pre_undo,
                        binding_moved,
                        force_recompute: options.force_recompute,
                    },
                    &mut failures,
                    &mut warnings,
                )?
            };
            if let Some(binding) = branch.resources.get_mut(name) {
                // Only a restore that ran something can have brought the
                // resource up. One that ran nothing — `none`, `external`, no
                // `[restore]`, a recompute skipped on unchanged identity —
                // left it as it found it, so it keeps the status it came in
                // with: a `prepare` that failed, or never ran behind a failed
                // dependency, has still not succeeded.
                if !ok {
                    binding.status = ResourceStatus::Failed;
                } else if exercised {
                    binding.status = ResourceStatus::Ready;
                }
                // Sticky once true: this answers "has restore ever
                // completed", not "would it complete right now". A skip (no
                // command ran) or a later failure must not erase a proof
                // that already happened.
                if ok && exercised {
                    binding.restore_proven = true;
                }
            }

            let mut restart_failed = false;
            if was_left_alone || (ok && state.was_running) {
                let when = if was_left_alone {
                    "before this undo"
                } else {
                    "at checkpoint time"
                };
                match self.restart_long_running(&branch, definition) {
                    Ok(true) => action_label.push_str(" + restarted"),
                    Ok(false) => warnings.push(format!(
                        "resource `{name}` was running {when} but has no long-running action to \
                         restart"
                    )),
                    Err(error) => {
                        restart_failed = true;
                        if let Some(binding) = branch.resources.get_mut(name) {
                            binding.status = ResourceStatus::Failed;
                        }
                        failures.push(RestoreFailure {
                            resource: name.clone(),
                            detail: format!("restart failed: {error}"),
                            log: None,
                            retry_with: format!("newgit action {name}.start {}", branch.name),
                        });
                    }
                }
            }
            // Read only now, after a failed restore or restart has marked the
            // resource failed, so the outcome says what the record is saved
            // with. Dependencies come first in bind order, so theirs are
            // already final too.
            resources.push(UndoResourceOutcome {
                status: branch.resources[name].status,
                blocked_by: self.blocked_dependencies(&branch, definition),
                name: name.clone(),
                action: action_label,
                ok,
                restart_failed,
            });
        }

        let recovery_record = if failures.is_empty() {
            None
        } else {
            Some(checkpoint_log.save_recovery(&RecoveryRecord {
                checkpoint: restored.id.clone(),
                branch: branch.name.clone(),
                created_at: Utc::now(),
                failures,
            })?)
        };

        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;

        // Now that the undo has finished, the safety checkpoint can say
        // whether it is a redo point. A pre-undo snapshot taken before an
        // undo that failed captures a state the instance never cleanly left,
        // and three failed attempts otherwise leave three of them looking
        // exactly like states a human chose to keep.
        let mut safety_record = safety.record;
        safety_record.undo_completed = Some(recovery_record.is_none());
        checkpoint_log.save(&safety_record)?;

        Ok(UndoOutcome {
            partial,
            restored,
            safety: safety_record,
            trackers,
            resources,
            recovery_record,
            warnings,
        })
    }

    /// `branch` is mutable because a recompute restore re-runs `prepare`,
    /// which may `capture` a fresh handle — a restored resource must not
    /// keep publishing the pre-undo one.
    ///
    /// Returns `(label, ok, exercised)`. `exercised` is true only when a
    /// command actually ran and could have failed — `none`/`external` never
    /// run anything, and a `recompute` skipped because identity is
    /// unchanged never touched the tree either, so neither proves the
    /// restore path works. `ok` without `exercised` is not proof.
    fn restore_resource(
        &self,
        branch: &mut BranchInstance,
        definition: &ResourceDefinition,
        state: &ResourceState,
        undo: RestoreContext<'_>,
        failures: &mut Vec<RestoreFailure>,
        warnings: &mut Vec<String>,
    ) -> Result<(String, bool, bool)> {
        let Some(spec) = &definition.restore else {
            return Ok(("none".to_owned(), true, false));
        };
        match spec.mode {
            RestoreMode::None => Ok(("none".to_owned(), true, false)),
            RestoreMode::External => Ok(("external (no-op)".to_owned(), true, false)),
            RestoreMode::Recompute => {
                let action_name = spec.recompute_action();
                let action = definition.actions.get(action_name).ok_or_else(|| {
                    NewgitError::UnknownAction {
                        resource: definition.name.clone(),
                        action: action_name.to_owned(),
                    }
                })?;

                // The whole premise of pointing `[identity]` at a lockfile is
                // that the same inputs rebuild the same tree. So when the
                // inputs have not moved between the checkpoint and now, the
                // tree already is what the checkpoint describes and the
                // rebuild is a no-op by construction — an expensive one,
                // since this is where a full dependency install lives.
                //
                // Both hashes are read from records rather than computed
                // here, and that is load-bearing: undo restores source before
                // resources, so hashing the workspace at this point would
                // compare the checkpoint against itself and skip every time,
                // including the case that matters — a lockfile that moved
                // after the checkpoint and has just been rewound under a
                // `node_modules` built from the newer one.
                //
                // The binding is an input too, and one the hash does not
                // record: the install store keys a tree on the trackers the
                // instance binds, because a shared lockfile lists every
                // member and the install builds the bound ones. An undo that
                // restored a different binding has changed what the tree is
                // built from, whether or not the lockfile moved, so the skip
                // compares the same inputs the store key does. Only for a
                // resource the store keys at all, one that declares
                // `[identity] produces`: a codegen step keyed on its inputs
                // alone skips on them alone, as it did.
                //
                // Identity describes the inputs, not the tree: someone can
                // delete half of `node_modules` without touching the lockfile,
                // and then the skip is wrong. That is what `--force-recompute`
                // is for, and why the skip says so on the line rather than
                // passing silently.
                // And only while the tree is there at all: a lockfile that did
                // not move says nothing about a `node_modules` someone deleted.
                let key_moved = undo.binding_moved && !definition.identity_produces().is_empty();
                if !undo.force_recompute
                    && !key_moved
                    && self.produced_present(branch, definition)
                    && let Some(recorded) = state.state_ref.as_deref()
                    && recorded.starts_with(HASH_STATE_REF_PREFIX)
                    && undo
                        .pre_undo_state_ref
                        .is_some_and(|current| current == recorded)
                {
                    return Ok((
                        format!("recompute({action_name}) skipped: identity unchanged"),
                        true,
                        false,
                    ));
                }

                // Past the skip, the rebuild is really needed — source has
                // been restored, so the workspace holds the checkpoint's
                // inputs and anything the store has under that key is the
                // tree this rebuild would produce. Cloning it is the same
                // trade as at spawn, one step further along: the rebuild
                // here is the expensive one, a full reinstall in the middle
                // of an undo somebody is waiting on.
                let install_key = match self.install_key(branch, definition) {
                    Some(Ok(key)) => Some(key),
                    Some(Err(report)) => {
                        warnings.push(format!(
                            "resource `{}`: {}; rebuilding instead",
                            definition.name,
                            report.summary()
                        ));
                        None
                    }
                    None => None,
                };
                if undo.force_recompute {
                    if let Some(key) = &install_key {
                        self.forget_install_entry(definition, key);
                    }
                } else if let Some(report) = install_key
                    .as_deref()
                    .and_then(|key| self.refill_from_install_store(branch, definition, key))
                {
                    if let InstallReport::Filled { key, method } = &report {
                        // Phrased like the `skipped:` case above, because it
                        // is the same fact: the rebuild did not run, and here
                        // is why it did not need to.
                        return Ok((
                            format!(
                                "recompute({action_name}) filled from install store {key} ({})",
                                method.label()
                            ),
                            true,
                            true,
                        ));
                    }
                    if let InstallReport::Unavailable { reason } = &report {
                        warnings.push(format!(
                            "resource `{}`: {reason}; rebuilding instead",
                            definition.name
                        ));
                    }
                }

                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.restore", definition.name));
                let (code, captured) =
                    self.run_one_shot(branch, definition, action, action_name, &log, true)?;
                warnings.extend(Self::missing_capture_warnings(
                    &definition.name,
                    action_name,
                    &captured,
                    &log,
                ));
                Self::apply_captures(branch, &definition.name, captured.found);
                let ok = code == 0;
                if !ok {
                    failures.push(RestoreFailure {
                        resource: definition.name.clone(),
                        detail: format!("recompute action `{action_name}` exited with {code}"),
                        log: Some(log),
                        retry_with: format!(
                            "newgit action {}.{action_name} {}",
                            definition.name, branch.name
                        ),
                    });
                }
                // A rebuild during an undo produces the same tree a spawn
                // would have, so it is worth the same to the next instance.
                let mut label = format!("recompute({action_name})");
                if ok && let Some(key) = &install_key {
                    match self.admit_to_install_store(branch, definition, key) {
                        Some(report @ InstallReport::Stored { .. }) => {
                            label.push_str(&format!("; {}", report.summary()));
                        }
                        Some(InstallReport::Unavailable { .. }) | None => {}
                        Some(report) => warnings.push(format!(
                            "resource `{}`: {}",
                            definition.name,
                            report.summary()
                        )),
                    }
                }
                Ok((label, ok, true))
            }
            RestoreMode::Command => {
                let template = spec.command.as_deref().expect("validated at parse time");
                let binding = branch.resources.get(&definition.name);
                let state_ref = self.consumable_state_ref(state)?;
                // A record can hold no state ref for a resource it did
                // capture: a `command` checkpoint that printed nothing, or
                // one taken while the definition checkpointed a `hash`.
                // Rendered anyway, the literal `{{state_ref}}` would show
                // only once the command had run, and the bundled restores
                // drop or reset the database before they read the dump. So
                // it is refused, like a cleanup hook in the same state, and
                // the resource counts as not restored.
                if state_ref.is_none() && template.contains(STATE_REF_PLACEHOLDER) {
                    warnings.push(format!(
                        "resource `{}`: the checkpoint recorded no state ref for it, so its \
                         restore command, which uses {STATE_REF_PLACEHOLDER}, was not run",
                        definition.name
                    ));
                    failures.push(RestoreFailure {
                        resource: definition.name.clone(),
                        detail: format!(
                            "restore command not run: {STATE_REF_PLACEHOLDER} has no value in \
                             this checkpoint"
                        ),
                        log: None,
                        retry_with: "undo to a checkpoint that recorded a state ref for it"
                            .to_owned(),
                    });
                    return Ok(("command not run: no state ref".to_owned(), false, false));
                }
                let bound = self.bound_vars(branch);
                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
                    snapshot_path: None,
                    state_ref: state_ref.as_deref(),
                    bound: Some(&bound),
                };
                let command = render(template, &context);
                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.restore", definition.name));
                let env = self.assemble_env(branch)?;
                let cwd = resource_cwd(
                    &branch.workspace_path,
                    &definition.name,
                    "restore",
                    definition.workdir_for(None),
                )?;
                let (code, _) = run_captured(&command, &cwd, &env, &log)?;
                let ok = code == 0;
                if !ok {
                    failures.push(RestoreFailure {
                        resource: definition.name.clone(),
                        detail: format!("restore command exited with {code}"),
                        log: Some(log),
                        retry_with: "repair the resource, then `newgit undo` again".to_owned(),
                    });
                }
                Ok(("command".to_owned(), ok, true))
            }
        }
    }

    /// Start the definition's long-running action again after an undo.
    /// `Ok(false)` when the definition has none.
    ///
    /// Invoked exactly as `newgit action <resource>.<action>` would invoke
    /// it, `workdir` included: the same process must not come back in a
    /// different directory depending on which path started it (#109).
    fn restart_long_running(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<bool> {
        let Some((action_name, action)) = long_running_action(definition) else {
            return Ok(false);
        };
        self.start_supervised(branch, definition, action, action_name)?;
        Ok(true)
    }

    /// Start one long-running action under supervision, invoked exactly as
    /// `newgit action <resource>.<action>` invokes it.
    fn start_supervised(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
        action_name: &str,
    ) -> Result<u32> {
        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));
        let (command, cwd) = self.action_invocation(branch, definition, action, action_name)?;
        let env = self.assemble_env(branch)?;
        self.supervisor(branch)
            .start(&definition.name, &command, &cwd, &env, &log)
    }

    /// Start each bound resource's `autostart` action, dependencies first,
    /// as `newgit action <resource>.<action>` would. One already running is
    /// left alone and not reported. One that is not ready, is blocked, or
    /// depends on a resource whose start failed here is not started, and is
    /// reported failed with the reason: it did not come up.
    ///
    /// Starting is not waiting: a dependent is started once its dependency's
    /// process exists, not once it answers (#54).
    ///
    /// `only`, when given, limits it to those resources.
    fn autostart(
        &self,
        branch: &BranchInstance,
        only: Option<&BTreeSet<&str>>,
    ) -> Vec<StopStartResource> {
        let supervisor = self.supervisor(branch);
        let mut failed_here = BTreeSet::new();
        let mut started = Vec::new();
        for definition in self.lifecycle_definitions() {
            let name = &definition.name;
            if only.is_some_and(|only| !only.contains(name.as_str())) {
                continue;
            }
            let Some((action_name, action)) = definition.autostart_action() else {
                continue;
            };
            let Some(binding) = branch.resources.get(name) else {
                continue;
            };
            if supervisor.running_pid(name).is_some() {
                continue;
            }
            let down: Vec<String> = definition
                .depends_on
                .iter()
                .filter(|dependency| failed_here.contains(*dependency))
                .map(|dependency| format!("`{dependency}`"))
                .collect();
            // The reason nearest the cause first: a dependency that did not
            // start here, then one that is down, then this resource's own
            // `prepare`.
            let result = if !down.is_empty() {
                Err(format!(
                    "resource `{name}` depends on {}, which did not start",
                    down.join(", ")
                ))
            } else if let Err(error) = self.refuse_if_blocked(branch, definition) {
                Err(error.to_string())
            } else if binding.status != ResourceStatus::Ready {
                let why = match binding.status {
                    ResourceStatus::Failed => "failed",
                    _ => "is not ready",
                };
                Err(format!(
                    "resource `{name}` {why}, so `{action_name}` was not started"
                ))
            } else {
                self.start_supervised(branch, definition, action, action_name)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            };
            let (status, error) = match result {
                Ok(()) => (StopStartStatus::Started, None),
                Err(error) => {
                    failed_here.insert(name.clone());
                    (StopStartStatus::Failed, Some(error))
                }
            };
            started.push(StopStartResource {
                name: name.clone(),
                action: action_name.to_owned(),
                status,
                error,
            });
        }
        started
    }

    /// Every defined resource in lifecycle order: dependencies before
    /// dependents over `depends_on`. A resource the order leaves out — a
    /// graph with problems skips what it cannot place — follows, in
    /// declaration order, so nothing is missed.
    fn lifecycle_definitions(&self) -> Vec<&ResourceDefinition> {
        let mut ordered: Vec<&ResourceDefinition> = self
            .graph
            .lifecycle_order
            .iter()
            .filter_map(|name| {
                self.resources
                    .iter()
                    .find(|definition| &definition.name == name)
            })
            .collect();
        for definition in &self.resources {
            if !ordered.iter().any(|placed| placed.name == definition.name) {
                ordered.push(definition);
            }
        }
        ordered
    }

    /// The resources with a supervised process alive right now, dependents
    /// first: the order to stop them in. `remove` and `stop` share it.
    fn running_resources(&self, branch: &BranchInstance) -> Vec<&ResourceDefinition> {
        let supervisor = self.supervisor(branch);
        let mut running: Vec<&ResourceDefinition> = self
            .lifecycle_definitions()
            .into_iter()
            .filter(|definition| supervisor.running_pid(&definition.name).is_some())
            .collect();
        running.reverse();
        running
    }

    fn stopped_record_path(&self, branch: &BranchInstance) -> Utf8PathBuf {
        self.store
            .instance_state_dir(&branch.slug)
            .join("stopped.toml")
    }

    fn load_stopped_record(&self, branch: &BranchInstance) -> Result<StoppedRecord> {
        let path = self.stopped_record_path(branch);
        if !path.is_file() {
            return Ok(StoppedRecord::default());
        }
        crate::store::read_toml_at(&path)
    }

    /// Write the record, or delete it once nothing is left in it.
    fn save_stopped_record(&self, branch: &BranchInstance, record: &StoppedRecord) -> Result<()> {
        let path = self.stopped_record_path(branch);
        if record.resources.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(NewgitError::io(path, error))
                }
                _ => Ok(()),
            };
        }
        crate::materializer::create_dir_all(&self.store.instance_state_dir(&branch.slug))?;
        crate::store::write_toml_at(&path, "stopped resources", record)
    }

    /// `newgit stop`: take the instance's resources down, dependents first,
    /// and add what it took down to the instance's stopped record for
    /// [`Self::start_instance`]. Each running long-running action is
    /// signalled, and each resource defining `suspend` runs it. The
    /// workspace, trackers, port claims, and installs are left as they are.
    ///
    /// Every resource is attempted even when one fails. One whose process
    /// ignored the signal is reported failed but still recorded: it was
    /// running, so `start` should bring it back once it is gone. A second
    /// `stop` adds to the record rather than replacing it, so starting
    /// something by hand between two stops does not make `start` forget what
    /// the first one stopped.
    ///
    /// `suspend` runs only for a resource that is `ready` and that
    /// per-instance teardown may touch: a `project` or `user` resource is
    /// shared with other instances, and stopping this one must not take it
    /// down under them. A failed `suspend` is not recorded; the resource is
    /// taken to be still up, and the next `stop` tries again.
    pub fn stop_instance(&self, instance: &str) -> Result<StopStartOutcome> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let supervisor = self.supervisor(&branch);
        let mut record = self.load_stopped_record(&branch)?;
        let mut resources = Vec::new();
        let mut captured = false;
        let mut teardown = self.lifecycle_definitions();
        teardown.reverse();
        for definition in teardown {
            let name = &definition.name;
            // The process first: it is the part of a resource that talks to
            // what `suspend` takes down.
            if supervisor.running_pid(name).is_some() {
                // The action `start` will run again. A process whose
                // definition no longer has one is still stopped; there is
                // just nothing to record for it.
                let restartable = long_running_action(definition).map(|(name, _)| name.to_owned());
                let action = restartable.clone().unwrap_or_else(|| "stop".to_owned());
                let signal = definition.stop_signal();
                let stopped = match supervisor.stop(name, &signal) {
                    Ok(StopOutcome::Stopped(_)) => Some((StopStartStatus::Stopped, None)),
                    // Exited on its own between the check and the signal:
                    // not something this stop did, and not running to bring
                    // back.
                    Ok(StopOutcome::NotRunning) => None,
                    Ok(StopOutcome::StillRunning(pid)) => Some((
                        StopStartStatus::Failed,
                        Some(format!(
                            "pid {pid} ignored `{signal}`; escalate with `kill -9 -- -{pid}` if \
                             needed"
                        )),
                    )),
                    Err(error) => Some((StopStartStatus::Failed, Some(error.to_string()))),
                };
                if let Some((status, error)) = stopped {
                    if let Some(action) = restartable {
                        record
                            .resources
                            .retain(|entry| &entry.name != name || entry.is_resume());
                        record.resources.push(StoppedResource {
                            name: name.clone(),
                            action,
                        });
                    }
                    resources.push(StopStartResource {
                        name: name.clone(),
                        action,
                        status,
                        error,
                    });
                }
            }

            let suspendable = definition.actions.contains_key("suspend")
                && may_tear_down(definition.ownership)
                && !record
                    .resources
                    .iter()
                    .any(|entry| &entry.name == name && entry.is_resume())
                && branch
                    .resources
                    .get(name)
                    .is_some_and(|binding| binding.status == ResourceStatus::Ready);
            if suspendable {
                let result = self.run_lifecycle_action(&mut branch, definition, "suspend");
                captured |= result.as_ref().is_ok_and(|&(_, _, changed)| changed);
                let (status, error) = match result {
                    Ok((0, _, _)) => {
                        record.resources.push(StoppedResource {
                            name: name.clone(),
                            action: "resume".to_owned(),
                        });
                        (StopStartStatus::Stopped, None)
                    }
                    Ok((code, log, _)) => (
                        StopStartStatus::Failed,
                        Some(format!("`suspend` exited {code}; log: {log}")),
                    ),
                    Err(error) => (StopStartStatus::Failed, Some(error.to_string())),
                };
                resources.push(StopStartResource {
                    name: name.clone(),
                    action: "suspend".to_owned(),
                    status,
                    error,
                });
            }
        }
        self.save_stopped_record(&branch, &record)?;
        if captured {
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
        }
        Ok(StopStartOutcome {
            instance: branch.name,
            resources,
        })
    }

    /// `newgit start`: bring back what the instance's stopped record names,
    /// dependencies first, and drop each from the record once it is up. A
    /// resource nobody had started stays stopped. A resource's `resume` runs
    /// before its long-running action starts again.
    ///
    /// One that fails stays in the record, so the next `start` tries it
    /// again, and what depends on it is not started against it: it fails
    /// too, naming the dependency, and stays owed in the same way. A process
    /// already running — started by hand since — or no longer defined is
    /// dropped without being reported: there is nothing to do for it.
    pub fn start_instance(&self, instance: &str) -> Result<StopStartOutcome> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        let mut record = self.load_stopped_record(&branch)?;
        if record.resources.is_empty() {
            return Ok(StopStartOutcome {
                instance: branch.name,
                resources: Vec::new(),
            });
        }
        self.require_resolvable_graph()?;
        self.require_workspace(&branch)?;
        let supervisor = self.supervisor(&branch);

        let mut resources = Vec::new();
        let mut still_stopped = Vec::new();
        let mut failed_here = BTreeSet::new();
        let mut captured = false;
        for definition in self.lifecycle_definitions() {
            let name = &definition.name;
            let mut owed: Vec<&StoppedResource> = record
                .resources
                .iter()
                .filter(|entry| &entry.name == name)
                .collect();
            owed.sort_by_key(|entry| !entry.is_resume());
            for entry in owed {
                // What `refuse_if_blocked` cannot see: a dependency that did
                // not come back in this start is still recorded `ready`.
                let down: Vec<&str> = definition
                    .depends_on
                    .iter()
                    .filter(|dependency| failed_here.contains(*dependency))
                    .map(String::as_str)
                    .collect();
                let blocked = || -> Result<()> {
                    // Its own `resume` ran first and failed: its process
                    // would come up against the half it talks to.
                    if failed_here.contains(name) {
                        return Err(NewgitError::Unsupported(format!(
                            "resource `{name}` did not resume, so its process is not started"
                        )));
                    }
                    if down.is_empty() {
                        return self.refuse_if_blocked(&branch, definition);
                    }
                    Err(NewgitError::Unsupported(format!(
                        "resource `{name}` depends on {}, which did not come back",
                        down.iter()
                            .map(|dependency| format!("`{dependency}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                };

                let (action, result) = if entry.is_resume() {
                    let result = blocked()
                        .and_then(|()| self.run_lifecycle_action(&mut branch, definition, "resume"))
                        .map_err(|error| error.to_string())
                        .and_then(|(code, log, changed)| {
                            captured |= changed;
                            match code {
                                0 => Ok(()),
                                code => Err(format!("`resume` exited {code}; log: {log}")),
                            }
                        });
                    (entry.action.clone(), result)
                } else {
                    if supervisor.running_pid(name).is_some() {
                        continue;
                    }
                    // The action the stop recorded, unless the definition has
                    // since dropped it; then whichever long-running action it
                    // has now.
                    let action = definition
                        .actions
                        .get_key_value(entry.action.as_str())
                        .filter(|(_, action)| action.long_running)
                        .map(|(name, action)| (name.as_str(), action))
                        .or_else(|| long_running_action(definition));
                    let result = match action {
                        None => Err(format!(
                            "resource `{name}` no longer has a long-running action to start"
                        )),
                        Some((action_name, action)) => blocked()
                            .and_then(|()| {
                                self.start_supervised(&branch, definition, action, action_name)
                            })
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                    };
                    let action =
                        action.map_or_else(|| entry.action.clone(), |(name, _)| name.to_owned());
                    (action, result)
                };

                let (status, error) = match result {
                    Ok(()) => (StopStartStatus::Started, None),
                    Err(error) => {
                        still_stopped.push(entry.clone());
                        failed_here.insert(name.clone());
                        (StopStartStatus::Failed, Some(error))
                    }
                };
                resources.push(StopStartResource {
                    name: name.clone(),
                    action,
                    status,
                    error,
                });
            }
        }
        record.resources = still_stopped;
        self.save_stopped_record(&branch, &record)?;
        if captured {
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
        }
        Ok(StopStartOutcome {
            instance: branch.name,
            resources,
        })
    }

    /// Run `suspend` or `resume` quietly, so `--json` keeps stdout to
    /// itself, and merge what it captured into the binding: a `resume` that
    /// brings a tunnel back may hand out a new URL. Returns the exit code,
    /// the log, and whether the binding changed.
    fn run_lifecycle_action(
        &self,
        branch: &mut BranchInstance,
        definition: &ResourceDefinition,
        action_name: &str,
    ) -> Result<(i32, Utf8PathBuf, bool)> {
        let action =
            definition
                .actions
                .get(action_name)
                .ok_or_else(|| NewgitError::UnknownAction {
                    resource: definition.name.clone(),
                    action: action_name.to_owned(),
                })?;
        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));
        let (code, found) =
            self.run_one_shot(branch, definition, action, action_name, &log, true)?;
        let changed = code == 0 && Self::apply_captures(branch, &definition.name, found.found);
        Ok((code, log, changed))
    }

    /// Point the store's branch ref at the checkpointed head. The branch is
    /// owned by this instance — divergence is policy, not mechanism — so a
    /// store-side advance is warned about loudly, never hard-refused.
    /// `expected_old` silences the warning when the ref is knowingly moved
    /// backwards from a rev a checkpoint ref keeps alive (undo).
    fn bless_store_branch(
        &self,
        branch: &mut BranchInstance,
        head_rev: &str,
        expected_old: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let branch_ref = format!("refs/heads/{}", branch.source_ref);
        if let Some(old) = self.source.ref_rev(&branch_ref)
            && old != head_rev
            && expected_old != Some(old.as_str())
            && !self.source.is_ancestor(&old, head_rev)?
        {
            warnings.push(format!(
                "store branch `{}` had commits this workspace does not (was at {}); it now \
                 points at {} — the old commits remain in the store repository but no branch \
                 ref reaches them",
                branch.source_ref,
                &old[..8.min(old.len())],
                &head_rev[..8.min(head_rev.len())]
            ));
        }
        self.source.update_ref(&branch_ref, head_rev)?;
        branch.source_rev = head_rev.to_owned();
        Ok(())
    }

    fn checkpoint_log(&self, branch: &BranchInstance) -> CheckpointLog {
        CheckpointLog::new(self.store.checkpoint_dir(&branch.slug), &branch.name)
    }

    fn definition(&self, tracker: &str) -> Result<&TrackerDefinition> {
        self.refuse_root(tracker, "commit to it with Git instead")?;
        self.trackers
            .iter()
            .find(|definition| definition.name == tracker)
            .ok_or_else(|| NewgitError::UnknownTracker(tracker.to_owned()))
    }

    fn definition_or_load(&self, tracker: &str) -> Result<TrackerDefinition> {
        self.refuse_root(tracker, "commit to it with Git instead")?;
        if let Some(definition) = self
            .trackers
            .iter()
            .find(|definition| definition.name == tracker)
        {
            return Ok(definition.clone());
        }
        let path = self.store.paths().trackers.join(format!("{tracker}.toml"));
        if path.is_file() {
            return TrackerDefinition::from_file(tracker, &path);
        }
        Err(NewgitError::UnknownTracker(tracker.to_owned()))
    }

    /// [`ResourceState::consumable_state_ref`], with deposited content
    /// materialized first: the recorded path points into the lane checkout
    /// cache, which may have been cleared since the checkpoint wrote it.
    fn consumable_state_ref(&self, state: &ResourceState) -> Result<Option<String>> {
        if state.state_path.is_some()
            && let Some((tracker, rev)) =
                state.state_ref.as_deref().and_then(parse_tracker_state_ref)
        {
            self.lane(tracker).checkout_dir(rev)?;
        }
        Ok(state.consumable_state_ref())
    }

    fn lane(&self, tracker: &str) -> TrackerRepo {
        let paths = self.store.paths();
        TrackerRepo::new(&paths.tracker_store, &paths.tracker_checkouts, tracker)
    }

    fn require_workspace(&self, branch: &BranchInstance) -> Result<()> {
        if branch.workspace_path.is_dir() {
            Ok(())
        } else {
            Err(NewgitError::Unsupported(format!(
                "the workspace for `{}` is missing at {}; spawn it again or remove the instance",
                branch.name, branch.workspace_path
            )))
        }
    }
}

/// What one resource's checkpoint mode produced.
struct CapturedResource {
    mode: String,
    state_ref: Option<String>,
    state_path: Option<Utf8PathBuf>,
    /// Lane deposit made via `into_tracker`: (tracker, rev).
    deposit: Option<(String, String)>,
}

impl CapturedResource {
    fn none() -> Self {
        Self {
            mode: "none".to_owned(),
            state_ref: None,
            state_path: None,
            deposit: None,
        }
    }
}

/// Resolve the working directory one resource command runs in: `workdir`
/// joined onto the workspace root, or the workspace root itself when none is
/// set. Checked here rather than at bind time — a `workdir` may legitimately
/// be created by an earlier action (e.g. `prepare` cloning a submodule into
/// it) — so every command site calls this right before it spawns, and a
/// missing directory names the resource, the action, and the resolved path
/// instead of surfacing as a bare "No such file or directory" from the shell.
fn resource_cwd(
    workspace_root: &Utf8Path,
    resource: &str,
    action: &str,
    workdir: Option<&Utf8Path>,
) -> Result<Utf8PathBuf> {
    let Some(workdir) = workdir else {
        return Ok(workspace_root.to_owned());
    };
    let resolved = workspace_root.join(workdir);
    if !resolved.is_dir() {
        return Err(NewgitError::MissingWorkdir {
            resource: resource.to_owned(),
            action: action.to_owned(),
            path: resolved,
        });
    }
    Ok(resolved)
}

/// The `pre-receive` hook a workspace's pushes run in the outbox.
///
/// It only prepares the environment and hands over. Git runs a receiving
/// hook with `GIT_DIR` and a quarantined object directory set, and every
/// `git` newgit spawns would inherit them: `GIT_DIR` would point every
/// store and workspace command at the outbox, and the quarantine forbids
/// ref updates in any repository — including the ones a checkpoint makes. The quarantine directory is passed on by name
/// instead, for the forwarded push to read. `GIT_CONFIG_PARAMETERS` carries
/// the `-c core.hooksPath` that selected this hook, and must not reach a
/// push to a remote that is itself a local path.
fn pre_receive_script(project_root: &Utf8Path, program: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by newgit; rewritten on spawn and checkpoint, so edits do not last.\n\
         # A `git push` from a branch instance's workspace arrives here: newgit\n\
         # checkpoints the instance, then forwards the push to the real remote.\n\
         NEWGIT_ADMITTING=\"$GIT_OBJECT_DIRECTORY\"\n\
         export NEWGIT_ADMITTING\n\
         instance=\"$NEWGIT_PUSH_INSTANCE\"\n\
         unset GIT_DIR GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY\n\
         unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_CONFIG_PARAMETERS NEWGIT_PUSH_INSTANCE\n\
         cd {} || exit 1\n\
         exec {} push-pre-receive -- \"$instance\"\n",
        sh_quote(project_root.as_str()),
        sh_quote(program)
    )
}

/// How the push route calls back into newgit: the bare name `newgit`
/// whenever that finds this very binary on `PATH`, and this binary's full
/// path otherwise. See [`hook_program`].
fn newgit_program() -> String {
    hook_program(
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// The bare name survives an upgrade that moves the binary — a new install
/// anywhere on `PATH` is found by the next push, where a baked-in path
/// would break every workspace's push until its next checkpoint rewrote
/// it. The full path is for a build that is not on `PATH` (a `cargo run`,
/// a checkout under test): the hook must call that same build, not
/// whatever older `newgit` happens to be installed. A process that is not
/// newgit at all (a library caller, a test harness) can only name it.
fn hook_program(exe: Option<&std::path::Path>, path_var: Option<&std::ffi::OsStr>) -> String {
    let Some(exe) = exe.filter(|exe| exe.file_stem().is_some_and(|stem| stem == "newgit")) else {
        return "newgit".to_owned();
    };
    let same_file =
        |candidate: &std::path::Path| match (candidate.canonicalize(), exe.canonicalize()) {
            (Ok(candidate), Ok(exe)) => candidate == exe,
            _ => false,
        };
    let on_path = path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("newgit"))
        .find(|candidate| candidate.is_file());
    match (on_path, exe.to_str()) {
        (Some(found), _) if same_file(&found) => "newgit".to_owned(),
        (_, Some(exe)) => exe.to_owned(),
        (_, None) => "newgit".to_owned(),
    }
}

/// Quote `value` as one POSIX shell word.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// How many lines differ between the expected render and what is on disk —
/// enough to say how big the discarded edit is without printing a diff.
fn differing_lines(expected: &str, actual: &str) -> usize {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let common = expected
        .iter()
        .zip(actual.iter())
        .filter(|(left, right)| left != right)
        .count();
    common + expected.len().abs_diff(actual.len())
}

fn validate_ownership_with_replacement(
    definitions: &[TrackerDefinition],
    replacement: &TrackerDefinition,
) -> Result<()> {
    let mut updated = Vec::with_capacity(definitions.len());
    let mut replaced = false;
    for definition in definitions {
        if definition.name == replacement.name {
            updated.push(replacement.clone());
            replaced = true;
        } else {
            updated.push(definition.clone());
        }
    }
    if !replaced {
        updated.push(replacement.clone());
    }
    crate::tracker::resolve_ownership(&mut updated)
}

/// Why a per-user tracker came up empty for whoever is spawning: nothing of
/// theirs has reached its `main`, which is what "your own" means.
fn no_own_content(tracker: &str) -> String {
    format!(
        "it starts empty: your own `{tracker}` has nothing on its main yet — capture it in an \
         instance and `newgit tracker merge {tracker}`, or `newgit tracker capture {tracker} \
         --from-store` in the project"
    )
}

/// Where a checkpoint's source lives in its instance's fork.
/// The history newgit adopted — as `newgit init` found it, or as a clone
/// received it: published already, so never held against a push.
pub const ADOPTED_REF: &str = "refs/newgit/adopted";

/// The project root's name as an instance: its tracker branches are
/// `instances/main`, so no spawned instance may take the name.
const PROJECT_ROOT_INSTANCE: &str = "main";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerMergeAction {
    /// The instance did not change it, or the branch has its commit already.
    Unchanged,
    /// The branch had not moved on: it takes the instance's commit.
    FastForward,
    /// Both changed it: a merge commit in its own history.
    Merged,
    /// New to the branch with this merge (a tracker carved since).
    New,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerMerge {
    pub tracker: String,
    pub action: TrackerMergeAction,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub instance: String,
    pub into: String,
    /// The checkpoint taken first: what merged. Empty for the project root,
    /// whose commits are what merged.
    pub checkpoint: String,
    pub trackers: Vec<TrackerMerge>,
    /// The project on the remote whose branch moved, when there is one.
    pub remote: Option<String>,
    /// What happened to the project root after a merge into `main`.
    pub project_root: Option<String>,
}

/// What a person asks of a spawn beyond the defaults: `--use`, `--drop`,
/// and `--private`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingChoice {
    pub uses: Vec<String>,
    pub drops: Vec<String>,
    /// Spawn it private: on the remote, only its owner sees it until merged.
    pub private: bool,
}

/// What to do with uncommitted work in a code tracker being dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uncommitted {
    /// Refuse the drop, saying what to do.
    Refuse,
    /// Stash it; `newgit tracker use` brings it back.
    Stash,
    /// Throw it away, after a checkpoint so `newgit undo` gets it back.
    Discard,
}

/// What `newgit tracker use` or `drop` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingChange {
    pub tracker: String,
    /// It was already bound (use) or already dropped (drop): nothing done.
    pub already: bool,
    /// Uncommitted work stashed by the drop.
    pub stashed: bool,
    /// A stash from an earlier drop, brought back by the use.
    pub restored_stash: bool,
    /// The checkpoint a discarding drop took first.
    pub checkpoint: Option<String>,
    /// A secret tracker's content, captured before its files were removed.
    pub captured: Option<String>,
    /// Files a secret tracker's use projected back.
    pub files: usize,
    /// Trackers in the same role a use dropped to make way.
    pub replaced: Vec<String>,
    /// Trackers a use bound because it requires them.
    pub brought: Vec<String>,
    /// Resources a use bound and prepared.
    pub started: Vec<String>,
    /// The `autostart` actions of those resources, started once the use
    /// was done.
    pub autostarted: Vec<StopStartResource>,
    /// Resources a drop stopped.
    pub stopped: Vec<String>,
    /// Installs rebuilt because the binding is part of their identity: a
    /// tree keyed on the trackers the instance binds was built for the
    /// binding this change replaced.
    pub rebuilt: Vec<RebuiltInstall>,
}

/// An instance's binding as a `use` or `drop` found it: what
/// `{{bound…}}` rendered to, and which resources were bound.
struct BindingBefore {
    bound: crate::exports::Bound,
    resources: BTreeSet<String>,
}

/// One install `tracker use` or `drop` rebuilt for the new binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltInstall {
    pub name: String,
    /// The producing action succeeded, or a stored tree was cloned in.
    pub ok: bool,
    /// What the install store did: filled from an entry built for this
    /// binding before, or stored what the rebuild built.
    pub install: Option<InstallReport>,
    /// The action log, when the producing action ran.
    pub log: Option<Utf8PathBuf>,
    /// Why newgit could not run the rebuild at all, as opposed to the
    /// producing action failing (which `log` holds).
    pub error: Option<String>,
    /// Dependencies not ready, so this install was not rebuilt: its tree is
    /// still the old binding's, and it waits as `pending` behind them.
    pub blocked_by: Vec<String>,
}

fn stash_message(tracker: &str) -> String {
    format!("newgit: uncommitted work in `{tracker}`")
}

/// What `newgit remote add` published when it created the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedProject {
    /// `<owner>/<name>`.
    pub key: String,
    pub endpoint: String,
    /// Branch `main`: one commit per code tracker.
    pub list: BTreeMap<String, String>,
}

/// A checkpoint's code, composed here: its committed work, its tip, and the
/// committed commit of each code tracker that was readable.
struct ImportedCode {
    head: String,
    tip: String,
    trackers: Vec<(String, String)>,
}

/// Where the project repo keeps a code tracker's `branch` (`main`,
/// `instances/<slug>`): its own refs, so what it composes from is never
/// borrowed.
/// Where an export target's view of `branch` is composed in the project repo.
fn export_ref(target: &str, branch: &str) -> String {
    format!("refs/newgit/exports/{target}/{branch}")
}

/// A push Git refused because it was not a fast-forward.
fn rejected(stderr: &str) -> bool {
    stderr.contains("non-fast-forward") || stderr.contains("[rejected]")
}

fn local_tracker_ref(tracker: &str, branch: &str) -> String {
    format!("refs/newgit/trackers/{tracker}/{branch}")
}

/// The repo a tracker's content lives in on the remote: one per user for a
/// per-user (`audience = "user"`) tracker, one shared otherwise.
pub fn tracker_repo(project: &str, definition: &TrackerDefinition, login: &str) -> String {
    let prefix = project.replace('/', ".");
    if definition.audience == "user" {
        format!("{prefix}.t.{}.{login}", definition.name)
    } else {
        format!("{prefix}.t.{}", definition.name)
    }
}

/// An instance's fork of one code tracker on the remote.
pub fn fork_repo(project: &str, instance: &str, tracker: &str) -> String {
    format!("{}.i.{instance}.{tracker}", project.replace('/', "."))
}

/// Where a fork keeps a checkpoint's commit for its tracker.
pub fn fork_checkpoint_ref(id: &str) -> String {
    format!("refs/newgit/checkpoints/{id}")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::hook_program;

    fn binary(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let path = dir.join(name);
        std::fs::write(&path, "").expect("write");
        path
    }

    fn path_var(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).expect("join PATH")
    }

    #[test]
    fn the_installed_binary_is_named_so_an_upgrade_is_found() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        let exe = binary(&bin, "newgit");
        let path = path_var(&[&temp.path().join("empty"), &bin]);
        assert_eq!(hook_program(Some(&exe), Some(&path)), "newgit");
    }

    #[test]
    fn a_symlink_on_path_to_this_binary_counts_as_this_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = binary(&temp.path().join("cellar/newgit/0.5.0"), "newgit");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::os::unix::fs::symlink(&exe, bin.join("newgit")).expect("symlink");
        assert_eq!(hook_program(Some(&exe), Some(&path_var(&[&bin]))), "newgit");
    }

    #[test]
    fn a_build_off_path_is_called_by_its_full_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let installed = temp.path().join("bin");
        binary(&installed, "newgit");
        let exe = binary(&temp.path().join("target/debug"), "newgit");
        let program = hook_program(Some(&exe), Some(&path_var(&[&installed])));
        assert_eq!(program, exe.to_str().expect("utf8"));
    }

    #[test]
    fn only_the_first_newgit_on_path_is_what_the_bare_name_finds() {
        let temp = tempfile::tempdir().expect("tempdir");
        let shadowing = temp.path().join("shadow");
        binary(&shadowing, "newgit");
        let bin = temp.path().join("bin");
        let exe = binary(&bin, "newgit");
        let program = hook_program(Some(&exe), Some(&path_var(&[&shadowing, &bin])));
        assert_eq!(program, exe.to_str().expect("utf8"));
    }

    #[test]
    fn a_process_that_is_not_newgit_can_only_name_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = binary(temp.path(), "manager-tests");
        assert_eq!(hook_program(Some(&exe), None), "newgit");
    }
}
