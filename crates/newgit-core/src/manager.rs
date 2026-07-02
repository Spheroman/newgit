use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;

use crate::branch::{
    BranchInstance, ResourceBinding, ResourceStatus, TrackerBinding, branch_slug, validate_name,
};
use crate::config::ProjectConfig;
use crate::error::{NewgitError, Result};
use crate::exports::{RenderContext, render};
use crate::lane::TrackerLane;
use crate::materializer::{Materializer, RealDirMaterializer};
use crate::ports;
use crate::resource::{ResourceDefinition, topological_order};
use crate::source::GitSource;
use crate::store::MetadataStore;
use crate::supervisor::{StopOutcome, Supervisor, run_foreground};
use crate::templates::resource_template;
use crate::tracker::{Storage, TrackerDefinition, collect_owned_files};

/// Orchestrates branch-instance lifecycle against one store.
#[derive(Debug)]
pub struct BranchManager {
    store: MetadataStore,
    config: ProjectConfig,
    source: GitSource,
    trackers: Vec<TrackerDefinition>,
    resources: Vec<ResourceDefinition>,
    /// Resource names, dependencies before dependents.
    resource_order: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    pub rev: String,
    pub files: usize,
    /// False when the content was identical to the previous binding.
    pub changed: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResourceOutcome {
    pub path: Utf8PathBuf,
    /// Companion definitions created because the template depends on them.
    pub companions_created: Vec<Utf8PathBuf>,
}

impl BranchManager {
    pub fn open(store: MetadataStore) -> Result<Self> {
        store.ensure_initialized()?;
        let config = store.load_config()?;
        let source = GitSource::open(&store.paths().project_root, config.project.source)?;
        let trackers = store.load_tracker_definitions()?;
        let resources = store.load_resource_definitions()?;
        let tracker_names: BTreeSet<String> = trackers.iter().map(|t| t.name.clone()).collect();
        let resource_order = topological_order(&resources, &tracker_names)?;
        Ok(Self {
            store,
            config,
            source,
            trackers,
            resources,
            resource_order,
        })
    }

    pub fn store(&self) -> &MetadataStore {
        &self.store
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
                ports: &resolved_ports,
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
                        let code = self.run_one_shot(branch, definition, action, &log)?;
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
            });
        }
        branch.updated_at = Utc::now();
        Ok(outcomes)
    }

    /// Run `<resource>.<action>` for an instance.
    pub fn run_action(&self, instance: &str, spec: &str) -> Result<ActionOutcome> {
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

        let code = self.run_one_shot(&branch, definition, action, &log)?;
        if action_name == "prepare"
            && let Some(binding) = branch.resources.get_mut(&definition.name)
        {
            binding.status = if code == 0 {
                ResourceStatus::Ready
            } else {
                ResourceStatus::Failed
            };
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch)?;
        }
        Ok(ActionOutcome::Ran { code, log })
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

    fn run_one_shot(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        action: &crate::resource::ActionSpec,
        log: &Utf8Path,
    ) -> Result<i32> {
        let command = self.rendered_command(branch, definition, action)?;
        let env = self.assemble_env(branch)?;
        run_foreground(
            &["sh".to_owned(), "-c".to_owned(), command],
            &branch.workspace_path,
            &env,
            log,
        )
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
        let empty = BTreeMap::new();
        let resource_ports = branch
            .resources
            .get(&definition.name)
            .map(|binding| &binding.resolved_ports)
            .unwrap_or(&empty);
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            ports: resource_ports,
        };
        Ok(render(&command, &context))
    }

    /// The layered environment `newgit run` and actions see. Later layers
    /// win: resource exports in dependency order → port env vars → newgit
    /// context vars. Trackers own content; command environment wiring lives
    /// outside the tracker primitive.
    pub fn assemble_env(&self, branch: &BranchInstance) -> Result<Vec<(String, String)>> {
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

        Ok(AddResourceOutcome {
            path,
            companions_created,
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

        Ok(TrackPathsOutcome {
            path,
            added_paths: paths.to_vec(),
            ignored_patterns: patterns,
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
    pub fn remove(&self, name: &str, cwd: &Utf8Path) -> Result<RemoveOutcome> {
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
        let state_dir = self.store.instance_state_dir(&branch.slug);
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir)
                .map_err(|source| NewgitError::io(state_dir, source))?;
        }

        RealDirMaterializer.remove(&branch)?;
        let archived_record = self.store.archive_branch_record(&branch)?;

        Ok(RemoveOutcome {
            branch,
            archived_record,
        })
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
