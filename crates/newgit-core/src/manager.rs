use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;

use crate::branch::{BranchInstance, TrackerBinding, branch_slug, validate_name};
use crate::config::ProjectConfig;
use crate::error::{NewgitError, Result};
use crate::lane::{TrackerLane, copy_tree};
use crate::materializer::{Materializer, RealDirMaterializer};
use crate::source::GitSource;
use crate::store::MetadataStore;
use crate::templates::tracker_template;
use crate::tracker::{Propagation, TrackerDefinition, collect_owned_files};

/// Orchestrates branch-instance lifecycle against one store.
#[derive(Debug)]
pub struct BranchManager {
    store: MetadataStore,
    config: ProjectConfig,
    source: GitSource,
    trackers: Vec<TrackerDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOutcome {
    pub branch: BranchInstance,
    pub record_path: Utf8PathBuf,
    /// False when the instance attached to a pre-existing source branch.
    pub created_source_branch: bool,
    pub trackers: Vec<TrackerBindOutcome>,
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
    /// Materialized from `materialize.copy_from`.
    Template,
    /// Materialized from the lane head (`rebase` propagation).
    LaneHead,
    /// Bound with nothing to materialize.
    Nothing,
    /// The materialize source was missing; bound loudly-unbound.
    MissingSource(Utf8PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceReport {
    pub branch: BranchInstance,
    pub workspace_exists: bool,
    /// Live HEAD of the workspace clone, when it can be read.
    pub live_rev: Option<String>,
    pub trackers: Vec<TrackerReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerReport {
    pub name: String,
    /// None when the tracker is defined but this instance has no binding.
    pub content_rev: Option<String>,
    /// A `rebase` tracker whose lane head has moved past this binding.
    pub behind: bool,
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
pub struct AddTrackerOutcome {
    pub path: Utf8PathBuf,
    pub ignored_patterns: Vec<String>,
}

impl BranchManager {
    pub fn open(store: MetadataStore) -> Result<Self> {
        store.ensure_initialized()?;
        let config = store.load_config()?;
        let source = GitSource::open(&store.paths().project_root, config.project.source)?;
        let trackers = store.load_tracker_definitions()?;
        Ok(Self {
            store,
            config,
            source,
            trackers,
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
                if let Ok(false) = self.source.is_ignored(path.as_str()) {
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

        let record_path = self.store.create_branch_record(&branch)?;

        Ok(SpawnOutcome {
            branch,
            record_path,
            created_source_branch,
            trackers: tracker_outcomes,
        })
    }

    /// Materialize a tracker's initial content into an instance workspace
    /// (at spawn, or later via `newgit tracker materialize`) and record the
    /// binding. `refresh` allows rebinding an already-bound tracker.
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

        let (origin, content_rev, files) = match definition.propagation {
            Propagation::Manual => (BindOrigin::Nothing, None, 0),
            Propagation::Rebase => match lane.latest() {
                Some(rev) => {
                    let files = lane.restore(&workspace, definition, &rev)?;
                    (BindOrigin::LaneHead, Some(rev), files)
                }
                None => self.materialize_from_template(&lane, definition, &workspace)?,
            },
            Propagation::Pin => self.materialize_from_template(&lane, definition, &workspace)?,
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

    fn materialize_from_template(
        &self,
        lane: &TrackerLane,
        definition: &TrackerDefinition,
        workspace: &Utf8Path,
    ) -> Result<(BindOrigin, Option<String>, usize)> {
        let Some(spec) = &definition.materialize else {
            return Ok((BindOrigin::Nothing, None, 0));
        };
        let from = self.store.paths().project_root.join(&spec.copy_from);
        if !from.exists() {
            return Ok((BindOrigin::MissingSource(spec.copy_from.clone()), None, 0));
        }
        let Some(target) = definition.materialize_target() else {
            return Ok((BindOrigin::Nothing, None, 0));
        };
        copy_tree(&from, &workspace.join(target))?;

        // Baseline capture so the fresh content is immediately restorable;
        // a rebase lane's first content becomes the lane head.
        let capture = lane.capture(workspace, definition)?;
        if definition.propagation == Propagation::Rebase && lane.latest().is_none() {
            lane.set_latest(&capture.rev)?;
        }
        Ok((BindOrigin::Template, Some(capture.rev), capture.files))
    }

    pub fn capture_tracker(&self, instance: &str, tracker: &str) -> Result<CaptureReport> {
        let mut branch = self.store.find_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        let lane = self.lane(&definition.name);
        let capture = lane.capture(&branch.workspace_path, definition)?;
        if definition.propagation == Propagation::Rebase {
            lane.set_latest(&capture.rev)?;
        }

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

    pub fn restore_tracker(
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

    /// Pull a tracker's default content into an existing instance: lane head
    /// for `rebase`, template for `pin`. `manual` trackers only move via
    /// explicit `restore --rev`.
    pub fn materialize_tracker(&self, instance: &str, tracker: &str) -> Result<TrackerBindOutcome> {
        let mut branch = self.store.find_branch(instance)?;
        let definition = self.definition(tracker)?;
        self.require_workspace(&branch)?;

        if definition.propagation == Propagation::Manual {
            return Err(NewgitError::Unsupported(format!(
                "tracker `{}` has manual propagation; use `newgit tracker restore {} --rev <rev>`",
                definition.name, definition.name
            )));
        }

        let outcome = self.bind_tracker(&mut branch, definition, true)?;
        self.store.save_branch_record(&branch)?;
        Ok(outcome)
    }

    pub fn add_tracker(&self, name: &str, template_name: &str) -> Result<AddTrackerOutcome> {
        validate_name(name)?;
        let template = tracker_template(template_name)
            .ok_or_else(|| NewgitError::UnknownTemplate(template_name.to_owned()))?;
        let path = self.store.write_tracker_file(name, template.contents)?;

        // env-file materializes from a committed template; make sure one exists.
        if template.name == "env-file" {
            let base_env = self.store.paths().templates.join("base.env");
            if !base_env.exists() {
                std::fs::write(&base_env, "# branch-local environment\n")
                    .map_err(|source| NewgitError::io(base_env, source))?;
            }
        }

        // Enforce the invariant at the source: owned paths get gitignored now.
        let definition = TrackerDefinition::from_file(name, &path)?;
        let mut patterns = Vec::new();
        for owned in &definition.paths {
            if !self.source.is_ignored(owned.as_str())? {
                patterns.push(format!("/{owned}"));
            }
        }
        self.store.append_gitignore(name, &patterns)?;

        Ok(AddTrackerOutcome {
            path,
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
                        let behind = definition.propagation == Propagation::Rebase
                            && head.is_some()
                            && content_rev != head;
                        TrackerReport {
                            name: definition.name.clone(),
                            content_rev,
                            behind,
                        }
                    })
                    .collect();
                Ok(InstanceReport {
                    branch,
                    workspace_exists,
                    live_rev,
                    trackers,
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
