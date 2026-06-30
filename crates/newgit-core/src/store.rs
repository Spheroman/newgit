use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::branch::{BranchInstance, branch_slug};
use crate::config::{ProjectConfig, SourceSubstrate};
use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::templates::{starter_template, starter_templates};
use crate::tracker::TrackerDefinition;

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
    pub trackers: Utf8PathBuf,
    pub templates: Utf8PathBuf,
    pub snapshots: Utf8PathBuf,
    pub logs: Utf8PathBuf,
    pub state: Utf8PathBuf,
}

impl MetadataStore {
    pub fn at(project_root: impl Into<Utf8PathBuf>) -> Self {
        Self {
            paths: NewgitPaths::new(project_root.into()),
        }
    }

    pub fn init(project_root: impl Into<Utf8PathBuf>, project_name: &str) -> Result<Self> {
        let store = Self::at(project_root);
        store.create_layout()?;

        if !store.paths.config.exists() {
            let source = detect_source_substrate(&store.paths.project_root);
            let config = ProjectConfig::new(project_name, source);
            store.write_toml(&store.paths.config, "project config", &config)?;
        }

        store.write_template_files()?;
        Ok(store)
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
        self.read_toml(&self.paths.config)
    }

    pub fn write_tracker_definition(
        &self,
        definition: &TrackerDefinition,
        overwrite: bool,
    ) -> Result<Utf8PathBuf> {
        definition.validate()?;
        create_dir_all(&self.paths.trackers)?;
        let path = self
            .paths
            .trackers
            .join(format!("{}.toml", definition.name));
        if path.exists() && !overwrite {
            return Err(NewgitError::AlreadyExists(path));
        }
        self.write_toml(&path, &format!("tracker `{}`", definition.name), definition)?;
        Ok(path)
    }

    pub fn add_tracker_from_template(
        &self,
        tracker_name: &str,
        template_name: &str,
    ) -> Result<Utf8PathBuf> {
        self.ensure_initialized()?;
        let definition = starter_template(template_name, tracker_name)?;
        self.write_tracker_definition(&definition, false)
    }

    pub fn load_tracker_definitions(&self) -> Result<Vec<TrackerDefinition>> {
        self.ensure_initialized()?;
        let mut definitions = Vec::new();

        for entry in read_dir_sorted(&self.paths.trackers)? {
            if entry.extension() != Some("toml") {
                continue;
            }
            definitions.push(self.read_toml(&entry)?);
        }

        let mut config = self.load_config()?;
        definitions.append(&mut config.trackers);
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(definitions)
    }

    pub fn write_branch(&self, branch: &BranchInstance) -> Result<Utf8PathBuf> {
        create_dir_all(&self.paths.branches)?;
        let path = self
            .paths
            .branches
            .join(format!("{}.toml", branch_slug(&branch.name)));
        self.write_toml(&path, &format!("branch `{}`", branch.name), branch)?;
        Ok(path)
    }

    pub fn load_branches(&self) -> Result<Vec<BranchInstance>> {
        self.ensure_initialized()?;
        let mut branches: Vec<BranchInstance> = Vec::new();

        for entry in read_dir_sorted(&self.paths.branches)? {
            if entry.extension() == Some("toml") {
                branches.push(self.read_toml(&entry)?);
            }
        }

        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(branches)
    }

    fn create_layout(&self) -> Result<()> {
        for path in [
            &self.paths.metadata_root,
            &self.paths.branches,
            &self.paths.trackers,
            &self.paths.templates,
            &self.paths.snapshots,
            &self.paths.logs,
            &self.paths.state,
        ] {
            create_dir_all(path)?;
        }
        Ok(())
    }

    fn write_template_files(&self) -> Result<()> {
        create_dir_all(&self.paths.templates)?;
        for template in starter_templates() {
            let path = self.paths.templates.join(format!("{}.toml", template.name));
            if !path.exists() {
                std::fs::write(&path, template.contents.trim_start())
                    .map_err(|source| NewgitError::io(path.clone(), source))?;
            }
        }

        let base_env = self.paths.templates.join("base.env");
        if !base_env.exists() {
            std::fs::write(&base_env, "# branch-local environment\n")
                .map_err(|source| NewgitError::io(base_env, source))?;
        }
        Ok(())
    }

    fn read_toml<T>(&self, path: &Utf8Path) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        toml::from_str(&contents).map_err(|source| NewgitError::TomlRead {
            path: path.to_path_buf(),
            source,
        })
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
            trackers: metadata_root.join("trackers"),
            templates: metadata_root.join("templates"),
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

fn detect_source_substrate(project_root: &Utf8Path) -> SourceSubstrate {
    if project_root.join(".jj").is_dir() {
        SourceSubstrate::Jj
    } else if project_root.join(".git").exists() {
        SourceSubstrate::Git
    } else {
        SourceSubstrate::Unknown
    }
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
        if path.file_name() != Some(".DS_Store") {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}
