use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::branch::BranchInstance;
use crate::config::{ProjectConfig, SourceSubstrate};
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;
use crate::installs::InstallStore;
use crate::lock::InstanceLock;
use crate::materializer::{WorkspaceMarker, create_dir_all};
use crate::resource::ResourceDefinition;
use crate::tracker::{TrackerDefinition, resolve_ownership};

const LOCAL_GITIGNORE: &str = "\
# newgit local state — never committed
/local/
/branches/
/logs/
/state/
/checkpoints/
/installs/
";

const SCRIPTS_README: &str = "\
# .newgit/scripts/

Scripts that resource definitions shell out to. Reference one as
`{{scripts}}/<name>` in any resource command:

```toml
[actions.prepare]
command = \"{{scripts}}/db-up.sh {{branch.slug}}\"
```

`{{scripts}}` resolves to this directory in the **store** — the repository
you ran `newgit init` in — not to a copy inside the workspace. That is the
same rule the resource definitions in `../resources/` already follow, so both
halves of a definition live under one rule: edit either one and the next
`newgit action` picks it up, with nothing to commit first.

Commit this directory. It is control plane, like `config.toml`, `trackers/`,
and `resources/`, and a teammate or CI without these scripts cannot bind
your resources.

Scripts run with the workspace as their working directory, so a relative
path inside one refers to the instance being prepared. Mark them executable
(`chmod +x`), or invoke them through an interpreter in the command.

A script your *project* owns — something the app itself runs — belongs in the
project tree as usual, not here. Those are read from the workspace and must
be committed before the first spawn that calls them.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataStore {
    paths: NewgitPaths,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewgitPaths {
    pub project_root: Utf8PathBuf,
    pub metadata_root: Utf8PathBuf,
    pub config: Utf8PathBuf,
    pub branches: Utf8PathBuf,
    pub archived_branches: Utf8PathBuf,
    pub trackers: Utf8PathBuf,
    pub resources: Utf8PathBuf,
    /// Where `newgit export --remote` pushes, one target per file.
    pub exports: Utf8PathBuf,
    /// Scripts a resource's commands shell out to, resolved from the store
    /// rather than a workspace. Part of the committed control plane.
    pub scripts: Utf8PathBuf,
    pub local: Utf8PathBuf,
    /// Every tracker's history: a bare Git repo of tracker branches. Under `local/`,
    /// which every project's `.newgit/.gitignore` already ignores — lane
    /// content can be secrets and must never be committable.
    pub tracker_store: Utf8PathBuf,
    /// Tracker revs materialized for commands that take a path. A cache.
    pub tracker_checkouts: Utf8PathBuf,
    /// The file [`MetadataStore::lock_ports`] locks. Always empty: only the
    /// lock on it means anything.
    pub ports_lock: Utf8PathBuf,
    /// The file [`MetadataStore::lock_fresh_base`] locks, the same way.
    pub fresh_base_lock: Utf8PathBuf,
    pub logs: Utf8PathBuf,
    pub state: Utf8PathBuf,
    pub checkpoints: Utf8PathBuf,
    /// Built trees shared between instances, keyed by resource identity.
    /// Local and rebuildable — never committed, and safe to delete.
    pub installs: Utf8PathBuf,
}

/// Where a newgit command is standing: which store owns the metadata, and —
/// when inside a workspace — which branch instance the cwd belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub store: MetadataStore,
    pub current_branch: Option<String>,
}

impl MetadataStore {
    pub fn at(project_root: impl Into<Utf8PathBuf>) -> Self {
        Self {
            paths: NewgitPaths::new(project_root.into()),
        }
    }

    pub fn init(
        project_root: impl Into<Utf8PathBuf>,
        project_name: &str,
        source: SourceSubstrate,
    ) -> Result<Self> {
        let store = Self::at(project_root);
        if store.paths.config.exists() {
            return Err(NewgitError::AlreadyExists(store.paths.config.clone()));
        }
        store.create_layout()?;
        let config = ProjectConfig::new(project_name, source);
        store.write_toml(&store.paths.config, "project config", &config)?;
        store.init_root()?;
        Ok(store)
    }

    /// Take up a `.newgit/` that arrived with the project's history (a clone):
    /// its committed definitions are kept, and only the local state around
    /// them is made.
    pub fn adopt(project_root: impl Into<Utf8PathBuf>) -> Result<Self> {
        let store = Self::at(project_root);
        store.ensure_initialized()?;
        store.create_layout()?;
        Ok(store)
    }

    /// `newgit init` where `.newgit/` already is: one made before trackers
    /// had histories, or one that arrived with a plain `git clone`. Writes
    /// whichever built-in definitions are missing and gives root the
    /// project's history if it has none — what `init` would have done.
    /// Returns what it did; nothing, run again.
    pub fn adopt_in_place(project_root: impl Into<Utf8PathBuf>) -> Result<(Self, Vec<String>)> {
        let store = Self::adopt(project_root)?;
        let mut done = Vec::new();
        for definition in [
            TrackerDefinition::root(),
            TrackerDefinition::project(),
            TrackerDefinition::access(),
        ] {
            let path = store
                .paths
                .trackers
                .join(format!("{}.toml", definition.name));
            if !path.exists() {
                store.create_tracker_definition(&definition)?;
                done.push(format!("wrote .newgit/trackers/{}.toml", definition.name));
            }
        }
        let seeded = std::process::Command::new("git")
            .arg(format!("--git-dir={}", store.paths.tracker_store))
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/trackers/root/main",
            ])
            .output()
            .is_ok_and(|output| output.status.success());
        // A fresh `init` vouches for the history it finds: nothing could have
        // been secret yet. Here secret trackers may already be defined, and
        // unpublished commits may touch their paths, so the secret gate keeps
        // checking every commit unless none are.
        let vouch = !store
            .load_tracker_definitions()?
            .iter()
            .any(|definition| definition.secret);
        if !seeded && store.adopt_root_history(vouch)? {
            done.push("gave `root` this project's history as its `main`".to_owned());
        }
        Ok((store, done))
    }

    /// Write the root tracker's definition and give it the project's history
    /// as its `main`: a copy of the repository's current branch. A repository
    /// with no commits yet gives root no history.
    fn init_root(&self) -> Result<()> {
        let root = TrackerDefinition::root();
        self.create_tracker_definition(&root)?;
        self.create_tracker_definition(&TrackerDefinition::project())?;
        self.create_tracker_definition(&TrackerDefinition::access())?;
        self.adopt_root_history(true).map(drop)
    }

    /// Root's `main` becomes the repository's current branch, history and
    /// all. False when there are no commits to give it. `vouch` marks that
    /// history published, so the secret gate does not check it again.
    fn adopt_root_history(&self, vouch: bool) -> Result<bool> {
        let root = TrackerDefinition::root();
        let project = self.paths.project_root.as_str();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(project)
                .args(args)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        if git(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_none() {
            return Ok(false);
        }
        // The project root is the instance `main`, and this is its branch:
        // what it has checked out later (a feature branch, an old tag) is
        // not its history.
        if let Some(branch) = git(&["symbolic-ref", "--short", "-q", "HEAD"]) {
            let file = self.paths.project_branch_file();
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|source| NewgitError::io(parent, source))?;
            }
            std::fs::write(&file, format!("{branch}\n"))
                .map_err(|source| NewgitError::io(&file, source))?;
        }
        crate::tracker_repo::import_history(
            &self.paths.tracker_store,
            &self.paths.project_root,
            "HEAD",
            &root.name,
        )?;
        // The history newgit adopted: published already, wherever it went,
        // so the secret gate never holds it against a push.
        if vouch {
            git(&["update-ref", crate::manager::ADOPTED_REF, "HEAD"]);
        }
        Ok(true)
    }

    /// Walk upward from `cwd` to find the governing metadata. A workspace is
    /// recognized by its gitignored marker and resolves to the store it was
    /// cloned from; a directory with a committed `.newgit/config.toml` and no
    /// marker is the store itself.
    pub fn discover(cwd: &Utf8Path) -> Result<Context> {
        let mut dir = Some(cwd);
        while let Some(current) = dir {
            let metadata_root = current.join(".newgit");
            if metadata_root.is_dir() {
                let marker_path = metadata_root.join("local/instance.toml");
                if marker_path.is_file() {
                    let marker: WorkspaceMarker = read_toml_at(&marker_path)?;
                    let store = Self::at(marker.store_root);
                    store.ensure_initialized()?;
                    return Ok(Context {
                        store,
                        current_branch: Some(marker.branch),
                    });
                }
                if metadata_root.join("config.toml").is_file() {
                    return Ok(Context {
                        store: Self::at(current),
                        current_branch: None,
                    });
                }
            }
            dir = current.parent();
        }
        Err(NewgitError::MissingMetadata(cwd.to_path_buf()))
    }

    pub fn paths(&self) -> &NewgitPaths {
        &self.paths
    }

    pub fn ensure_initialized(&self) -> Result<()> {
        if self.paths.metadata_root.is_dir() && self.paths.config.is_file() {
            Ok(())
        } else {
            Err(NewgitError::MissingMetadata(
                self.paths.metadata_root.clone(),
            ))
        }
    }

    pub fn load_config(&self) -> Result<ProjectConfig> {
        let config: ProjectConfig = read_toml_at(&self.paths.config)?;
        if let Some(problem) = config.remote.as_ref().and_then(|remote| remote.problem()) {
            return Err(NewgitError::InvalidDefinition {
                kind: "config section",
                name: "[remote]".to_owned(),
                reason: problem,
            });
        }
        Ok(config)
    }

    pub fn write_config(&self, config: &ProjectConfig) -> Result<()> {
        self.write_toml(&self.paths.config, "project config", config)
    }

    pub fn branch_record_path(&self, slug: &str) -> Utf8PathBuf {
        self.paths.branches.join(format!("{slug}.toml"))
    }

    /// Write a brand-new binding record; fails on slug collision.
    pub fn create_branch_record(&self, branch: &BranchInstance) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.branches)?;
        let path = self.branch_record_path(&branch.slug);
        if path.exists() {
            return Err(NewgitError::BranchInstanceExists {
                name: branch.name.clone(),
                path,
            });
        }
        self.write_toml(&path, &format!("branch `{}`", branch.name), branch)?;
        Ok(path)
    }

    /// Take `slug`'s lock (see [`crate::lock`]), waiting for any other
    /// process that holds it. Commands go through
    /// `BranchManager::lock_instance`, which also hands a nested call the
    /// lock the manager already holds.
    pub fn lock_instance(&self, slug: &str, name: &str) -> Result<InstanceLock> {
        InstanceLock::acquire(&self.locks_dir(), slug, name)
    }

    /// Where instance lock files live: `.newgit/local/locks/<slug>.lock`.
    pub fn locks_dir(&self) -> Utf8PathBuf {
        self.paths.local.join("locks")
    }

    /// Overwrite an existing binding record (e.g. after a tracker capture).
    pub fn save_branch_record(&self, branch: &BranchInstance) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.branches)?;
        let path = self.branch_record_path(&branch.slug);
        self.write_toml(&path, &format!("branch `{}`", branch.name), branch)?;
        Ok(path)
    }

    /// Take the store's port lock, held until the returned file is dropped.
    ///
    /// It covers one thing: choosing a resource's ports and writing them into
    /// the instance's binding record (`BranchManager::claim_ports`, the only
    /// caller). The records are the only claim on a port, so two processes
    /// that each read them before the other had written would both be handed
    /// the same one, and those may be spawning different instances: this is
    /// one lock for the whole store, not one per instance.
    ///
    /// On Linux and macOS `File::lock` is `flock(2)`, which locks the open
    /// file rather than the process: two managers in one process exclude
    /// each other the way two processes do, and the kernel drops the lock
    /// with the last descriptor. A newgit that crashes or is killed while
    /// holding it lets go as it dies, and the file it leaves behind locks
    /// nothing.
    pub fn lock_ports(&self) -> Result<std::fs::File> {
        self.lock_store_file(&self.paths.ports_lock)
    }

    /// Take the store's fresh-base lock, held until the returned file is
    /// dropped: the same kind of lock as [`MetadataStore::lock_ports`].
    ///
    /// It covers choosing the base a Claude Code worktree starts from
    /// ([`crate::fresh_base`]), which may fetch the project branch's
    /// upstream. Subagents launched together run their hooks at once; under
    /// this lock the first fetches and the rest find that fetch fresh and
    /// read the ref it left, so they all start from the same commit.
    pub fn lock_fresh_base(&self) -> Result<std::fs::File> {
        self.lock_store_file(&self.paths.fresh_base_lock)
    }

    fn lock_store_file(&self, path: &Utf8Path) -> Result<std::fs::File> {
        create_dir_all(&self.paths.local)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|source| NewgitError::io(path, source))?;
        file.lock()
            .map_err(|source| NewgitError::io(path, source))?;
        Ok(file)
    }

    /// Tracker definitions, one per file in `.newgit/trackers/`; the name
    /// comes from the filename. Ownership is resolved across all of them: a
    /// path belongs to its most specific claimant.
    pub fn load_tracker_definitions(&self) -> Result<Vec<TrackerDefinition>> {
        self.ensure_initialized()?;
        let mut definitions = Vec::new();
        for entry in read_dir_sorted(&self.paths.trackers)? {
            if entry.extension() != Some("toml") {
                continue;
            }
            let Some(name) = entry.file_stem() else {
                continue;
            };
            definitions.push(TrackerDefinition::from_file(name, &entry)?);
        }
        resolve_ownership(&mut definitions)?;
        Ok(definitions)
    }

    pub fn create_tracker_definition(&self, definition: &TrackerDefinition) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.trackers)?;
        let path = self
            .paths
            .trackers
            .join(format!("{}.toml", definition.name));
        if path.exists() {
            return Err(NewgitError::AlreadyExists(path));
        }
        self.write_toml(
            &path,
            &format!("tracker `{}`", definition.name),
            &definition.to_file(),
        )?;
        Ok(path)
    }

    pub fn save_tracker_definition(&self, definition: &TrackerDefinition) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.trackers)?;
        let path = self
            .paths
            .trackers
            .join(format!("{}.toml", definition.name));
        self.write_toml(
            &path,
            &format!("tracker `{}`", definition.name),
            &definition.to_file(),
        )?;
        Ok(path)
    }

    /// Resource definitions, one per file in `.newgit/resources/`.
    pub fn load_resource_definitions(&self) -> Result<Vec<ResourceDefinition>> {
        self.ensure_initialized()?;
        let mut definitions = Vec::new();
        for entry in read_dir_sorted(&self.paths.resources)? {
            if entry.extension() != Some("toml") {
                continue;
            }
            let Some(name) = entry.file_stem() else {
                continue;
            };
            definitions.push(ResourceDefinition::from_file(name, &entry)?);
        }
        let targets: Vec<(&str, &crate::render::RenderSpec)> = definitions
            .iter()
            .flat_map(|definition| {
                definition
                    .render
                    .iter()
                    .map(move |spec| (definition.name.as_str(), spec))
            })
            .collect();
        crate::render::validate_disjoint(&targets)?;
        Ok(definitions)
    }

    /// Export targets, one per file in `.newgit/exports/`.
    pub fn load_export_targets(&self) -> Result<Vec<ExportTarget>> {
        self.ensure_initialized()?;
        let mut targets = Vec::new();
        for entry in read_dir_sorted(&self.paths.exports)? {
            if entry.extension() != Some("toml") {
                continue;
            }
            let Some(name) = entry.file_stem() else {
                continue;
            };
            targets.push(ExportTarget::from_file(name, &entry)?);
        }
        Ok(targets)
    }

    pub fn write_resource_file(&self, name: &str, contents: &str) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.resources)?;
        let path = self.paths.resources.join(format!("{name}.toml"));
        if path.exists() {
            return Err(NewgitError::AlreadyExists(path));
        }
        std::fs::write(&path, contents).map_err(|source| NewgitError::io(&path, source))?;
        Ok(path)
    }

    /// Delete a resource definition file. `newgit resource remove`'s caller is
    /// responsible for the dependents/bound-instance checks; this is the
    /// mechanical last step once those have cleared.
    pub fn delete_resource_definition(&self, name: &str) -> Result<Utf8PathBuf> {
        let path = self.paths.resources.join(format!("{name}.toml"));
        std::fs::remove_file(&path).map_err(|source| NewgitError::io(&path, source))?;
        Ok(path)
    }

    /// Delete a tracker definition file. See [`Self::delete_resource_definition`].
    pub fn delete_tracker_definition(&self, name: &str) -> Result<Utf8PathBuf> {
        let path = self.paths.trackers.join(format!("{name}.toml"));
        std::fs::remove_file(&path).map_err(|source| NewgitError::io(&path, source))?;
        Ok(path)
    }

    /// `path` relative to the repository root, for display. Falls back to the
    /// absolute path when `path` does not live under the root at all — better
    /// to print something true than to fail a report over it.
    pub fn relative_to_root(&self, path: &Utf8Path) -> Utf8PathBuf {
        path.strip_prefix(&self.paths.project_root)
            .map(Utf8Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf())
    }

    pub fn instance_state_dir(&self, slug: &str) -> Utf8PathBuf {
        self.paths.state.join(slug)
    }

    /// The shared store of built trees, at `.newgit/installs/`.
    ///
    /// The local-ignore rule is ensured here rather than only at `init`,
    /// because this is the moment the directory can first come to exist: a
    /// store written under an `.newgit/.gitignore` that predates it would
    /// put several gigabytes of `node_modules` into `git status`.
    pub fn install_store(&self) -> InstallStore {
        let _ = self.ensure_installs_ignored();
        InstallStore::at(self.paths.installs.clone())
    }

    fn ensure_installs_ignored(&self) -> Result<()> {
        let path = self.paths.metadata_root.join(".gitignore");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        if contents.lines().any(|line| line.trim() == "/installs/") {
            return Ok(());
        }
        let mut updated = contents;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("/installs/\n");
        std::fs::write(&path, updated).map_err(|source| NewgitError::io(path, source))
    }

    /// Per-instance checkpoint records: `.newgit/checkpoints/<slug>/`.
    pub fn checkpoint_dir(&self, slug: &str) -> Utf8PathBuf {
        self.paths.checkpoints.join(slug)
    }

    /// Every slug with a checkpoint directory, including instances whose
    /// binding record has been archived. Checkpoints outlive removal, so
    /// snapshot pruning has to consult all of them, not just live records.
    pub fn checkpointed_slugs(&self) -> Result<Vec<String>> {
        Ok(read_subdirs_sorted(&self.paths.checkpoints)?
            .iter()
            .filter_map(|dir| dir.file_name().map(ToOwned::to_owned))
            .collect())
    }

    /// Per-instance runtime state directories that exist on disk.
    pub fn state_dirs(&self) -> Result<Vec<Utf8PathBuf>> {
        read_subdirs_sorted(&self.paths.state)
    }

    /// Timestamped log path for one action run.
    pub fn action_log_path(&self, slug: &str, label: &str) -> Utf8PathBuf {
        let now = Utc::now();
        self.paths.logs.join(slug).join(format!(
            "{label}-{}-{:09}Z.log",
            now.format("%Y%m%dT%H%M%S"),
            now.timestamp_subsec_nanos()
        ))
    }

    /// Append patterns to the store repo's .gitignore under a labeled block.
    pub fn append_gitignore(&self, label: &str, patterns: &[String]) -> Result<()> {
        if patterns.is_empty() {
            return Ok(());
        }
        let path = self.paths.project_root.join(".gitignore");
        let existing = if path.exists() {
            std::fs::read_to_string(&path).map_err(|source| NewgitError::io(&path, source))?
        } else {
            String::new()
        };
        let mut updated = existing.clone();
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&format!("\n# newgit tracker: {label}\n"));
        for pattern in patterns {
            updated.push_str(pattern);
            updated.push('\n');
        }
        std::fs::write(&path, updated).map_err(|source| NewgitError::io(&path, source))
    }

    /// Reverse [`Self::append_gitignore`]: drop every `# newgit tracker:
    /// <label>` block from the store repo's `.gitignore`, plus the blank
    /// separator line `append_gitignore` puts in front of each one.
    ///
    /// `tracker track` can be called more than once for the same tracker, and
    /// each call appends its own block — so this removes every occurrence,
    /// not just the first. Returns the pattern lines removed, for reporting.
    pub fn remove_gitignore_block(&self, label: &str) -> Result<Vec<String>> {
        let path = self.paths.project_root.join(".gitignore");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents =
            std::fs::read_to_string(&path).map_err(|source| NewgitError::io(&path, source))?;
        let header = format!("# newgit tracker: {label}");

        let mut kept: Vec<&str> = Vec::new();
        let mut removed = Vec::new();
        let mut lines = contents.lines().peekable();
        while let Some(line) = lines.next() {
            if line == header {
                // The block runs until the next blank line, comment, or EOF —
                // exactly the shape `append_gitignore` writes: a header
                // followed by nothing but pattern lines.
                while let Some(next) = lines.peek() {
                    if next.is_empty() || next.starts_with('#') {
                        break;
                    }
                    removed.push((*next).to_owned());
                    lines.next();
                }
                // Drop the blank separator `append_gitignore` put in front of
                // the header, so removing every block leaves no extra gaps.
                if kept.last() == Some(&"") {
                    kept.pop();
                }
                continue;
            }
            kept.push(line);
        }

        if removed.is_empty() {
            return Ok(removed);
        }

        let mut updated = kept.join("\n");
        if !updated.is_empty() {
            updated.push('\n');
        }
        std::fs::write(&path, updated).map_err(|source| NewgitError::io(&path, source))?;
        Ok(removed)
    }

    pub fn load_branches(&self) -> Result<Vec<BranchInstance>> {
        self.ensure_initialized()?;
        let mut branches: Vec<BranchInstance> = Vec::new();

        for entry in read_dir_sorted(&self.paths.branches)? {
            if entry.extension() == Some("toml") {
                branches.push(read_toml_at(&entry)?);
            }
        }

        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(branches)
    }

    /// Look an instance up by name or slug.
    pub fn find_branch(&self, name: &str) -> Result<BranchInstance> {
        self.load_branches()?
            .into_iter()
            .find(|branch| branch.name == name || branch.slug == name)
            .ok_or_else(|| NewgitError::UnknownBranchInstance(name.to_owned()))
    }

    /// The binding record outlives the workspace: removal archives it rather
    /// than deleting it.
    pub fn archive_branch_record(&self, branch: &BranchInstance) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.archived_branches)?;
        let record = self.branch_record_path(&branch.slug);
        let archived = self.paths.archived_branches.join(format!(
            "{}-{}.toml",
            branch.slug,
            Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        std::fs::rename(&record, &archived).map_err(|source| NewgitError::io(record, source))?;
        Ok(archived)
    }

    fn create_layout(&self) -> Result<()> {
        for path in [
            &self.paths.metadata_root,
            &self.paths.branches,
            &self.paths.trackers,
            &self.paths.resources,
            &self.paths.scripts,
            &self.paths.local,
            &self.paths.logs,
            &self.paths.state,
            &self.paths.checkpoints,
            &self.paths.installs,
        ] {
            create_dir_all(path)?;
        }

        let gitignore = self.paths.metadata_root.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, LOCAL_GITIGNORE)
                .map_err(|source| NewgitError::io(gitignore, source))?;
        }

        // Git does not track empty directories, so `scripts/` needs a file to
        // survive a commit and reach a clone. Make that file explain itself.
        let scripts_readme = self.paths.scripts.join("README.md");
        if !scripts_readme.exists() {
            std::fs::write(&scripts_readme, SCRIPTS_README)
                .map_err(|source| NewgitError::io(scripts_readme, source))?;
        }
        Ok(())
    }

    fn write_toml<T>(&self, path: &Utf8Path, label: &str, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        write_toml_at(path, label, value)
    }
}

impl NewgitPaths {
    /// Names the project root's own branch (the instance `main`), written at
    /// `newgit init`.
    pub fn project_branch_file(&self) -> Utf8PathBuf {
        self.local.join("project-branch")
    }

    pub fn new(project_root: Utf8PathBuf) -> Self {
        let metadata_root = project_root.join(".newgit");
        Self {
            project_root,
            config: metadata_root.join("config.toml"),
            branches: metadata_root.join("branches"),
            archived_branches: metadata_root.join("branches/archived"),
            trackers: metadata_root.join("trackers"),
            resources: metadata_root.join("resources"),
            exports: metadata_root.join("exports"),
            scripts: metadata_root.join("scripts"),
            local: metadata_root.join("local"),
            tracker_store: metadata_root.join("local/trackers.git"),
            tracker_checkouts: metadata_root.join("local/tracker-checkouts"),
            ports_lock: metadata_root.join("local/ports.lock"),
            fresh_base_lock: metadata_root.join("local/fresh-base.lock"),
            logs: metadata_root.join("logs"),
            state: metadata_root.join("state"),
            checkpoints: metadata_root.join("checkpoints"),
            installs: metadata_root.join("installs"),
            metadata_root,
        }
    }
}

pub fn expand_home(path: &Utf8Path) -> Utf8PathBuf {
    let Some(stripped) = path.as_str().strip_prefix("~/") else {
        return path.to_path_buf();
    };

    std::env::var("HOME")
        .map(|home| Utf8PathBuf::from(home).join(stripped))
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn read_toml_at<T>(path: &Utf8Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let contents = std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
    toml::from_str(&contents).map_err(|source| NewgitError::TomlRead {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn write_toml_at<T>(path: &Utf8Path, label: &str, value: &T) -> Result<()>
where
    T: Serialize,
{
    let contents = toml::to_string_pretty(value).map_err(|source| NewgitError::TomlWrite {
        label: label.to_owned(),
        source,
    })?;
    // Written whole to a temporary file beside it, flushed to disk, then
    // renamed over it: a reader sees the old record or the new one, never
    // the truncated middle `std::fs::write` goes through, and a process or
    // machine that dies mid-write leaves the record it found (#212).
    //
    // The temporary has no `.toml` extension, so a directory listing never
    // mistakes one for a record. One is left behind only if the process is
    // killed between creating and renaming it; nothing reads it, and it is
    // safe to delete.
    //
    // A new file gets the mode `std::fs::write` would give it (0666 less the
    // umask) rather than the temporary's 0600, and an existing one keeps its
    // own.
    let dir = path.parent().unwrap_or_else(|| Utf8Path::new("."));
    let mode = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(NewgitError::io(path, source)),
    };
    let mut temp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .permissions(std::fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)
        .map_err(|source| NewgitError::io(dir, source))?;
    if let Some(mode) = mode {
        temp.as_file()
            .set_permissions(mode)
            .map_err(|source| NewgitError::io(path, source))?;
    }
    temp.write_all(contents.as_bytes())
        .map_err(|source| NewgitError::io(path, source))?;
    // Plain fsync, not the F_FULLFSYNC `File::sync_all` uses on macOS: that
    // is a cache flush of the whole drive per record, and the data reaching
    // the drive is the guarantee this is after.
    nix::unistd::fsync(temp.as_file())
        .map_err(|errno| NewgitError::io(path, std::io::Error::from(errno)))?;
    temp.persist(path)
        .map_err(|error| NewgitError::io(path, error.error))?;
    // The rename itself is durable once the directory is. Best-effort: some
    // filesystems refuse to fsync a directory, and the record is already in
    // place either way.
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = nix::unistd::fsync(&dir);
    }
    Ok(())
}

/// Immediate subdirectories, sorted. The dir-shaped counterpart to
/// [`read_dir_sorted`], for walking per-instance checkpoint dirs.
pub(crate) fn read_subdirs_sorted(path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|source| NewgitError::io(path, source))? {
        let entry = entry.map_err(|source| NewgitError::io(path, source))?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| NewgitError::NonUtf8Path(path.display().to_string()))?;
        if path.is_dir() {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

pub(crate) fn read_dir_sorted(path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|source| NewgitError::io(path, source))? {
        let entry = entry.map_err(|source| NewgitError::io(path, source))?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| NewgitError::NonUtf8Path(path.display().to_string()))?;
        if path.is_file() && path.file_name() != Some(".DS_Store") {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn store() -> (tempfile::TempDir, MetadataStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let store = MetadataStore::init(&root, "proj", SourceSubstrate::Git).expect("init");
        (temp, store)
    }

    /// A record renamed into place keeps the mode the file had, and a new
    /// one gets what `std::fs::write` would give it — not the 0600 of the
    /// temporary it was written to.
    #[test]
    fn a_record_written_atomically_keeps_the_usual_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let mode =
            |path: &Utf8Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        let plain = dir.join("plain");
        std::fs::write(&plain, "").expect("write");

        let record = dir.join("record.toml");
        write_toml_at(&record, "record", &BTreeMap::from([("a", 1)])).expect("new");
        assert_eq!(mode(&record), mode(&plain));

        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        write_toml_at(&record, "record", &BTreeMap::from([("a", 2)])).expect("rewrite");
        assert_eq!(mode(&record), 0o640);
        assert_eq!(std::fs::read_to_string(&record).expect("read"), "a = 2\n");
    }

    #[test]
    fn remove_gitignore_block_drops_every_occurrence_and_nothing_else() {
        let (_temp, store) = store();
        store
            .append_gitignore("env", &["/.env.local".to_owned()])
            .expect("append 1");
        // `tracker track` called again later appends a second block under the
        // same label.
        store
            .append_gitignore("env", &["/.env.production".to_owned()])
            .expect("append 2");
        store
            .append_gitignore("other", &["/other.secret".to_owned()])
            .expect("append other");

        let removed = store.remove_gitignore_block("env").expect("remove");
        assert_eq!(
            removed,
            vec!["/.env.local".to_owned(), "/.env.production".to_owned()]
        );

        let gitignore =
            std::fs::read_to_string(store.paths().project_root.join(".gitignore")).expect("read");
        assert!(
            !gitignore.contains("env"),
            "no trace of the env block: {gitignore}"
        );
        assert!(gitignore.contains("# newgit tracker: other"));
        assert!(gitignore.contains("/other.secret"));

        // Removing again is a no-op, not an error.
        assert!(
            store
                .remove_gitignore_block("env")
                .expect("remove again")
                .is_empty()
        );
    }

    #[test]
    fn remove_gitignore_block_is_a_no_op_without_a_gitignore() {
        let (_temp, store) = store();
        assert!(
            store
                .remove_gitignore_block("env")
                .expect("no file")
                .is_empty()
        );
    }

    #[test]
    fn relative_to_root_strips_the_project_root() {
        let (_temp, store) = store();
        let path = store.paths().trackers.join("env.toml");
        assert_eq!(store.relative_to_root(&path), ".newgit/trackers/env.toml");

        let outside = Utf8PathBuf::from("/somewhere/else.toml");
        assert_eq!(store.relative_to_root(&outside), outside);
    }
}
