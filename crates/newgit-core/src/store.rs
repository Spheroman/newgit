use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::branch::BranchInstance;
use crate::config::{ProjectConfig, SourceSubstrate};
use crate::error::{NewgitError, Result};
use crate::materializer::{WorkspaceMarker, create_dir_all};
use crate::tracker::{TrackerDefinition, validate_disjoint};

const LOCAL_GITIGNORE: &str = "\
# newgit local state — never committed
/local/
/branches/
/snapshots/
/logs/
/state/
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
    pub templates: Utf8PathBuf,
    pub local: Utf8PathBuf,
    pub snapshots: Utf8PathBuf,
    pub logs: Utf8PathBuf,
    pub state: Utf8PathBuf,
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

    pub fn write_tracker_file(&self, name: &str, contents: &str) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.trackers)?;
        let path = self.paths.trackers.join(format!("{name}.toml"));
        if path.exists() {
            return Err(NewgitError::AlreadyExists(path));
        }
        std::fs::write(&path, contents).map_err(|source| NewgitError::io(&path, source))?;
        Ok(path)
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
            &self.paths.templates,
            &self.paths.local,
            &self.paths.snapshots,
            &self.paths.logs,
            &self.paths.state,
        ] {
            create_dir_all(path)?;
        }

        let gitignore = self.paths.metadata_root.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, LOCAL_GITIGNORE)
                .map_err(|source| NewgitError::io(gitignore, source))?;
        }
        Ok(())
    }

    fn write_toml<T>(&self, path: &Utf8Path, label: &str, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let contents = toml::to_string_pretty(value).map_err(|source| NewgitError::TomlWrite {
            label: label.to_owned(),
            source,
        })?;
        std::fs::write(path, contents).map_err(|source| NewgitError::io(path, source))
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
            templates: metadata_root.join("templates"),
            local: metadata_root.join("local"),
            snapshots: metadata_root.join("snapshots"),
            logs: metadata_root.join("logs"),
            state: metadata_root.join("state"),
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

fn read_toml_at<T>(path: &Utf8Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let contents = std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
    toml::from_str(&contents).map_err(|source| NewgitError::TomlRead {
        path: path.to_path_buf(),
        source,
    })
}

fn read_dir_sorted(path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
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
