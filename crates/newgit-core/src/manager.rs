use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;

use crate::branch::{
    BranchInstance, ResourceBinding, ResourceStatus, TrackerBinding, branch_slug, validate_name,
};
use crate::checkpoint::{
    CheckpointLog, CheckpointReason, CheckpointRecord, RecoveryRecord, ResourceState,
    RestoreFailure, SourceState, TrackerState,
};
use crate::cleanup::{
    ArchivedCheckpoints, CleanupOutcome, FinalizedInstance, HookDetail, HookOutcome, PrunedRev,
    PurgedCheckpoints, SnapshotRoots, lane_revs, may_tear_down, orphan_workspaces,
};
use crate::config::ProjectConfig;
use crate::error::{NewgitError, Result};
use crate::export::{self, ExportFilter, ExportPlan, prepare_destination};
use crate::exports::{RenderContext, render, unresolved_placeholder};
use crate::lane::{TrackerLane, clear_owned_paths, copy_file};
use crate::materializer::{Materializer, RealDirMaterializer, exclude_tracker_paths};
use crate::ports;
use crate::resource::{
    Captures, CheckpointMode, GraphProblem, ResourceDefinition, RestoreMode, parse_captures,
    resolve_order,
};
use crate::source::GitSource;
use crate::store::MetadataStore;
use crate::supervisor::{StopOutcome, Supervisor, run_captured, run_foreground};
use crate::templates::resource_template;
use crate::tracker::{Storage, TrackerDefinition, collect_files, collect_owned_files, content_rev};

/// Orchestrates branch-instance lifecycle against one store.
#[derive(Debug)]
pub struct BranchManager {
    store: MetadataStore,
    config: ProjectConfig,
    source: GitSource,
    trackers: Vec<TrackerDefinition>,
    resources: Vec<ResourceDefinition>,
    /// Resource names, dependencies before dependents. Resources whose
    /// dependencies are unresolved are still ordered; see `graph_problems`.
    resource_order: Vec<String>,
    /// Why the dependency graph does not hold together, if it doesn't.
    /// Commands that build the graph warn; commands that act on it refuse.
    graph_problems: Vec<GraphProblem>,
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
    },
}

/// How a tracker's content landed in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerBindOutcome {
    pub name: String,
    pub content_rev: Option<String>,
    pub files: usize,
    pub origin: BindOrigin,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
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

impl BranchManager {
    pub fn open(store: MetadataStore) -> Result<Self> {
        store.ensure_initialized()?;
        let config = store.load_config()?;
        let source = GitSource::open(&store.paths().project_root, config.project.source)?;
        let trackers = store.load_tracker_definitions()?;
        let resources = store.load_resource_definitions()?;
        let tracker_names: BTreeSet<String> = trackers.iter().map(|t| t.name.clone()).collect();
        let (resource_order, graph_problems) = resolve_order(&resources, &tracker_names);
        Ok(Self {
            store,
            config,
            source,
            trackers,
            resources,
            resource_order,
            graph_problems,
        })
    }

    /// Ways the resource graph is incomplete. An empty slice means it resolves.
    pub fn graph_problems(&self) -> &[GraphProblem] {
        &self.graph_problems
    }

    /// The gate for commands that act on the graph — `spawn`, `run`, `action`,
    /// `checkpoint`, `undo`, `remove`. Commands that *build* the graph
    /// (`tracker create`, `tracker track`, `resource add`) must not call this:
    /// they are how an incomplete graph gets completed.
    pub fn require_resolvable_graph(&self) -> Result<()> {
        match self.graph_problems.first() {
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

        let created_source_branch = if self.source.branch_exists(name)? {
            if let Some(base) = from {
                return Err(NewgitError::Unsupported(format!(
                    "source branch `{name}` already exists; `--from {base}` only applies when \
                     creating a new branch"
                )));
            }
            false
        } else {
            let base = from.unwrap_or("HEAD");
            let base_rev = self.source.rev_parse(base)?;
            self.source.create_branch(name, &base_rev)?;
            true
        };

        let source_rev = self.source.rev_parse(&format!("refs/heads/{name}"))?;
        let workspace_path = self
            .config
            .workspace_root(&self.store.paths().project_root)
            .join(&slug);
        let mut branch = BranchInstance::new(name, name, source_rev, workspace_path)?;

        RealDirMaterializer.materialize(&self.source, &branch)?;

        // Before any lane content lands, make the clone's Git ignore the
        // paths those lanes own — otherwise projected content arrives as
        // untracked files an agent can commit into source history.
        let owned: Vec<Utf8PathBuf> = self
            .trackers
            .iter()
            .flat_map(|definition| definition.paths.iter().cloned())
            .collect();
        exclude_tracker_paths(&branch.workspace_path, &owned)?;

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

        for name in &self.resource_order {
            let definition = self.resource_definition(name)?;

            let mut resolved_ports = BTreeMap::new();
            for (port_name, request) in &definition.ports {
                resolved_ports.insert(
                    port_name.clone(),
                    ports::allocate(request.start, &mut used)?,
                );
            }

            let context = RenderContext {
                branch_name: &branch.name,
                branch_slug: &branch.slug,
                workspace: branch.workspace_path.as_str(),
                scripts: self.scripts_dir(),
                ports: Some(&resolved_ports),
                ..RenderContext::default()
            };
            let resolved_exports = definition
                .exports
                .iter()
                .map(|(key, value)| (key.clone(), render(value, &context)))
                .collect();

            branch.resources.insert(
                definition.name.clone(),
                ResourceBinding {
                    definition_rev: definition.definition_rev.clone(),
                    resolved_ports: resolved_ports.clone(),
                    resolved_exports,
                    status: ResourceStatus::Pending,
                },
            );

            let blocked_by = self.blocked_dependencies(branch, definition);

            // Prepare runs with the bindings made so far, so dependents see
            // their dependencies' exports. Failed dependencies block
            // dependents; the instance still spawns so logs can be inspected.
            let mut captured_names = Vec::new();
            let mut missing_captures = Vec::new();
            let (status, prepare) = if !blocked_by.is_empty() {
                if let Some(binding) = branch.resources.get_mut(&definition.name) {
                    binding.status = ResourceStatus::Blocked;
                }
                (ResourceStatus::Blocked, None)
            } else {
                match definition.actions.get("prepare") {
                    Some(action) if action.command.is_some() && !action.long_running => {
                        let log = self
                            .store
                            .action_log_path(&branch.slug, &format!("{}.prepare", definition.name));
                        let (code, captured) =
                            self.run_one_shot(branch, definition, action, &log)?;
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
                        (status, Some((code == 0, log)))
                    }
                    _ => {
                        if let Some(binding) = branch.resources.get_mut(&definition.name) {
                            binding.status = ResourceStatus::Ready;
                        }
                        (ResourceStatus::Ready, None)
                    }
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
            });
        }
        branch.updated_at = Utc::now();
        Ok(outcomes)
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

        let blocked_by = self.blocked_dependencies(&branch, definition);
        if !blocked_by.is_empty() {
            if let Some(binding) = branch.resources.get_mut(&definition.name) {
                binding.status = ResourceStatus::Blocked;
                branch.updated_at = Utc::now();
                self.store.save_branch_record(&branch)?;
            }
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
            let pid = supervisor.start(
                &definition.name,
                &command,
                &branch.workspace_path,
                &env,
                &log,
            )?;
            return Ok(ActionOutcome::Started { pid, log });
        }

        let (code, captured) = self.run_one_shot(&branch, definition, action, &log)?;
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
        Ok(ActionOutcome::Ran {
            code,
            log,
            missing_captures,
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
        log: &Utf8Path,
    ) -> Result<(i32, Captures)> {
        let command = self.rendered_command(branch, definition, action)?;
        let env = self.assemble_env(branch)?;

        if action.captures.is_empty() {
            let code = run_foreground(
                &["sh".to_owned(), "-c".to_owned(), command],
                &branch.workspace_path,
                &env,
                log,
            )?;
            return Ok((code, Captures::default()));
        }

        let (code, stdout) = run_captured(&command, &branch.workspace_path, &env, log)?;
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
        for name in &self.resource_order {
            if let Some(binding) = branch.resources.get(name) {
                for (key, value) in &binding.resolved_exports {
                    env.insert(key.clone(), value.clone());
                }
            }
        }

        // Layer 2: port env vars.
        for name in &self.resource_order {
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
        let path = self.store.write_resource_file(name, template.contents)?;

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
                companions_created.push(
                    self.store
                        .write_resource_file(companion.name, companion.contents)?,
                );
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

        let lane = self.lane(&definition.name);
        let capture = lane.capture(&branch.workspace_path, definition)?;

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

        // Restoring never loses state: current content is captured first.
        let safety = lane.capture(&branch.workspace_path, definition)?;
        let safety_rev = (safety.rev != target_rev).then_some(safety.rev);

        let files = lane.restore(&branch.workspace_path, definition, &target_rev)?;

        branch.trackers.insert(
            definition.name.clone(),
            TrackerBinding {
                definition_rev: definition.definition_rev.clone(),
                content_rev: Some(target_rev.clone()),
            },
        );
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;

        Ok(RestoreReport {
            rev: target_rev,
            files,
            safety_rev,
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

        let outcome = self.bind_tracker(&mut branch, definition, true)?;
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
                                    "running".to_owned()
                                } else if definition.has_long_running_action()
                                    && binding.status == ResourceStatus::Ready
                                {
                                    "stopped".to_owned()
                                } else {
                                    match binding.status {
                                        ResourceStatus::Pending => "pending".to_owned(),
                                        ResourceStatus::Ready => "ready".to_owned(),
                                        ResourceStatus::Failed => "failed".to_owned(),
                                        ResourceStatus::Blocked => "blocked".to_owned(),
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
                Ok(InstanceReport {
                    branch,
                    workspace_exists,
                    live_rev,
                    trackers,
                    resources,
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
    fn run_cleanup_hooks(
        &self,
        branch: &BranchInstance,
        dry_run: bool,
    ) -> Result<Vec<HookOutcome>> {
        // The workspace is usually still here; when cleanup is finishing an
        // instance whose workspace is already gone, hooks run from the store
        // root so an external teardown can still reach its own API.
        let cwd = if branch.workspace_path.is_dir() {
            branch.workspace_path.clone()
        } else {
            self.store.paths().project_root.clone()
        };

        let mut outcomes = Vec::new();
        for name in self.resource_order.iter().rev() {
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
    /// recorded. Deposited content resolves to its path, like restore.
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
            {
                let resolved = state
                    .state_path
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| state.state_ref.clone());
                if resolved.is_some() {
                    return Ok(resolved);
                }
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

        // Dead process state: PID files whose group exited, and state
        // directories belonging to no live instance.
        for branch in &live {
            outcome
                .dead_state
                .extend(self.supervisor(branch).prune_dead_pids(dry_run)?);
        }
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

        Ok(outcome)
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

        for file in &plan.files {
            copy_file(&workspace.join(&file.path), &destination.join(&file.path))?;
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

    fn checkpoint_branch(
        &self,
        branch: &mut BranchInstance,
        message: Option<&str>,
        reason: CheckpointReason,
    ) -> Result<CheckpointOutcome> {
        self.require_workspace(branch)?;
        let mut warnings = Vec::new();
        let checkpoint_log = self.checkpoint_log(branch);
        let id = checkpoint_log.next_id()?;
        let workspace = branch.workspace_path.clone();

        // Source: the committed state plus a dangling commit for anything
        // uncommitted, fetched into the store. The store learns about
        // workspace commits only here — checkpoint is the blessing boundary,
        // and a checkpoint protects the worktree as it stands, not just what
        // the agent remembered to commit.
        let head_rev = GitSource::workspace_head(&workspace)?;
        let dirty_rev = GitSource::workspace_dirty_commit(
            &workspace,
            &format!("newgit {id}: uncommitted state of `{}`", branch.name),
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
        let mut resource_states = Vec::new();
        let mut deposits: Vec<(String, String)> = Vec::new();
        for name in self.resource_order.iter().rev() {
            let Some(binding) = branch.resources.get(name) else {
                continue;
            };
            let definition = self.resource_definition(name)?;
            let was_running = self.supervisor(branch).running_pid(name).is_some();
            let captured = self.checkpoint_resource(branch, definition)?;
            if let Some(deposit) = captured.deposit {
                deposits.push(deposit);
            }
            resource_states.push(ResourceState {
                name: name.clone(),
                definition_rev: definition.definition_rev.clone(),
                mode: captured.mode,
                state_ref: captured.state_ref,
                state_path: captured.state_path,
                was_running,
                resolved_ports: binding.resolved_ports.clone(),
                resolved_exports: binding.resolved_exports.clone(),
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
                let files = collect_files(&branch.workspace_path, &spec.paths)?;
                let rev = content_rev(&files)?;
                Ok(CapturedResource {
                    mode: "hash".to_owned(),
                    state_ref: Some(format!("hash:{rev}")),
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
                    return Err(NewgitError::InvalidDefinition {
                        tracker: definition.name.clone(),
                        reason: format!(
                            "checkpoint `into_tracker = \"{tracker}\"` names a tracker that is \
                             not defined; create it with `newgit tracker create {tracker}`"
                        ),
                    });
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
                let (code, stdout) = run_captured(&command, &branch.workspace_path, &env, &log)?;
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
    pub fn undo(&self, instance: &str, to: Option<&str>) -> Result<UndoOutcome> {
        self.require_resolvable_graph()?;
        let mut branch = self.store.find_branch(instance)?;
        self.require_workspace(&branch)?;
        let checkpoint_log = self.checkpoint_log(&branch);
        let restored = match to {
            Some(id) => checkpoint_log.load(id)?,
            None => checkpoint_log.latest()?,
        };

        let safety = self.checkpoint_branch(
            &mut branch,
            Some(&format!("state before undo to {}", restored.id)),
            CheckpointReason::BeforeUndo,
        )?;
        let mut warnings = safety.warnings.clone();

        // Nothing may keep running while content changes underneath it.
        let supervisor = self.supervisor(&branch);
        for definition in &self.resources {
            if supervisor.running_pid(&definition.name).is_some() {
                supervisor.stop(&definition.name, &definition.stop_signal())?;
            }
        }

        // Source: content back exactly, uncommitted state uncommitted again.
        let workspace = branch.workspace_path.clone();
        GitSource::workspace_fetch_ref(&workspace, self.source.root(), &restored.source.store_ref)?;
        GitSource::workspace_restore_to(
            &workspace,
            &restored.source.head_rev,
            restored.source.dirty_rev.as_deref(),
        )?;
        // The pre-undo tip stays reachable from the safety checkpoint's ref,
        // so moving the branch back to it is expected, not divergence.
        self.bless_store_branch(
            &mut branch,
            &restored.source.head_rev,
            Some(&safety.record.source.head_rev),
            &mut warnings,
        )?;

        // Trackers: plain content, restored exactly; should not partially
        // fail in interesting ways, so failures here are hard errors.
        let mut trackers = Vec::new();
        for state in &restored.tracker_states {
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

        // Resources: dependencies before dependents, restarting what was
        // running. Failures are collected into a recovery record, not fatal.
        let mut resources = Vec::new();
        let mut failures: Vec<RestoreFailure> = Vec::new();
        for name in &self.resource_order {
            let Some(state) = restored
                .resource_states
                .iter()
                .find(|state| &state.name == name)
            else {
                continue;
            };
            if !branch.resources.contains_key(name) {
                continue;
            }
            let definition = self.resource_definition(name)?;
            if definition.definition_rev != state.definition_rev {
                warnings.push(format!(
                    "resource `{name}` definition changed since the checkpoint; restored with \
                     the current definition"
                ));
            }

            let (mut action_label, ok) = self.restore_resource(
                &mut branch,
                definition,
                state,
                &mut failures,
                &mut warnings,
            )?;
            if let Some(binding) = branch.resources.get_mut(name) {
                binding.status = if ok {
                    ResourceStatus::Ready
                } else {
                    ResourceStatus::Failed
                };
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
    fn restore_resource(
        &self,
        branch: &mut BranchInstance,
        definition: &ResourceDefinition,
        state: &ResourceState,
        failures: &mut Vec<RestoreFailure>,
        warnings: &mut Vec<String>,
    ) -> Result<(String, bool)> {
        let Some(spec) = &definition.restore else {
            return Ok(("none".to_owned(), true));
        };
        match spec.mode {
            RestoreMode::None => Ok(("none".to_owned(), true)),
            RestoreMode::External => Ok(("external (no-op)".to_owned(), true)),
            RestoreMode::Recompute => {
                let action_name = spec.recompute_action();
                let action = definition.actions.get(action_name).ok_or_else(|| {
                    NewgitError::UnknownAction {
                        resource: definition.name.clone(),
                        action: action_name.to_owned(),
                    }
                })?;
                let log = self
                    .store
                    .action_log_path(&branch.slug, &format!("{}.restore", definition.name));
                let (code, captured) = self.run_one_shot(branch, definition, action, &log)?;
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
                Ok((format!("recompute({action_name})"), ok))
            }
            RestoreMode::Command => {
                let template = spec.command.as_deref().expect("validated at parse time");
                let binding = branch.resources.get(&definition.name);
                let state_ref = state
                    .state_path
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| state.state_ref.clone());
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
                let (code, _) = run_captured(&command, &branch.workspace_path, &env, &log)?;
                let ok = code == 0;
                if !ok {
                    failures.push(RestoreFailure {
                        resource: definition.name.clone(),
                        detail: format!("restore command exited with {code}"),
                        log: Some(log),
                        retry_with: "repair the resource, then `newgit undo` again".to_owned(),
                    });
                }
                Ok(("command".to_owned(), ok))
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
