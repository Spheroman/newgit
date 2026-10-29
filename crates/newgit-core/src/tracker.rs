use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
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
    /// Workspace-relative paths this tracker owns, as [`normalize_path`]
    /// spells them. May be empty for lanes that only receive deposits (e.g.
    /// `db-snapshots`). `.` means everything no more specific tracker owns —
    /// the `root` tracker.
    pub paths: Vec<Utf8PathBuf>,
    /// Kept out of Git entirely: excluded in every workspace, never committed,
    /// captured by newgit from the files. True unless a definition says
    /// otherwise — fails closed, like export's audience filter.
    pub secret: bool,
    /// Who may merge changes to it: an audience (`project-devs`,
    /// `user:<login>`) or `owner`, the project's owner. `None` means its
    /// audience — anyone who may read it may change it. Changing a tracker's
    /// *definition* is changing whichever tracker owns its file, so layers of
    /// governance are trackers owning other trackers' definitions.
    pub maintainers: Option<String>,
    /// The slot it fills: trackers sharing a role are alternatives (`dev-db`,
    /// `staging-db`), and an instance binds one of them.
    pub role: Option<String>,
    /// Which tracker an instance binds by default: the highest in its role,
    /// unless a person's own default says otherwise. Negative means only when
    /// asked for (`--use`), for a role-less tracker too.
    pub priority: i32,
    /// Trackers or roles it cannot do without: binding it binds them, and
    /// they cannot be dropped while it is bound.
    pub requires: Vec<String>,
    /// Resources it brings: they bind only while it is bound.
    pub resources: Vec<String>,
    /// Paths inside this tracker's own that a more specific tracker owns, so
    /// they are not this tracker's to capture, restore, or clear. Computed
    /// from the full set of definitions ([`resolve_ownership`]); not part of
    /// the definition file.
    pub excluded: Vec<Utf8PathBuf>,
    /// `sha256:<hex12>` of the definition file contents.
    pub definition_rev: String,
}

/// The tracker owning everything no other tracker claims.
pub const ROOT_TRACKER: &str = "root";

/// The tracker carrying the project's committed definitions, `.newgit/`.
pub const PROJECT_TRACKER: &str = "project";

/// The tracker carrying `.newgit/trackers/`: which tracker owns which paths
/// and who may read each. Its own tracker so that only the project's owner
/// changes it, while anyone who merges may change resources and scripts.
pub const ACCESS_TRACKER: &str = "access";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Storage {
    Local,
    Remote,
}

/// On-disk shape (everything but the filename-derived name).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerDefinitionFile {
    audience: String,
    storage: Storage,
    #[serde(default)]
    merge_with_source: bool,
    #[serde(default)]
    paths: Vec<Utf8PathBuf>,
    #[serde(default = "secret_by_default", skip_serializing_if = "is_true")]
    secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintainers: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    priority: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resources: Vec<String>,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

fn secret_by_default() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
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
            paths: paths.iter().map(|path| normalize_path(path)).collect(),
            secret: true,
            maintainers: None,
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            definition_rev: String::new(),
        };
        definition.validate()?;
        definition.definition_rev = definition.compute_definition_rev()?;
        Ok(definition)
    }

    /// A code tracker as `newgit tracker create --code` writes it: committed,
    /// merged with the work that changes it. Checked as one from the start,
    /// since a code tracker may own what a secret one may not: another
    /// tracker's definition.
    pub fn code(name: &str, audience: String, paths: &[Utf8PathBuf]) -> Result<Self> {
        let mut definition = Self {
            name: name.to_owned(),
            audience,
            storage: Storage::Local,
            merge_with_source: true,
            paths: paths.iter().map(|path| normalize_path(path)).collect(),
            secret: false,
            maintainers: None,
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
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
            paths: file.paths.iter().map(|path| normalize_path(path)).collect(),
            secret: file.secret,
            maintainers: file.maintainers,
            role: file.role,
            priority: file.priority,
            requires: file.requires,
            resources: file.resources,
            excluded: Vec::new(),
            definition_rev: format!("sha256:{hex}"),
        };
        definition.validate()?;
        Ok(definition)
    }

    fn validate(&self) -> Result<()> {
        if let Some(maintainers) = &self.maintainers {
            let ok = matches!(maintainers.as_str(), "owner" | "project-devs")
                || (maintainers.starts_with("user:") && maintainers.len() > "user:".len());
            if !ok {
                return Err(self.invalid(format!(
                    "maintainers `{maintainers}` must be `owner`, `project-devs`, or `user:<login>`"
                )));
            }
        }
        let audience_ok = matches!(self.audience.as_str(), "public" | "project-devs" | "user")
            || self.audience.starts_with("user:");
        if !audience_ok {
            return Err(self.invalid(format!(
                "audience `{}` is not `public`, `project-devs`, `user`, or `user:<name>`",
                self.audience
            )));
        }

        if self.owns_everything() && self.paths.len() > 1 {
            return Err(self.invalid(
                "`.` owns everything not claimed elsewhere, so it must be the only path".to_owned(),
            ));
        }
        // `.newgit` holds the project's own definitions: `project` carries
        // them, and `access` the tracker definitions among them. Anyone else
        // claiming part of it would split the definitions across trackers.
        let is_project = (self.name == PROJECT_TRACKER
            && self.paths == [Utf8PathBuf::from(".newgit")])
            || (self.name == ACCESS_TRACKER
                && self.paths == [Utf8PathBuf::from(".newgit/trackers")]);

        // Every path here is already normalized, so `./.git` meets the
        // checks as `.git`.
        for path in &self.paths {
            if path.as_str() == "." || is_project {
                continue;
            }
            if path.is_absolute()
                || path.as_str().is_empty()
                || path.components().any(|c| c.as_str() == "..")
            {
                return Err(self.invalid(format!("path `{path}` must be workspace-relative")));
            }
            // Except a tracker's definition: whoever may change the tracker
            // owning `web.toml` decides who may read `web`, so governance
            // layers by owning definitions (docs/design/trackers.md, "Who may
            // change what"). Only a code tracker may: a secret one would
            // keep the definition out of Git.
            if is_definition_file(path) {
                if self.is_code() {
                    continue;
                }
                return Err(self.invalid(format!(
                    "path `{path}` is a tracker definition, which only a code tracker may own"
                )));
            }
            // Regardless of case: a case-insensitive file system opens `.GIT`
            // as `.git`, and Git refuses it everywhere.
            let first = path
                .components()
                .next()
                .map(|c| c.as_str().to_owned())
                .unwrap_or_default();
            if [".git", ".newgit"]
                .iter()
                .any(|reserved| first.eq_ignore_ascii_case(reserved))
            {
                return Err(self.invalid(format!("path `{path}` may not reach into `{first}`")));
            }
        }
        Ok(())
    }

    pub fn with_added_paths(&self, paths: &[Utf8PathBuf]) -> Result<Self> {
        let mut updated = self.clone();
        for path in paths {
            let path = normalize_path(path);
            if !updated.paths.contains(&path) {
                updated.paths.push(path);
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
            secret: self.secret,
            maintainers: self.maintainers.clone(),
            role: self.role.clone(),
            priority: self.priority,
            requires: self.requires.clone(),
            resources: self.resources.clone(),
        }
    }

    /// The root tracker as `newgit init` writes it: owns `.`, committed, read
    /// by the project's developers. Its history is the project's own Git
    /// history.
    pub fn root() -> Self {
        let mut definition = Self {
            name: ROOT_TRACKER.to_owned(),
            audience: "project-devs".to_owned(),
            storage: Storage::Local,
            merge_with_source: true,
            paths: vec![Utf8PathBuf::from(".")],
            secret: false,
            maintainers: None,
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            definition_rev: String::new(),
        };
        definition.definition_rev = definition
            .compute_definition_rev()
            .expect("the root definition serializes");
        definition
    }

    /// The access tracker as `newgit init` writes it: `.newgit/trackers/`,
    /// every tracker's paths and audience. Carved out of `project` so the
    /// remote can make changing it the owner's alone.
    pub fn access() -> Self {
        let mut definition = Self {
            name: ACCESS_TRACKER.to_owned(),
            audience: "project-devs".to_owned(),
            storage: Storage::Local,
            merge_with_source: true,
            paths: vec![Utf8PathBuf::from(".newgit/trackers")],
            secret: false,
            maintainers: Some("owner".to_owned()),
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            definition_rev: String::new(),
        };
        definition.definition_rev = definition
            .compute_definition_rev()
            .expect("the access definition serializes");
        definition
    }

    /// The project tracker as `newgit init` writes it: the committed parts of
    /// `.newgit/` (its local state is gitignored, so never committed), read
    /// by the project's developers, merged with the work that changes it.
    pub fn project() -> Self {
        let mut definition = Self {
            name: PROJECT_TRACKER.to_owned(),
            audience: "project-devs".to_owned(),
            storage: Storage::Local,
            merge_with_source: true,
            paths: vec![Utf8PathBuf::from(".newgit")],
            secret: false,
            maintainers: None,
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            definition_rev: String::new(),
        };
        definition.definition_rev = definition
            .compute_definition_rev()
            .expect("the project definition serializes");
        definition
    }

    /// Committed content with its own history, composed into workspaces
    /// rather than captured from files — every non-secret tracker but root.
    pub fn is_code(&self) -> bool {
        !self.secret && !self.owns_everything()
    }

    /// Owns `.`: everything no more specific tracker claims.
    pub fn owns_everything(&self) -> bool {
        self.paths.iter().any(|path| path.as_str() == ".")
    }

    /// Whether `relative` is this tracker's: under one of its paths and not
    /// under a more specific tracker's.
    pub fn owns(&self, relative: &Utf8Path) -> bool {
        let claimed = self
            .paths
            .iter()
            .any(|path| path.as_str() == "." || relative.starts_with(path));
        claimed && !self.excluded.iter().any(|path| relative.starts_with(path))
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
            kind: "tracker",
            name: self.name.clone(),
            reason,
        }
    }
}

/// A tracker path by its components alone, so `./.git`, `.git/` and `.git`
/// are one path: checked, compared, and written to `.gitignore` as one. `./`
/// is `.`. A path that is not workspace-relative (absolute, or with `..`) is
/// returned as written, for validation to refuse.
pub fn normalize_path(path: &Utf8Path) -> Utf8PathBuf {
    let mut normal = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::Normal(part) => normal.push(part),
            Utf8Component::ParentDir | Utf8Component::RootDir | Utf8Component::Prefix(_) => {
                return path.to_path_buf();
            }
        }
    }
    if normal.as_str().is_empty() && !path.as_str().is_empty() {
        return Utf8PathBuf::from(".");
    }
    normal
}

/// Whether `path` is a tracker's definition: a `.toml` file directly in
/// `.newgit/trackers/`, the files newgit reads definitions from.
fn is_definition_file(path: &Utf8Path) -> bool {
    let mut components = path.components().map(|component| component.as_str());
    components.next() == Some(".newgit")
        && components.next() == Some("trackers")
        && components
            .next()
            .is_some_and(|file| Utf8Path::new(file).extension() == Some("toml"))
        && components.next().is_none()
}

/// Settle who owns what across every tracker. A path's owner is its most
/// specific claimant, so trackers may nest — `apps` can hold `apps/electron`
/// for another tracker — but no two may claim the identical path, and only
/// one may own `.`. Fills in each definition's `excluded` (paths inside its
/// own that a more specific tracker owns).
pub fn resolve_ownership(definitions: &mut [TrackerDefinition]) -> Result<()> {
    let roots: Vec<&str> = definitions
        .iter()
        .filter(|definition| definition.owns_everything())
        .map(|definition| definition.name.as_str())
        .collect();
    if roots.len() > 1 {
        return Err(NewgitError::TrackerPathConflict {
            left: roots[0].to_owned(),
            right: roots[1].to_owned(),
            path: Utf8PathBuf::from("."),
        });
    }
    for (index, left) in definitions.iter().enumerate() {
        for right in &definitions[index + 1..] {
            // Trackers sharing a role are alternatives — an instance binds
            // one of them — so they may claim the very same paths.
            if left.role.is_some() && left.role == right.role {
                continue;
            }
            for path in &left.paths {
                if right.paths.contains(path) {
                    return Err(NewgitError::TrackerPathConflict {
                        left: left.name.clone(),
                        right: right.name.clone(),
                        path: path.clone(),
                    });
                }
            }
        }
    }
    let claims: Vec<(String, Utf8PathBuf)> = definitions
        .iter()
        .flat_map(|definition| {
            definition
                .paths
                .iter()
                .map(|path| (definition.name.clone(), path.clone()))
        })
        .collect();
    for definition in definitions.iter_mut() {
        definition.excluded = claims
            .iter()
            .filter(|(name, inner)| {
                name != &definition.name
                    && inner.as_str() != "."
                    && definition.paths.iter().any(|outer| {
                        outer.as_str() == "." || (inner != outer && inner.starts_with(outer))
                    })
            })
            .map(|(_, inner)| inner.clone())
            .collect();
        definition.excluded.sort();
        definition.excluded.dedup();
    }
    Ok(())
}

/// Files a tracker owns inside a workspace, as (relative, absolute) pairs,
/// sorted by relative path. Missing paths are simply absent.
pub fn collect_owned_files(
    workspace: &Utf8Path,
    definition: &TrackerDefinition,
) -> Result<Vec<(Utf8PathBuf, Utf8PathBuf)>> {
    let mut files = collect_files(workspace, &definition.paths)?;
    files.retain(|(relative, _)| {
        !definition
            .excluded
            .iter()
            .any(|inner| relative.starts_with(inner))
    });
    Ok(files)
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
            secret: true,
            maintainers: None,
            role: None,
            priority: 0,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            definition_rev: "sha256:000000000000".to_owned(),
        }
    }

    #[test]
    fn the_most_specific_owner_wins_and_identical_claims_conflict() {
        let mut defs = [
            definition("a", &["src/generated"]),
            definition("b", &["src/generated/sdk"]),
            TrackerDefinition::root(),
        ];
        resolve_ownership(&mut defs).expect("nesting is allowed");
        assert_eq!(
            defs[0].excluded,
            vec![Utf8PathBuf::from("src/generated/sdk")]
        );
        assert!(defs[1].excluded.is_empty());
        assert_eq!(
            defs[2].excluded,
            vec![
                Utf8PathBuf::from("src/generated"),
                Utf8PathBuf::from("src/generated/sdk")
            ]
        );
        assert!(defs[0].owns(Utf8Path::new("src/generated/types.ts")));
        assert!(!defs[0].owns(Utf8Path::new("src/generated/sdk/client.ts")));
        assert!(defs[2].owns(Utf8Path::new("README.md")));
        assert!(!defs[2].owns(Utf8Path::new("src/generated/types.ts")));

        let mut clash = [
            definition("a", &[".env.local"]),
            definition("b", &[".env.local"]),
        ];
        assert!(matches!(
            resolve_ownership(&mut clash),
            Err(NewgitError::TrackerPathConflict { .. })
        ));
    }

    #[test]
    fn code_trackers_and_the_project_tracker_are_committed() {
        assert!(TrackerDefinition::project().is_code());
        assert!(!TrackerDefinition::root().is_code() && !TrackerDefinition::root().secret);
        // Only the project tracker may own `.newgit`.
        let error = TrackerDefinition::new(
            "sneaky",
            "project-devs".to_owned(),
            Storage::Local,
            false,
            vec![Utf8PathBuf::from(".newgit")],
        )
        .expect_err("`.newgit` is the project tracker's");
        assert!(error.to_string().contains("may not reach into"), "{error}");
    }

    #[test]
    fn invalid_definition_error_names_tracker() {
        let error = TrackerDefinition::new(
            "runtime-env",
            "project-devs".to_owned(),
            Storage::Local,
            false,
            vec![Utf8PathBuf::from("../.env")],
        )
        .expect_err("a tracker path outside the workspace is invalid");

        assert_eq!(
            error.to_string(),
            "tracker `runtime-env` is invalid: path `../.env` must be workspace-relative"
        );
    }

    /// `merge_with_source` decides whether a lane's bound state travels with
    /// a real source merge. Misspelled, it defaults to false and the tracker
    /// quietly stops making that trip.
    #[test]
    fn a_misspelled_key_is_rejected_rather_than_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(temp.path().join("env.toml")).expect("utf8 path");
        std::fs::write(
            &path,
            r#"audience = "project-devs"
storage = "local"
merge_with_sauce = true
"#,
        )
        .expect("write definition");

        let error = TrackerDefinition::from_file("env", &path)
            .expect_err("a key newgit does not understand is an error");
        let message = error.to_string() + &format!("{error:?}");
        assert!(message.contains("merge_with_sauce"), "names the bad key");
        assert!(
            message.contains("merge_with_source"),
            "names the valid keys"
        );
    }
}
