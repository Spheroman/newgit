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
    /// Workspace-relative paths this tracker claims, as [`normalize_path`]
    /// spells them: its claims (`paths` in the file, relative to its mount)
    /// joined onto its mount. May be empty for lanes that only receive
    /// deposits (e.g. `db-snapshots`). `.` means everything no more specific
    /// tracker owns — the `root` tracker.
    pub paths: Vec<Utf8PathBuf>,
    /// Where its own root sits in the workspace (`mount`, default `.`). A
    /// tracker in Git stores its files relative to it, so `apps/t3code/x.ts`
    /// is `x.ts` in t3code's history.
    pub mount: Utf8PathBuf,
    /// The ref `newgit pull <tracker>` brings in (`follow`, default `main`).
    pub follow: String,
    /// Its claims as the file writes them, relative to the mount, `!`
    /// exclusions included: what [`Self::to_file`] writes back.
    pub claims: Vec<String>,
    /// Workspace-relative paths its claims exclude (`!node_modules/`): owned
    /// only by a tracker that claims them at the same mount, or by nothing.
    pub exclusions: Vec<Utf8PathBuf>,
    /// Whether its content is committed to Git (and split into the tracker's
    /// own history). False unless a definition says otherwise: the content
    /// is then kept out of Git entirely — excluded in every workspace, never
    /// committed, captured by newgit from the files — which fails closed,
    /// like export's audience filter.
    pub git: bool,
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
    /// How its parent records it rolling up (`history`, the parent's
    /// choice, as the mount is): `commits`, one per change so the parent's
    /// `git log` is complete, or `pins`, one snapshot of where it is now
    /// (docs/design/anchoring.md, *How much history rolls up*).
    pub history: History,
    /// Whether its parent holds it where it is (`pinned = true`): a change
    /// landing at it never rolls up into the parent, and moving the
    /// parent's pin is a change to the parent, its maintainers' to approve.
    /// For what must not move by itself: a private dependency, `/env/prod`.
    pub pinned: bool,
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
    /// The tracker in Git that defines this one in its own
    /// `.newgit/trackers/`, so its own commits pin it and it goes wherever
    /// that tracker goes. `None`: root's, defined in `access`.
    pub parent: Option<String>,
    /// `sha256:<hex12>` of the definition file contents.
    pub definition_rev: String,
}

/// The tracker owning everything no other tracker claims.
pub const ROOT_TRACKER: &str = "root";

/// What joins a tracker's id to its parent's: `t3code__repos` is the child
/// `repos` of `t3code`. An id is what refs, records, and remote repos hold; a
/// slash would land inside the parent's own ref space and is no part of a
/// remote repo's name. People see paths ([`tracker_path`]). Reserved, so no
/// one tracker's name can look like two.
pub const ID_SEPARATOR: &str = "__";

/// The id of `parent`'s child `name`, which is the name a pin and a definition
/// file have in `parent`'s own tree. Root's children are their own ids.
pub fn child_id(parent: &str, name: &str) -> String {
    if parent == ROOT_TRACKER {
        name.to_owned()
    } else {
        format!("{parent}{ID_SEPARATOR}{name}")
    }
}

/// The name `id` has in its parent's tree.
pub fn leaf_name(id: &str) -> &str {
    id.rsplit(ID_SEPARATOR).next().unwrap_or(id)
}

/// Whether `name` can be part of an id: no `__`, which joins an id's parts, and
/// no `_` at either end, which would run into the join (`a_` + `b` and `a` +
/// `_b` would both be `a___b`).
pub fn name_fits_id(name: &str) -> bool {
    !name.contains(ID_SEPARATOR) && !name.starts_with('_') && !name.ends_with('_')
}

/// The most an id can hold: a remote repo's name gives it one part of 63.
pub const MAX_ID_LEN: usize = 63;

/// The tracker's path from the top of its project: `t3code/repos`.
pub fn tracker_path(id: &str) -> String {
    id.replace(ID_SEPARATOR, "/")
}

/// The tracker that carried `.newgit/` before it folded into the root
/// tracker (docs/design/trackers.md, *Migration*). Only `newgit tracker
/// migrate` still knows it.
pub const PROJECT_TRACKER: &str = "project";

/// The tracker carrying `.newgit/trackers/`: which tracker owns which paths
/// and who may read each. Its own tracker, mounted at `.newgit/trackers`, so
/// that only the project's owner changes it, while anyone who merges may
/// change resources and scripts.
pub const ACCESS_TRACKER: &str = "access";

/// Where `access` is mounted: the definitions.
pub const DEFINITIONS_DIR: &str = ".newgit/trackers";

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
    #[serde(default, skip_serializing_if = "is_default_mount")]
    mount: Option<Utf8PathBuf>,
    #[serde(default, skip_serializing_if = "is_default_follow")]
    follow: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    git: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintainers: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    priority: i32,
    #[serde(default, skip_serializing_if = "is_default_history")]
    history: History,
    #[serde(default, skip_serializing_if = "is_false")]
    pinned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resources: Vec<String>,
}

/// How a parent records a child rolling up: every change (`commits`, the
/// default), or where it is now (`pins`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum History {
    #[default]
    Commits,
    Pins,
}

fn is_default_history(history: &History) -> bool {
    *history == History::Commits
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_mount(mount: &Option<Utf8PathBuf>) -> bool {
    mount
        .as_ref()
        .is_none_or(|mount| normalize_path(mount).as_str() == ".")
}

fn is_default_follow(follow: &Option<String>) -> bool {
    follow.as_deref().is_none_or(|follow| follow == "main")
}

/// Claims as written (`apps/web`, `!dist/`) laid onto `mount`: the paths
/// claimed, and the paths excluded, both workspace-relative.
fn place_claims(mount: &Utf8Path, claims: &[String]) -> (Vec<Utf8PathBuf>, Vec<Utf8PathBuf>) {
    let mut paths = Vec::new();
    let mut exclusions = Vec::new();
    for claim in claims {
        let (excluded, claim) = match claim.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, claim.as_str()),
        };
        let relative = normalize_path(Utf8Path::new(claim.trim_end_matches('/')));
        let placed = if mount.as_str() == "." {
            relative
        } else if relative.as_str() == "." {
            mount.to_path_buf()
        } else if relative.is_absolute() || relative.components().any(|c| c.as_str() == "..") {
            // Left as written, for validation to refuse.
            relative
        } else {
            mount.join(relative)
        };
        if excluded {
            exclusions.push(placed);
        } else {
            paths.push(placed);
        }
    }
    (paths, exclusions)
}

impl TrackerDefinition {
    pub fn new(
        name: &str,
        audience: String,
        storage: Storage,
        merge_with_source: bool,
        paths: Vec<Utf8PathBuf>,
    ) -> Result<Self> {
        let paths: Vec<Utf8PathBuf> = paths.iter().map(|path| normalize_path(path)).collect();
        let mut definition = Self {
            name: name.to_owned(),
            audience,
            storage,
            merge_with_source,
            claims: paths.iter().map(ToString::to_string).collect(),
            paths,
            mount: Utf8PathBuf::from("."),
            follow: "main".to_owned(),
            exclusions: Vec::new(),
            git: false,
            maintainers: None,
            role: None,
            priority: 0,
            history: History::Commits,
            pinned: false,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            parent: None,
            definition_rev: String::new(),
        };
        definition.validate()?;
        definition.definition_rev = definition.compute_definition_rev()?;
        Ok(definition)
    }

    /// A tracker in Git as `newgit tracker create --git --path <mount>`
    /// writes it: mounted at `mount`, claiming everything under it, committed,
    /// merged with the work that changes it. Checked as one from the start,
    /// since a tracker in Git may own what one outside Git may not: another
    /// tracker's definition.
    pub fn in_git(name: &str, audience: String, mount: &Utf8Path) -> Result<Self> {
        let mount = normalize_path(mount);
        let mut definition = Self {
            name: name.to_owned(),
            audience,
            storage: Storage::Local,
            merge_with_source: true,
            paths: vec![mount.clone()],
            mount,
            follow: "main".to_owned(),
            claims: vec![".".to_owned()],
            exclusions: Vec::new(),
            git: true,
            maintainers: None,
            role: None,
            priority: 0,
            history: History::Commits,
            pinned: false,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            parent: None,
            definition_rev: String::new(),
        };
        definition.validate()?;
        definition.definition_rev = definition.compute_definition_rev()?;
        Ok(definition)
    }

    /// Move a definition read from `parent`'s own `.newgit/trackers/`, whose
    /// paths are relative to `parent`, to where `parent` is mounted at `at`.
    pub fn nest_under(&mut self, parent: &str, at: &Utf8Path) {
        let place = |path: &Utf8PathBuf| {
            if path.as_str() == "." {
                at.to_path_buf()
            } else {
                normalize_path(&at.join(path))
            }
        };
        self.paths = self.paths.iter().map(place).collect();
        self.exclusions = self.exclusions.iter().map(place).collect();
        self.mount = place(&self.mount);
        self.name = child_id(parent, &self.name);
        self.parent = Some(parent.to_owned());
    }

    pub fn from_file(name: &str, path: &Utf8Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        Self::from_text(name, path, &contents)
    }

    /// A definition from its file's text; `path` names it in errors.
    pub fn from_text(name: &str, path: &Utf8Path, contents: &str) -> Result<Self> {
        let file: TrackerDefinitionFile =
            toml::from_str(contents).map_err(|source| NewgitError::TomlRead {
                path: path.to_path_buf(),
                source,
            })?;

        let digest = Sha256::digest(contents.as_bytes());
        let hex: String = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let mount = file
            .mount
            .as_deref()
            .map_or_else(|| Utf8PathBuf::from("."), normalize_path);
        let (paths, exclusions) = place_claims(&mount, &file.paths);
        let definition = Self {
            name: name.to_owned(),
            audience: file.audience,
            storage: file.storage,
            merge_with_source: file.merge_with_source,
            paths,
            mount,
            follow: file.follow.unwrap_or_else(|| "main".to_owned()),
            claims: file.paths,
            exclusions,
            git: file.git,
            maintainers: file.maintainers,
            role: file.role,
            priority: file.priority,
            history: file.history,
            pinned: file.pinned,
            requires: file.requires,
            resources: file.resources,
            excluded: Vec::new(),
            parent: None,
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
        // `.newgit` holds the project's own definitions: the root tracker
        // carries them, and `access` the tracker definitions among them.
        // Anyone else claiming part of it would split the definitions across
        // trackers. (`project` is only ever read, by the migration.)
        let is_project = (self.name == PROJECT_TRACKER
            && self.paths == [Utf8PathBuf::from(".newgit")])
            || (self.name == ACCESS_TRACKER && self.paths == [Utf8PathBuf::from(DEFINITIONS_DIR)]);
        if self.follow.is_empty() || self.follow.starts_with('-') || self.follow.contains("..") {
            return Err(self.invalid(format!("follow `{}` is not a ref name", self.follow)));
        }

        // Every path here is already normalized, so `./.git` meets the
        // checks as `.git`.
        for path in
            self.paths
                .iter()
                .chain(&self.exclusions)
                .chain(std::iter::once(&self.mount).filter(|mount| {
                    // A tracker in Git may sit beside `access`, claiming the
                    // definitions it governs.
                    mount.as_str() != "." && !(self.git && mount.as_str() == DEFINITIONS_DIR)
                }))
        {
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
            // change what"). Only a tracker in Git may: one outside Git would
            // keep the definition out of Git.
            if is_definition_file(path) {
                // Governance only delegates downward (#228): `access`'s
                // definition is always its own, and no tracker may govern
                // itself. The remote ignores such a claim; refusing it here
                // keeps the local view from reading a definition the remote
                // never would.
                let file = path.file_stem().unwrap_or_default();
                if file == ACCESS_TRACKER || file == self.name {
                    return Err(self.invalid(format!(
                        "path `{path}` is {} definition, which no tracker may own but `access`",
                        if file == self.name {
                            "this tracker's own".to_owned()
                        } else {
                            "`access`'s".to_owned()
                        }
                    )));
                }
                if self.carved() {
                    continue;
                }
                return Err(self.invalid(format!(
                    "path `{path}` is a tracker definition, which only a tracker in Git may own"
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
                updated.paths.push(path.clone());
                updated.claims.push(path.to_string());
            }
        }
        updated.paths.sort();
        updated.claims.sort();
        updated.validate()?;
        updated.definition_rev = updated.compute_definition_rev()?;
        Ok(updated)
    }

    pub fn to_file(&self) -> TrackerDefinitionFile {
        TrackerDefinitionFile {
            audience: self.audience.clone(),
            storage: self.storage,
            merge_with_source: self.merge_with_source,
            mount: (self.mount.as_str() != ".").then(|| self.mount.clone()),
            follow: (self.follow != "main").then(|| self.follow.clone()),
            paths: self.claims.clone(),
            git: self.git,
            maintainers: self.maintainers.clone(),
            role: self.role.clone(),
            priority: self.priority,
            history: self.history,
            pinned: self.pinned,
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
            mount: Utf8PathBuf::from("."),
            follow: "main".to_owned(),
            claims: vec![".".to_owned()],
            exclusions: Vec::new(),
            git: true,
            maintainers: None,
            role: None,
            priority: 0,
            history: History::Commits,
            pinned: false,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            parent: None,
            definition_rev: String::new(),
        };
        definition.definition_rev = definition
            .compute_definition_rev()
            .expect("the root definition serializes");
        definition
    }

    /// The access tracker as `newgit init` writes it: mounted at
    /// `.newgit/trackers/`, every tracker's paths and audience, at its own
    /// root. Its own tracker so the remote can make changing it the owner's
    /// alone.
    pub fn access() -> Self {
        let mut definition = Self {
            name: ACCESS_TRACKER.to_owned(),
            audience: "project-devs".to_owned(),
            storage: Storage::Local,
            merge_with_source: true,
            paths: vec![Utf8PathBuf::from(DEFINITIONS_DIR)],
            mount: Utf8PathBuf::from(DEFINITIONS_DIR),
            follow: "main".to_owned(),
            claims: vec![".".to_owned()],
            exclusions: Vec::new(),
            git: true,
            maintainers: Some("owner".to_owned()),
            role: None,
            priority: 0,
            history: History::Commits,
            pinned: false,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            parent: None,
            definition_rev: String::new(),
        };
        definition.definition_rev = definition
            .compute_definition_rev()
            .expect("the access definition serializes");
        definition
    }

    /// Committed content with its own history, composed into workspaces
    /// rather than captured from files — every tracker in Git but root.
    pub fn carved(&self) -> bool {
        self.git && !self.owns_everything()
    }

    /// Owns `.`: everything no more specific tracker claims.
    pub fn owns_everything(&self) -> bool {
        self.paths.iter().any(|path| path.as_str() == ".")
    }

    /// Whether `relative` is this tracker's: under one of its paths, not
    /// under one of its exclusions, and not under a more specific tracker's.
    pub fn owns(&self, relative: &Utf8Path) -> bool {
        let claimed = self
            .paths
            .iter()
            .any(|path| path.as_str() == "." || relative.starts_with(path));
        claimed
            && !self
                .exclusions
                .iter()
                .any(|path| relative.starts_with(path))
            && !self.excluded.iter().any(|path| relative.starts_with(path))
    }

    /// Mounted somewhere other than the workspace's root.
    pub fn mounted(&self) -> bool {
        self.mount.as_str() != "."
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

/// Whether `mount` holds `path`: `.` holds everything.
fn mount_holds(mount: &Utf8Path, path: &Utf8Path) -> bool {
    mount.as_str() == "." || path.starts_with(mount)
}

fn mount_depth(mount: &Utf8Path) -> usize {
    if mount.as_str() == "." {
        0
    } else {
        mount.components().count()
    }
}

impl TrackerDefinition {
    /// How specifically this tracker's own claims take `path`: the length of
    /// its longest claim covering it, or `None` when none does or one of its
    /// exclusions takes it out.
    pub fn claim_on(&self, path: &Utf8Path) -> Option<usize> {
        if self
            .exclusions
            .iter()
            .any(|excluded| path.starts_with(excluded))
        {
            return None;
        }
        self.paths
            .iter()
            .filter(|claim| claim.as_str() == "." || path.starts_with(claim))
            .map(|claim| {
                if claim.as_str() == "." {
                    0
                } else {
                    claim.as_str().len()
                }
            })
            .max()
    }
}

/// The owner of `path` among `definitions`, by the ownership rules
/// (docs/design/trackers.md, *Ownership*): the deepest mount holding the path
/// decides, a parent's claim of `.` not reaching into it; among the trackers
/// mounted there, the longest claim wins; a path every claim there excludes
/// is owned by nothing.
pub fn owner_of<'a>(
    definitions: &'a [TrackerDefinition],
    path: &Utf8Path,
) -> Option<&'a TrackerDefinition> {
    let deepest = definitions
        .iter()
        .filter(|definition| mount_holds(&definition.mount, path))
        .map(|definition| mount_depth(&definition.mount))
        .max()?;
    definitions
        .iter()
        .filter(|definition| {
            mount_holds(&definition.mount, path) && mount_depth(&definition.mount) == deepest
        })
        .filter_map(|definition| definition.claim_on(path).map(|claim| (claim, definition)))
        .max_by(|(left, a), (right, b)| left.cmp(right).then_with(|| b.name.cmp(&a.name)))
        .map(|(_, definition)| definition)
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
    // A tracker mounted deeper takes its whole mount, whatever it claims
    // there: a parent's claim does not reach into a mount.
    let mounts: Vec<(String, Utf8PathBuf)> = definitions
        .iter()
        .filter(|definition| definition.mounted())
        .map(|definition| (definition.name.clone(), definition.mount.clone()))
        .collect();
    for definition in definitions.iter_mut() {
        let own_depth = mount_depth(&definition.mount);
        let inside = |inner: &Utf8Path, outer: &Utf8PathBuf| {
            outer.as_str() == "." || (inner != outer && inner.starts_with(outer))
        };
        definition.excluded = claims
            .iter()
            .filter(|(name, inner)| {
                name != &definition.name
                    && inner.as_str() != "."
                    && definition.paths.iter().any(|outer| inside(inner, outer))
            })
            .map(|(_, inner)| inner.clone())
            .chain(
                mounts
                    .iter()
                    .filter(|(name, mount)| {
                        name != &definition.name
                            && mount_depth(mount) > own_depth
                            && definition
                                .paths
                                .iter()
                                .any(|outer| outer.as_str() == "." || mount.starts_with(outer))
                    })
                    .map(|(_, mount)| mount.clone()),
            )
            .collect();
        definition.excluded.sort();
        definition.excluded.dedup();
    }
    Ok(())
}

/// Whether a symlink at `link` (relative to `root`) whose target is written
/// `target` stays inside `root`, judged from the text alone: the target is
/// relative and its `..`s never climb above `root`. This is what a stored
/// link means on any machine, which is why checkout judges a lane's links
/// by it; capture additionally resolves the link on disk
/// ([`link_resolves_inside`]).
pub fn link_target_inside(link: &Utf8Path, target: &Utf8Path) -> bool {
    if target.as_str().is_empty() {
        return false;
    }
    let mut depth: Vec<&str> = link
        .parent()
        .into_iter()
        .flat_map(|parent| parent.components())
        .filter_map(|component| match component {
            Utf8Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    for component in target.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::Normal(part) => depth.push(part),
            Utf8Component::ParentDir => {
                if depth.pop().is_none() {
                    return false;
                }
            }
            Utf8Component::RootDir | Utf8Component::Prefix(_) => return false,
        }
    }
    true
}

/// Whether the symlink at `root/link` resolves inside `root` on disk, as
/// well as by its text: a relative target can still leave through another
/// link along the way. A dangling link is judged by its text alone.
pub fn link_resolves_inside(root: &Utf8Path, link: &Utf8Path, target: &Utf8Path) -> bool {
    if !link_target_inside(link, target) {
        return false;
    }
    match (
        std::fs::canonicalize(root.join(link)),
        std::fs::canonicalize(root),
    ) {
        (Ok(resolved), Ok(root)) => resolved.starts_with(root),
        _ => true,
    }
}

/// The first directory along `relative` (excluding `relative` itself) that
/// is a symlink, with its target. Anything found "under" such a directory is
/// really wherever the link leads, so a walk must not go through it.
pub fn symlinked_parent(
    root: &Utf8Path,
    relative: &Utf8Path,
) -> Option<(Utf8PathBuf, Utf8PathBuf)> {
    let mut prefix = Utf8PathBuf::new();
    let parent = relative.parent()?;
    for component in parent.components() {
        prefix.push(component);
        let absolute = root.join(&prefix);
        let metadata = std::fs::symlink_metadata(&absolute).ok()?;
        if metadata.is_symlink() {
            let target = std::fs::read_link(&absolute)
                .ok()
                .and_then(|target| Utf8PathBuf::from_path_buf(target).ok())
                .unwrap_or_else(|| Utf8PathBuf::from("?"));
            return Some((prefix, target));
        }
        if !metadata.is_dir() {
            return None;
        }
    }
    None
}

/// Files a tracker owns inside a workspace, as (relative, absolute) pairs,
/// sorted by relative path. Missing paths are simply absent.
///
/// A symlink is listed as itself — the absolute path is the link, never what
/// it points at — and a symlink to a directory is not descended into, as
/// Git does. Callers that read content check [`std::fs::symlink_metadata`]
/// and must not follow it. A tracked path under a symlinked directory is
/// refused ([`NewgitError::SymlinkedParent`]).
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
        if let Some((link, target)) = symlinked_parent(root, relative) {
            return Err(NewgitError::SymlinkedParent {
                path: relative.clone(),
                link,
                target,
            });
        }
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
    // `symlink_metadata`, never `metadata`: a link is listed as itself and
    // never descended into, so nothing outside the root is ever walked.
    let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
        return Ok(());
    };
    if metadata.is_file() || metadata.is_symlink() {
        files.push((relative.to_path_buf(), absolute));
        return Ok(());
    }
    if !metadata.is_dir() {
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
        // A link is its target's text, as Git stores it: hashing what it
        // points at would read outside the workspace.
        let is_link = std::fs::symlink_metadata(absolute).is_ok_and(|meta| meta.is_symlink());
        let bytes = if is_link {
            let target =
                std::fs::read_link(absolute).map_err(|source| NewgitError::io(absolute, source))?;
            let mut bytes = b"\0symlink\0".to_vec();
            bytes.extend_from_slice(target.as_os_str().as_encoded_bytes());
            bytes
        } else {
            std::fs::read(absolute).map_err(|source| NewgitError::io(absolute, source))?
        };
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
            mount: Utf8PathBuf::from("."),
            follow: "main".to_owned(),
            claims: paths.iter().map(|path| (*path).to_owned()).collect(),
            exclusions: Vec::new(),
            git: false,
            maintainers: None,
            role: None,
            priority: 0,
            history: History::Commits,
            pinned: false,
            requires: Vec::new(),
            resources: Vec::new(),
            excluded: Vec::new(),
            parent: None,
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
    fn access_is_mounted_at_the_definitions_and_root_owns_the_rest() {
        let access = TrackerDefinition::access();
        assert!(access.carved() && access.mounted());
        assert_eq!(access.paths, vec![Utf8PathBuf::from(".newgit/trackers")]);
        assert!(!TrackerDefinition::root().carved() && TrackerDefinition::root().git);
        // Nobody else may own `.newgit`: the root tracker holds it.
        let error = TrackerDefinition::new(
            "sneaky",
            "project-devs".to_owned(),
            Storage::Local,
            false,
            vec![Utf8PathBuf::from(".newgit")],
        )
        .expect_err("`.newgit` is the root tracker's");
        assert!(error.to_string().contains("may not reach into"), "{error}");
    }

    #[test]
    fn claims_are_relative_to_the_mount_and_can_exclude() {
        let path = Utf8Path::new(".newgit/trackers/t3code.toml");
        let definition = TrackerDefinition::from_text(
            "t3code",
            path,
            "audience = \"public\"\nstorage = \"remote\"\ngit = true\nmount = \"apps/t3code\"\nfollow = \"newgit\"\npaths = [\".\", \"!node_modules/\"]\n",
        )
        .expect("a mounted definition");
        assert_eq!(definition.mount, Utf8PathBuf::from("apps/t3code"));
        assert_eq!(definition.follow, "newgit");
        assert_eq!(definition.paths, vec![Utf8PathBuf::from("apps/t3code")]);
        assert_eq!(
            definition.exclusions,
            vec![Utf8PathBuf::from("apps/t3code/node_modules")]
        );
        assert!(definition.owns(Utf8Path::new("apps/t3code/src/x.ts")));
        assert!(!definition.owns(Utf8Path::new("apps/t3code/node_modules/react/index.js")));
        // What it writes back is what it read.
        let text = toml::to_string_pretty(&definition.to_file()).expect("serializes");
        assert!(text.contains("mount = \"apps/t3code\""), "{text}");
        assert!(text.contains("\"!node_modules/\""), "{text}");

        let outside = TrackerDefinition::from_text(
            "env",
            path,
            "audience = \"user\"\nstorage = \"local\"\nmount = \"apps\"\npaths = [\".env\"]\n",
        )
        .expect("a tracker outside Git is mounted too, and never pinned");
        assert_eq!(outside.paths, vec![Utf8PathBuf::from("apps/.env")]);
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
