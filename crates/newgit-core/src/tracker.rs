use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};

/// A named, versioned lane of file content. Parsed from
/// `.newgit/trackers/<name>.toml`; the name comes from the filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerDefinition {
    pub name: String,
    pub kind: String,
    pub audience: String,
    pub propagation: Propagation,
    pub storage: Storage,
    /// Workspace-relative paths this tracker owns. May be empty for lanes
    /// that only receive deposits (e.g. `db-snapshots`).
    pub paths: Vec<Utf8PathBuf>,
    pub materialize: Option<MaterializeSpec>,
    /// Parsed and recorded in v1; consumed by `newgit run` from M3 on.
    pub exports: BTreeMap<String, String>,
    /// `sha256:<hex12>` of the definition file contents.
    pub definition_rev: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Propagation {
    Rebase,
    Pin,
    Manual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Storage {
    Local,
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterializeSpec {
    /// Source, relative to the store project root.
    pub copy_from: Utf8PathBuf,
    /// Destination in the workspace; defaults to the tracker's single owned
    /// path when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Utf8PathBuf>,
}

/// On-disk shape (everything but the filename-derived name).
#[derive(Debug, Deserialize)]
struct TrackerDefinitionFile {
    kind: String,
    audience: String,
    propagation: Propagation,
    storage: Storage,
    #[serde(default)]
    paths: Vec<Utf8PathBuf>,
    #[serde(default)]
    materialize: Option<MaterializeSpec>,
    #[serde(default)]
    exports: BTreeMap<String, String>,
}

impl TrackerDefinition {
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
            kind: file.kind,
            audience: file.audience,
            propagation: file.propagation,
            storage: file.storage,
            paths: file.paths,
            materialize: file.materialize,
            exports: file.exports,
            definition_rev: format!("sha256:{hex}"),
        };
        definition.validate()?;
        Ok(definition)
    }

    /// Where materialized content lands when `materialize.to` is omitted.
    pub fn materialize_target(&self) -> Option<&Utf8Path> {
        let spec = self.materialize.as_ref()?;
        if let Some(to) = &spec.to {
            return Some(to);
        }
        match self.paths.as_slice() {
            [single] => Some(single),
            _ => None,
        }
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

        if let Some(spec) = &self.materialize
            && spec.to.is_none()
            && self.paths.len() != 1
        {
            return Err(self.invalid(
                "materialize.to is required when the tracker does not own exactly one path"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn invalid(&self, reason: String) -> NewgitError {
        NewgitError::InvalidDefinition {
            tracker: self.name.clone(),
            reason,
        }
    }
}

/// Content lanes must be disjoint: no two trackers may own the same path or
/// nest inside each other, or materialization order would matter.
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
    let mut files = Vec::new();
    for owned in &definition.paths {
        walk(workspace, owned, &mut files)?;
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
            kind: "file-snapshot".to_owned(),
            audience: "project-devs".to_owned(),
            propagation: Propagation::Pin,
            storage: Storage::Local,
            paths: paths.iter().map(Utf8PathBuf::from).collect(),
            materialize: None,
            exports: BTreeMap::new(),
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
