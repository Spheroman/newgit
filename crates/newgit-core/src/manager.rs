use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;

use crate::branch::{
    BranchInstance, ResourceBinding, ResourceStatus, TrackerBinding, branch_slug, validate_name,
};
use crate::checkpoint::{
    CheckpointLog, CheckpointReason, CheckpointRecord, HASH_STATE_REF_PREFIX, RecoveryRecord,
    ResourceState, RestoreFailure, SourceState, TrackerState,
};
use crate::cleanup::{
    ArchivedCheckpoints, CleanupOutcome, FinalizedInstance, HookDetail, HookOutcome, PrunedInstall,
    PrunedRev, PurgedCheckpoints, SnapshotRoots, lane_revs, may_tear_down, orphan_workspaces,
};
use crate::config::ProjectConfig;
use crate::error::{NewgitError, Result};
use crate::export::{self, ExportFilter, ExportPlan, prepare_destination};
use crate::exports::{RenderContext, render, unresolved_placeholder};
use crate::installs::{Admission, InstallReport, InstallStore};
use crate::lane::{TrackerLane, clear_owned_paths, copy_file};
use crate::materializer::{
    Materializer, RealDirMaterializer, exclude_produced_paths, exclude_tracker_paths,
    unexclude_tracker_paths,
};
use crate::ports;
use crate::render::{self, RenderRecord};
use crate::resource::{
    Captures, CheckpointMode, DataEdges, GraphProblem, ResourceDefinition, ResourceGraph,
    RestoreMode, parse_captures, resolve_graph,
};
use crate::source::GitSource;
use crate::store::MetadataStore;
use crate::supervisor::{StopOutcome, Supervisor, run_captured, run_foreground};
use crate::templates::{instantiate, resource_template};
use crate::tracker::{Storage, TrackerDefinition, collect_files, collect_owned_files, content_rev};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOutcome {
    pub branch: BranchInstance,
    pub record_path: Utf8PathBuf,
    /// False when the instance attached to a pre-existing source branch.
    pub created_source_branch: bool,
    pub trackers: Vec<TrackerBindOutcome>,
    pub resources: Vec<ResourceBindOutcome>,
}

/// How a resource was bound at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBindOutcome {
    pub name: String,
    pub ports: BTreeMap<String, u16>,
    pub status: ResourceStatus,
    /// Present when a `prepare` action ran: (succeeded, log path).
    pub prepare: Option<(bool, Utf8PathBuf)>,
    /// Resource dependencies that prevented `prepare` from running.
    pub blocked_by: Vec<String>,
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
}

impl RenderCheckTarget {
    /// The path was readable and every `find` matched its declared count.
    pub fn ok(&self) -> bool {
        self.checks
            .as_ref()
            .is_some_and(|checks| checks.iter().all(render::FindCheck::ok))
    }
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOutcome {
    pub record: CheckpointRecord,
    pub record_path: Utf8PathBuf,
    pub warnings: Vec<String>,
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
    /// started from.
    pub fn is_proven(&self) -> bool {
        self.undo.is_complete()
            && self
                .resources
                .iter()
                .filter(|resource| resource.exercised)
                .all(|resource| resource.agree)
    }
}

/// One resource's before/after comparison from a verify run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResource {
    pub name: String,
    /// False for `[restore] mode = "none"` or `"external"` — nothing ran
    /// that could prove or disprove anything, so agreement here is trivial.
    pub exercised: bool,
    pub before_state_ref: Option<String>,
    pub after_state_ref: Option<String>,
    pub agree: bool,
}

/// What this undo knows about a resource beyond the checkpoint being
/// restored — the facts a `recompute` needs to decide whether to run at all.
#[derive(Debug, Clone, Copy)]
struct RestoreContext<'a> {
    /// This resource's state ref in the safety checkpoint taken moments ago:
    /// the workspace as it stood before this undo touched anything.
    pre_undo_state_ref: Option<&'a str>,
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
    /// `command`, `external (no-op)`; `+ restarted` when a process came back.
    pub action: String,
    pub name: String,
    pub ok: bool,
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
        let trackers = store.load_tracker_definitions()?;
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
        Ok(Self {
            store,
            installs,
            config,
            source,
            trackers,
            resources,
            graph,
        })
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

    /// The gate for commands that act on the graph — `spawn`, `run`, `action`,
    /// `checkpoint`, `undo`, `remove`. Commands that *build* the graph
    /// (`tracker create`, `tracker track`, `resource add`) must not call this:
    /// they are how an incomplete graph gets completed.
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
        self.require_resolvable_graph()?;
        validate_name(name)?;

        let slug = branch_slug(name);
        let record_path = self.store.branch_record_path(&slug);
        if record_path.exists() {
            return Err(NewgitError::BranchInstanceExists {
                name: name.to_owned(),
                path: record_path,
            });
        }

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
        if let Some((base_ref, base_rev)) = base {
            branch = branch.with_base(base_ref, base_rev);
        }

        RealDirMaterializer.materialize(&self.source, &branch)?;

        // Before any lane content lands, make the clone's Git ignore the
        // paths trackers own and the trees resources build — otherwise both
        // arrive as untracked files an agent, or a checkpoint's `git add
        // -A`, can commit into source history.
        self.ensure_workspace_excludes(&branch)?;

        let mut tracker_outcomes = Vec::new();
        for definition in &self.trackers {
            let outcome = self.bind_tracker(&mut branch, definition, false)?;
            tracker_outcomes.push(outcome);
        }

        let resource_outcomes = self.bind_resources(&mut branch)?;

        let record_path = self.store.create_branch_record(&branch)?;

        Ok(SpawnOutcome {
            branch,
            record_path,
            created_source_branch,
            trackers: tracker_outcomes,
            resources: resource_outcomes,
        })
    }

    /// Allocate ports, render exports, and run `prepare` hooks in dependency
    /// order. Prepare failures are loud but leave the instance spawned —
    /// re-run with `newgit action <resource>.prepare`.
    fn bind_resources(&self, branch: &mut BranchInstance) -> Result<Vec<ResourceBindOutcome>> {
        let mut used = ports::used_ports(&self.store.load_branches()?);
        let mut outcomes = Vec::new();

        for name in &self.graph.bind_order {
            let definition = self.resource_definition(name)?;

            let mut resolved_ports = BTreeMap::new();
            for (port_name, request) in &definition.ports {
                resolved_ports.insert(
                    port_name.clone(),
                    ports::allocate(request.start, &mut used)?,
                );
            }

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
                    captured: Vec::new(),
                    missing_captures: Vec::new(),
                    rendered: Vec::new(),
                    render_error,
                    export_error,
                    install: None,
                });
                continue;
            }

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
            let runnable_prepare = match definition.actions.get("prepare") {
                Some(action) if action.command.is_some() && !action.long_running => Some(action),
                _ => None,
            };
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
                        self.run_one_shot(branch, definition, action, "prepare", &log)?;
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
                match run_captured(command, &branch.workspace_path, &env, &log) {
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
        match store.fill(
            &entry,
            &branch.workspace_path,
            definition.identity_produces(),
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
        match store.replace(
            &entry,
            &branch.workspace_path,
            definition.identity_produces(),
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
        match store.admit(
            &definition.name,
            &key,
            &branch.workspace_path,
            definition.identity_produces(),
        ) {
            Ok(Admission::Stored(method)) => Some(InstallReport::Stored { key, method }),
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

        for spec in &definition.render {
            let owner = self.tracker_owning(&spec.path);
            let committed = self.committed_content(branch, &spec.path, owner.as_deref())?;
            let Some(committed) = committed else {
                return Err(NewgitError::RenderPathNotCommitted {
                    resource: definition.name.clone(),
                    path: spec.path.clone(),
                });
            };

            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: Some(&context_ports),
                exports: Some(&context_exports),
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
            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: Some(resolved_ports),
                exports: Some(&visible),
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
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: Some(resolved_ports),
            exports: Some(&visible),
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
        let source = self.lane(tracker).rev_path(&rev).join(path);
        match std::fs::read_to_string(&source) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(NewgitError::io(source, error)),
        }
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
                if actual != expected {
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
        self.require_resolvable_graph()?;
        let (resource_name, action_name) = spec.split_once('.').ok_or_else(|| {
            NewgitError::Unsupported(format!("`{spec}` is not of the form <resource>.<action>"))
        })?;
        let mut branch = self.store.find_branch(instance)?;
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
        let supervisor = self.supervisor(&branch);

        // Signal-only action (e.g. stop): signal the supervised process.
        if action.command.is_none() {
            let signal = action
                .signal
                .clone()
                .unwrap_or_else(|| definition.stop_signal());
            return Ok(ActionOutcome::Stopped(
                supervisor.stop(&definition.name, &signal)?,
            ));
        }

        // Refusing to run is the right call, but it is not this resource's own
        // outcome — it is derived from a dependency, and re-derivable the
        // moment that dependency changes. Nothing is written to the record.
        let blocked_by = self.blocked_dependencies(&branch, definition);
        if !blocked_by.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "resource `{resource_name}` is blocked by failed dependency/dependencies: {}",
                blocked_by.join(", ")
            )));
        }

        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));

        if action.long_running {
            let command = self.rendered_command(&branch, definition, action)?;
            let env = self.assemble_env(&branch)?;
            let cwd = resource_cwd(
                &branch.workspace_path,
                &definition.name,
                action_name,
                definition.workdir_for(Some(action)),
            )?;
            let pid = supervisor.start(&definition.name, &command, &cwd, &env, &log)?;
            return Ok(ActionOutcome::Started { pid, log });
        }

        let (code, captured) = self.run_one_shot(&branch, definition, action, action_name, &log)?;
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
    ) -> Result<(i32, Captures)> {
        let command = self.rendered_command(branch, definition, action)?;
        let env = self.assemble_env(branch)?;
        let cwd = resource_cwd(
            &branch.workspace_path,
            &definition.name,
            action_name,
            definition.workdir_for(Some(action)),
        )?;

        if action.captures.is_empty() {
            let code = run_foreground(
                &["sh".to_owned(), "-c".to_owned(), command],
                &cwd,
                &env,
                log,
            )?;
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
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: branch
                .resources
                .get(&definition.name)
                .map(|binding| &binding.resolved_ports),
            ..RenderContext::default()
        };
        Ok(render(&command, &context))
    }

    /// The layered environment `newgit run` and actions see. Later layers
    /// win: resource exports in dependency order → port env vars → newgit
    /// context vars. Trackers own content; command environment wiring lives
    /// outside the tracker primitive.
    pub fn assemble_env(&self, branch: &BranchInstance) -> Result<Vec<(String, String)>> {
        // Layering depends on dependency order, so the order has to be real.
        self.require_resolvable_graph()?;
        let mut env: BTreeMap<String, String> = BTreeMap::new();

        // Layer 1: resource exports, dependency order (dependents win).
        for name in &self.graph.bind_order {
            if let Some(binding) = branch.resources.get(name) {
                for (key, value) in &binding.resolved_exports {
                    env.insert(key.clone(), value.clone());
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
                    env.insert(env_name.clone(), port.to_string());
                }
            }
        }

        // Layer 3: context vars.
        env.insert("NEWGIT_BRANCH".to_owned(), branch.name.clone());
        env.insert(
            "NEWGIT_WORKSPACE".to_owned(),
            branch.workspace_path.to_string(),
        );

        Ok(env.into_iter().collect())
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
                let full = root.join(&spec.path);
                let checks = match std::fs::read_to_string(&full) {
                    Ok(content) => Some(render::check(spec, &content)),
                    Err(_) => None,
                };
                targets.push(RenderCheckTarget {
                    resource: definition.name.clone(),
                    path: spec.path.clone(),
                    tracker,
                    checks,
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
        for mut branch in bound {
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

    fn blocked_dependencies(
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

        // Safety net when re-materializing over existing content.
        if refresh && !definition.paths.is_empty() {
            let existing = collect_owned_files(&workspace, definition)?;
            if !existing.is_empty() {
                lane.capture(&workspace, definition)?;
            }
        }

        let (origin, content_rev, files) = match lane.latest() {
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

    pub fn capture_tracker(&self, instance: &str, tracker: &str) -> Result<CaptureReport> {
        let mut branch = self.store.find_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        // A lane is shared by every instance, so this instance's rendered
        // values must not enter it — but the file cannot simply be skipped
        // either, or a key added beside them would never reach the lane.
        // Reversing the substitution keeps the edits and drops the values.
        let reversed = self.unrender_for_capture(&branch, definition)?;

        let lane = self.lane(&definition.name);
        let capture = lane.capture(&branch.workspace_path, definition);
        reversed.restore()?;
        let capture = capture?;

        let previous = branch
            .trackers
            .get(&definition.name)
            .and_then(|binding| binding.content_rev.clone());
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
        let capture = lane.capture(&root, definition)?;
        let changed = lane.latest().as_deref() != Some(capture.rev.as_str());
        lane.set_latest(&capture.rev)?;

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
        let mut branch = self.store.find_branch(instance)?;
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
        let safety = lane.capture(&branch.workspace_path, definition);
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
        let mut branch = self.store.find_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        if self.lane(&definition.name).latest().is_none() {
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
        lane.set_latest(&rev)?;
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
        let definition = self.definition_or_load(tracker)?;
        let updated = definition.with_added_paths(paths)?;
        validate_disjoint_with_replacement(&self.trackers, &updated)?;
        let path = self.store.save_tracker_definition(&updated)?;

        let mut patterns = Vec::new();
        for owned in paths {
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
            added_paths: paths.to_vec(),
            ignored_patterns: patterns,
            seedable,
        })
    }

    /// The inverse of `tracker create` — undoes exactly what `tracker
    /// create`/`tracker track` did: the definition file, the store
    /// `.gitignore` block, and each live workspace's `.git/info/exclude`
    /// entries. Captured lane content under `.newgit/snapshots/<name>/` is
    /// left in place; see the caller for why that is safe to leave to
    /// `newgit cleanup`.
    pub fn remove_tracker(&self, name: &str) -> Result<RemoveTrackerOutcome> {
        if name == "source" {
            return Err(NewgitError::Unsupported(
                "`source` is the default tracker: Git/jj owns its history, and there is no \
                 `.newgit/trackers/source.toml` to remove"
                    .to_owned(),
            ));
        }

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
            .map(|definition| {
                (
                    definition.name.clone(),
                    self.lane(&definition.name).latest(),
                )
            })
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
                                } else {
                                    let blocked_by = self.blocked_dependencies(&branch, definition);
                                    if !blocked_by.is_empty() {
                                        format!("blocked({})", blocked_by.join(","))
                                    } else {
                                        match binding.status {
                                            ResourceStatus::Pending => "pending".to_owned(),
                                            ResourceStatus::Ready => "ready".to_owned(),
                                            ResourceStatus::Failed => "failed".to_owned(),
                                        }
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
                Ok(InstanceReport {
                    branch,
                    workspace_exists,
                    live_rev,
                    trackers,
                    resources,
                    base,
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
    ///
    /// Checkpoints outlive removal by default — they pin the lane revs their
    /// undo would need, and the instance may be re-created. `checkpoints =
    /// Purge` says that undo will never be wanted, and drops them so the next
    /// `cleanup` can reclaim what they held.
    pub fn remove(
        &self,
        name: &str,
        cwd: &Utf8Path,
        checkpoints: ArchivedCheckpoints,
    ) -> Result<RemoveOutcome> {
        let branch = self.store.find_branch(name)?;

        if cwd.starts_with(&branch.workspace_path) {
            return Err(NewgitError::Unsupported(format!(
                "the current directory is inside the workspace of `{}`; step out of it before \
                 removing",
                branch.name
            )));
        }

        // Stop anything still running before the workspace disappears.
        let supervisor = self.supervisor(&branch);
        for definition in &self.resources {
            if supervisor.running_pid(&definition.name).is_some() {
                supervisor.stop(&definition.name, &definition.stop_signal())?;
            }
        }

        let hooks = self.run_cleanup_hooks(&branch, false)?;

        let state_dir = self.store.instance_state_dir(&branch.slug);
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir)
                .map_err(|source| NewgitError::io(state_dir, source))?;
        }

        RealDirMaterializer.remove(&branch)?;
        let archived_record = self.store.archive_branch_record(&branch)?;
        // Either way, count what the instance leaves behind: checkpoints kept
        // for an unreachable instance are disk nobody will reclaim by accident.
        let keeping = checkpoints == ArchivedCheckpoints::Keep;
        let found = self.purge_checkpoints(&branch.slug, keeping)?;
        let (purged_checkpoints, kept_checkpoints) = match (keeping, found) {
            (true, found) => (None, found.map_or(0, |plan| plan.checkpoints)),
            (false, purged) => (purged, 0),
        };

        Ok(RemoveOutcome {
            branch,
            archived_record,
            hooks,
            purged_checkpoints,
            kept_checkpoints,
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
            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: binding.map(|binding| &binding.resolved_ports),
                exports: binding.map(|binding| &binding.resolved_exports),
                snapshot_path: None,
                state_ref: state_ref.as_deref(),
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
            let env = self.assemble_env(branch)?;
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
                && let Some(resolved) = state.consumable_state_ref()
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

        for branch in &stale {
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

        // Unclaimed workspace directories: a failed spawn, or a record
        // archived while its directory survived.
        let workspace_root = self.config.workspace_root(&self.store.paths().project_root);
        let (orphans, unrecognized) = orphan_workspaces(&workspace_root, &branches)?;
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
        for lane_rev in lane_revs(&self.store.paths().snapshots)? {
            if !lane_rev.is_staging && roots.contains(&lane_rev.tracker, &lane_rev.rev) {
                continue;
            }
            if !dry_run {
                std::fs::remove_dir_all(&lane_rev.path)
                    .map_err(|source| NewgitError::io(&lane_rev.path, source))?;
            }
            outcome.pruned.push(PrunedRev {
                tracker: lane_rev.tracker,
                rev: lane_rev.rev,
                path: lane_rev.path,
            });
        }
        outcome.pinned_by_checkpoints = roots.pinned_only_by_checkpoints().count();
        outcome.pinned_by_archived = roots.pinned_only_by_archived_checkpoints().count();

        self.prune_install_store(&surviving, dry_run, &mut outcome)?;

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

        Ok(ExportOutcome {
            destination: destination.to_path_buf(),
            branch: branch.source_ref.clone(),
            instance: branch.name.clone(),
            source_head: head_rev,
            commit,
            plan,
        })
    }

    /// Record one coherent snapshot across source, trackers, and resources.
    pub fn checkpoint(&self, instance: &str, message: Option<&str>) -> Result<CheckpointOutcome> {
        self.require_resolvable_graph()?;
        let mut branch = self.store.find_branch(instance)?;
        self.checkpoint_branch(&mut branch, message, CheckpointReason::Explicit)
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
        let mut branch = self.store.find_branch(instance)?;
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

        let resources = before
            .record
            .resource_states
            .iter()
            .map(|before_state| {
                let after_ref = after
                    .record
                    .resource_states
                    .iter()
                    .find(|state| state.name == before_state.name)
                    .and_then(|state| state.state_ref.clone());
                VerifyResource {
                    name: before_state.name.clone(),
                    exercised: before_state.restore_exercisable,
                    before_state_ref: before_state.state_ref.clone(),
                    after_state_ref: after_ref.clone(),
                    agree: before_state.state_ref == after_ref,
                }
            })
            .collect();

        Ok(VerifyOutcome {
            before: before.record,
            undo,
            after: after.record,
            resources,
        })
    }

    fn checkpoint_branch(
        &self,
        branch: &mut BranchInstance,
        message: Option<&str>,
        reason: CheckpointReason,
    ) -> Result<CheckpointOutcome> {
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
        let mut deposits: Vec<(String, String)> = Vec::new();
        for name in self.graph.lifecycle_order.iter().rev() {
            let Some(binding) = branch.resources.get(name) else {
                continue;
            };
            let definition = self.resource_definition(name)?;
            let was_running = self.supervisor(branch).running_pid(name).is_some();
            // A safety checkpoint minted ahead of an undo captures whatever
            // is there so redo has something to restore to. But a resource
            // whose last restore already failed is known-broken, not merely
            // unobserved — running its checkpoint command (a `pg_dump`, say)
            // against it spends real time and I/O dumping rubble nobody is
            // likely to ask to go back to. Explicit checkpoints still run
            // it: a person asked for that one on purpose (#58).
            let captured = if reason == CheckpointReason::BeforeUndo
                && binding.status == ResourceStatus::Failed
            {
                warnings.push(format!(
                    "resource `{name}` is in a failed state from its last restore; skipped its \
                     checkpoint capture rather than run an expensive command against \
                     known-broken state"
                ));
                CapturedResource::none()
            } else {
                self.checkpoint_resource(branch, definition)?
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
            let content_rev = if definition.paths.is_empty() {
                branch
                    .trackers
                    .get(&definition.name)
                    .and_then(|binding| binding.content_rev.clone())
            } else {
                Some(
                    self.lane(&definition.name)
                        .capture(&workspace, definition)?
                        .rev,
                )
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
        };
        let record_path = checkpoint_log.save(&record)?;
        branch.updated_at = Utc::now();
        self.store.save_branch_record(branch)?;

        Ok(CheckpointOutcome {
            record,
            record_path,
            warnings,
        })
    }

    fn checkpoint_resource(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
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

                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
                    snapshot_path: staging_path.as_deref().map(Utf8Path::as_str),
                    state_ref: None,
                };
                let command = render(template, &context);
                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.checkpoint", definition.name));
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
                        let deposit = lane.deposit(staging_path)?;
                        // A path the command echoed under {{snapshot.path}}
                        // maps to the same relative location inside the lane.
                        let state_path = Utf8Path::new(&stdout)
                            .strip_prefix(staging_path)
                            .map(|relative| lane.rev_path(&deposit.rev).join(relative))
                            .unwrap_or_else(|_| lane.rev_path(&deposit.rev));
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
                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
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
        let mut branch = self.store.find_branch(instance)?;
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

        // Nothing may keep running while content changes underneath it. A
        // partial undo leaves every other resource alone, so it must not stop
        // them either.
        let supervisor = self.supervisor(&branch);
        for definition in &self.resources {
            if wanted(&definition.name) && supervisor.running_pid(&definition.name).is_some() {
                supervisor.stop(&definition.name, &definition.stop_signal())?;
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
            GitSource::workspace_fetch_ref(
                &workspace,
                self.source.root(),
                &restored.source.store_ref,
            )?;
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
        }

        // Trackers: plain content, restored exactly; should not partially
        // fail in interesting ways, so failures here are hard errors.
        let mut trackers = Vec::new();
        for state in restored.tracker_states.iter().filter(|_| !partial) {
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
            if definition.definition_rev != state.definition_rev {
                warnings.push(format!(
                    "resource `{name}` definition changed since the checkpoint; restored with \
                     the current definition"
                ));
            }

            let pre_undo = safety
                .record
                .resource_states
                .iter()
                .find(|pre| &pre.name == name)
                .and_then(|pre| pre.state_ref.as_deref());
            let (mut action_label, ok, exercised) = self.restore_resource(
                &mut branch,
                definition,
                state,
                RestoreContext {
                    pre_undo_state_ref: pre_undo,
                    force_recompute: options.force_recompute,
                },
                &mut failures,
                &mut warnings,
            )?;
            if let Some(binding) = branch.resources.get_mut(name) {
                binding.status = if ok {
                    ResourceStatus::Ready
                } else {
                    ResourceStatus::Failed
                };
                // Sticky once true: this answers "has restore ever
                // completed", not "would it complete right now". A skip (no
                // command ran) or a later failure must not erase a proof
                // that already happened.
                if ok && exercised {
                    binding.restore_proven = true;
                }
            }

            if ok && state.was_running {
                match self.restart_long_running(&branch, definition) {
                    Ok(true) => action_label.push_str(" + restarted"),
                    Ok(false) => warnings.push(format!(
                        "resource `{name}` was running at checkpoint time but has no \
                         long-running action to restart"
                    )),
                    Err(error) => failures.push(RestoreFailure {
                        resource: name.clone(),
                        detail: format!("restart failed: {error}"),
                        log: None,
                        retry_with: format!("newgit action {name}.start {}", branch.name),
                    }),
                }
            }
            resources.push(UndoResourceOutcome {
                name: name.clone(),
                action: action_label,
                ok,
            });
        }

        let recovery_record = if failures.is_empty() {
            None
        } else {
            for failure in &failures {
                if let Some(binding) = branch.resources.get_mut(&failure.resource) {
                    binding.status = ResourceStatus::Failed;
                }
            }
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
                // Identity describes the inputs, not the tree: someone can
                // delete half of `node_modules` without touching the lockfile,
                // and then the skip is wrong. That is what `--force-recompute`
                // is for, and why the skip says so on the line rather than
                // passing silently.
                if !undo.force_recompute
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
                    self.run_one_shot(branch, definition, action, action_name, &log)?;
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
                        Some(InstallReport::Stored { key, method }) => {
                            label.push_str(&format!(
                                "; install store: stored as {key} ({})",
                                method.label()
                            ));
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
                let state_ref = state.consumable_state_ref();
                let context = RenderContext {
                    branch_name: &branch.name,
                    branch_slug: &branch.slug,
                    workspace: branch.workspace_path.as_str(),
                    scripts: self.scripts_dir(),
                    ports: binding.map(|binding| &binding.resolved_ports),
                    exports: binding.map(|binding| &binding.resolved_exports),
                    snapshot_path: None,
                    state_ref: state_ref.as_deref(),
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
    fn restart_long_running(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
    ) -> Result<bool> {
        let Some((action_name, action)) = definition
            .actions
            .iter()
            .find(|(_, action)| action.long_running)
        else {
            return Ok(false);
        };
        let log = self
            .store
            .action_log_path(&branch.slug, &format!("{}.{action_name}", definition.name));
        let command = self.rendered_command(branch, definition, action)?;
        let env = self.assemble_env(branch)?;
        self.supervisor(branch).start(
            &definition.name,
            &command,
            &branch.workspace_path,
            &env,
            &log,
        )?;
        Ok(true)
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
        self.trackers
            .iter()
            .find(|definition| definition.name == tracker)
            .ok_or_else(|| NewgitError::UnknownTracker(tracker.to_owned()))
    }

    fn definition_or_load(&self, tracker: &str) -> Result<TrackerDefinition> {
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

    fn lane(&self, tracker: &str) -> TrackerLane {
        TrackerLane::new(&self.store.paths().snapshots, tracker)
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

fn validate_disjoint_with_replacement(
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
    crate::tracker::validate_disjoint(&updated)
}
