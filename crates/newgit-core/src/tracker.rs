use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};

/// A named, versioned lane of file content. Parsed from
/// `.newgit/trackers/<name>.toml`; the name comes from the filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerDefinition {
    pub name: String,
    pub audience: String,
    pub storage: Storage,
    /// Whether a source merge should carry this tracker binding with it.
    pub merge_with_source: bool,
    /// Workspace-relative paths this tracker owns. May be empty for lanes
    /// that only receive deposits (e.g. `db-snapshots`).
    pub paths: Vec<Utf8PathBuf>,
    /// `sha256:<hex12>` of the definition file contents.
    pub definition_rev: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Storage {
    Local,
    Remote,
}

/// On-disk shape (everything but the filename-derived name).
#[derive(Debug, Serialize, Deserialize)]
pub struct TrackerDefinitionFile {
    audience: String,
    storage: Storage,
    #[serde(default)]
    merge_with_source: bool,
    #[serde(default)]
    paths: Vec<Utf8PathBuf>,
}

impl TrackerDefinition {
    pub fn new(
        name: &str,
        audience: String,
        storage: Storage,
        merge_with_source: bool,
        paths: Vec<Utf8PathBuf>,
    ) -> Result<Self> {
        let mut definition = Self {
            name: name.to_owned(),
            audience,
            storage,
            merge_with_source,
            paths,
            definition_rev: String::new(),
        };
        definition.validate()?;
        definition.definition_rev = definition.compute_definition_rev()?;
        Ok(definition)
    }

    pub fn from_file(name: &str, path: &Utf8Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        let file: TrackerDefinitionFile =
            toml::from_str(&contents).map_err(|source| NewgitError::TomlRead {
                path: path.to_path_buf(),
                source,
            })?;

        let digest = Sha256::digest(contents.as_bytes());
        let hex: String = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let definition = Self {
            name: name.to_owned(),
            audience: file.audience,
            storage: file.storage,
            merge_with_source: file.merge_with_source,
            paths: file.paths,
            definition_rev: format!("sha256:{hex}"),
        };
        definition.validate()?;
        Ok(definition)
    }

    fn validate(&self) -> Result<()> {
        let audience_ok = matches!(self.audience.as_str(), "public" | "project-devs" | "user")
            || self.audience.starts_with("user:");
        if !audience_ok {
            return Err(self.invalid(format!(
                "audience `{}` is not `public`, `project-devs`, `user`, or `user:<name>`",
                self.audience
            )));
        }

        for path in &self.paths {
            if path.is_absolute()
                || path.as_str().is_empty()
                || path.components().any(|c| c.as_str() == "..")
            {
                return Err(self.invalid(format!("path `{path}` must be workspace-relative")));
            }
            let first = path.components().next().map(|c| c.as_str().to_owned());
            if matches!(first.as_deref(), Some(".git" | ".newgit")) {
                return Err(self.invalid(format!(
                    "path `{path}` may not reach into `{}`",
                    first.unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    pub fn with_added_paths(&self, paths: &[Utf8PathBuf]) -> Result<Self> {
        let mut updated = self.clone();
        for path in paths {
            if !updated.paths.iter().any(|existing| existing == path) {
                updated.paths.push(path.clone());
            }
        }
        updated.paths.sort();
        updated.validate()?;
        updated.definition_rev = updated.compute_definition_rev()?;
        Ok(updated)
    }

    pub fn to_file(&self) -> TrackerDefinitionFile {
        TrackerDefinitionFile {
            audience: self.audience.clone(),
            storage: self.storage,
            merge_with_source: self.merge_with_source,
            paths: self.paths.clone(),
        }
    }

    fn compute_definition_rev(&self) -> Result<String> {
        let contents =
            toml::to_string_pretty(&self.to_file()).map_err(|source| NewgitError::TomlWrite {
                label: format!("tracker `{}`", self.name),
                source,
            })?;
        let digest = Sha256::digest(contents.as_bytes());
        let hex: String = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(format!("sha256:{hex}"))
    }

    fn invalid(&self, reason: String) -> NewgitError {
        NewgitError::InvalidDefinition {
            tracker: self.name.clone(),
            reason,
        }
    }
}

/// Content lanes must be disjoint: no two trackers may own the same path or
/// nest inside each other, or projection/restore order would matter.
pub fn validate_disjoint(definitions: &[TrackerDefinition]) -> Result<()> {
    for (index, left) in definitions.iter().enumerate() {
        for right in &definitions[index + 1..] {
            for left_path in &left.paths {
                for right_path in &right.paths {
                    if left_path.starts_with(right_path) || right_path.starts_with(left_path) {
                        return Err(NewgitError::TrackerPathConflict {
                            left: left.name.clone(),
                            right: right.name.clone(),
                            path: left_path.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Files a tracker owns inside a workspace, as (relative, absolute) pairs,
/// sorted by relative path. Missing paths are simply absent.
pub fn collect_owned_files(
    workspace: &Utf8Path,
    definition: &TrackerDefinition,
) -> Result<Vec<(Utf8PathBuf, Utf8PathBuf)>> {
    collect_files(workspace, &definition.paths)
}

/// Files under the given root-relative paths, as (relative, absolute) pairs,
/// sorted by relative path. Missing paths are simply absent.
pub fn collect_files(
    root: &Utf8Path,
    paths: &[Utf8PathBuf],
) -> Result<Vec<(Utf8PathBuf, Utf8PathBuf)>> {
    let mut files = Vec::new();
    for relative in paths {
        walk(root, relative, &mut files)?;
    }
    files.sort();
    Ok(files)
}

/// Every file under `root`, as (root-relative, absolute) pairs, sorted.
pub fn collect_all_files(root: &Utf8Path) -> Result<Vec<(Utf8PathBuf, Utf8PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|source| NewgitError::io(root, source))? {
        let entry = entry.map_err(|source| NewgitError::io(root, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(NewgitError::NonUtf8Path(entry.path().display().to_string()));
        };
        walk(root, Utf8Path::new(name), &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn walk(
    workspace: &Utf8Path,
    relative: &Utf8Path,
    files: &mut Vec<(Utf8PathBuf, Utf8PathBuf)>,
) -> Result<()> {
    let absolute = workspace.join(relative);
    if absolute.is_file() {
        files.push((relative.to_path_buf(), absolute));
        return Ok(());
    }
    if !absolute.is_dir() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(&absolute).map_err(|source| NewgitError::io(&absolute, source))?
    {
        let entry = entry.map_err(|source| NewgitError::io(&absolute, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(NewgitError::NonUtf8Path(entry.path().display().to_string()));
        };
        walk(workspace, &relative.join(name), files)?;
    }
    Ok(())
}

/// Content-addressed revision of a set of files: `hex12` over sorted
/// (path, length, bytes). Identical content dedupes to the same rev.
pub fn content_rev(files: &[(Utf8PathBuf, Utf8PathBuf)]) -> Result<String> {
    let mut hasher = Sha256::new();
    for (relative, absolute) in files {
        let bytes = std::fs::read(absolute).map_err(|source| NewgitError::io(absolute, source))?;
        hasher.update(relative.as_str().as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let digest = hasher.finalize();
    Ok(digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;

    fn definition(name: &str, paths: &[&str]) -> TrackerDefinition {
        TrackerDefinition {
            name: name.to_owned(),
            audience: "project-devs".to_owned(),
            storage: Storage::Local,
            merge_with_source: false,
            paths: paths.iter().map(Utf8PathBuf::from).collect(),
            definition_rev: "sha256:000000000000".to_owned(),
        }
    }

    #[test]
    fn nested_paths_conflict() {
        let defs = [
            definition("a", &["src/generated"]),
            definition("b", &["src/generated/sdk"]),
        ];
        assert!(matches!(
            validate_disjoint(&defs),
            Err(NewgitError::TrackerPathConflict { .. })
        ));
        let ok = [
            definition("a", &["src/generated"]),
            definition("b", &[".env.local"]),
        ];
        assert!(validate_disjoint(&ok).is_ok());
    }
}
