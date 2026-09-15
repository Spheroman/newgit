use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::branch::BranchInstance;
use crate::config::{ProjectConfig, SourceSubstrate};
use crate::error::{NewgitError, Result};
use crate::installs::InstallStore;
use crate::materializer::{WorkspaceMarker, create_dir_all};
use crate::resource::ResourceDefinition;
use crate::tracker::{TrackerDefinition, validate_disjoint};

const LOCAL_GITIGNORE: &str = "\
# newgit local state — never committed
/local/
/branches/
/snapshots/
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
    /// Scripts a resource's commands shell out to, resolved from the store
    /// rather than a workspace. Part of the committed control plane.
    pub scripts: Utf8PathBuf,
    pub local: Utf8PathBuf,
    pub snapshots: Utf8PathBuf,
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
        Ok(store)
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
        read_toml_at(&self.paths.config)
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

    /// Overwrite an existing binding record (e.g. after a tracker capture).
    pub fn save_branch_record(&self, branch: &BranchInstance) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.branches)?;
        let path = self.branch_record_path(&branch.slug);
        self.write_toml(&path, &format!("branch `{}`", branch.name), branch)?;
        Ok(path)
    }

    /// Tracker definitions, one per file in `.newgit/trackers/`; the name
    /// comes from the filename. Lanes are validated as disjoint.
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
        validate_disjoint(&definitions)?;
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
            &self.paths.snapshots,
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
    pub fn new(project_root: Utf8PathBuf) -> Self {
        let metadata_root = project_root.join(".newgit");
        Self {
            project_root,
            config: metadata_root.join("config.toml"),
            branches: metadata_root.join("branches"),
            archived_branches: metadata_root.join("branches/archived"),
            trackers: metadata_root.join("trackers"),
            resources: metadata_root.join("resources"),
            scripts: metadata_root.join("scripts"),
            local: metadata_root.join("local"),
            snapshots: metadata_root.join("snapshots"),
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
    std::fs::write(path, contents).map_err(|source| NewgitError::io(path, source))
}

/// Immediate subdirectories, sorted. The dir-shaped counterpart to
/// [`read_dir_sorted`], for walking per-instance checkpoint dirs and lane
/// rev dirs.
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
    use super::*;

    fn store() -> (tempfile::TempDir, MetadataStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let store = MetadataStore::init(&root, "proj", SourceSubstrate::Git).expect("init");
        (temp, store)
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
