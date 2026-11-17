//! Where `newgit export --remote <name>` pushes: each tracker in Git says
//! where it ships with `export` in its own definition (docs/design/trackers.md,
//! *Git and GitHub*).
//!
//! GitHub is an export, not the project's remote: a target receives a
//! composed projection of trackers' `main`s, with their history, as an
//! ordinary Git repository. A tracker that declares a URL is the target's
//! root: the view is rooted at it, its own files at the top (rooted at
//! `root`, the project's own view). A tracker with `export = "inherit"` ships
//! wherever its parent ships, and one with no `export` never ships, so what
//! leaves fails closed. Definitions live in `access`, so widening where
//! something ships takes an owner's merge.

use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};
use crate::tracker::{ID_SEPARATOR, ROOT_TRACKER, TrackerDefinition, tracker_path};

/// One entry of a definition's `export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportEntry {
    /// `"inherit"`: wherever the parent's export goes — one place, several,
    /// or none.
    Inherit,
    /// A target declared here, rooted at this tracker.
    Target(ExportDeclaration),
}

/// A target as its declaring definition writes it: `"github.com/a/b"`, or
/// `{ url = "github.com/a/b", branch = "newgit", branches = true }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDeclaration {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub branches: bool,
    /// The target's name, when the repository's own name would not do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

const INHERIT: &str = "inherit";

/// `export` as the file writes it, in order: a string, a table, or an array
/// of those. `Err` says what is wrong, for the definition's error.
pub fn parse_export(value: &toml::Value) -> std::result::Result<Vec<ExportEntry>, String> {
    let entry = |value: &toml::Value| -> std::result::Result<ExportEntry, String> {
        match value {
            toml::Value::String(word) if word == INHERIT => Ok(ExportEntry::Inherit),
            toml::Value::String(url) => Ok(ExportEntry::Target(ExportDeclaration {
                url: url.clone(),
                branch: None,
                branches: false,
                name: None,
            })),
            toml::Value::Table(table) => ExportDeclaration::deserialize(table.clone())
                .map(ExportEntry::Target)
                .map_err(|error| format!("`export` table: {}", error.message())),
            other => Err(format!(
                "`export` entries are `\"inherit\"`, a URL, or a table with `url`; found a {}",
                other.type_str()
            )),
        }
    };
    let entries = match value {
        toml::Value::Array(items) => items
            .iter()
            .map(entry)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        one => vec![entry(one)?],
    };
    if entries.is_empty() {
        return Err(
            "`export = []` ships nowhere, which is what leaving `export` out says".to_owned(),
        );
    }
    if entries
        .iter()
        .filter(|entry| matches!(entry, ExportEntry::Inherit))
        .count()
        > 1
    {
        return Err("`export` says `\"inherit\"` twice".to_owned());
    }
    Ok(entries)
}

/// `export` as [`parse_export`] reads it back: one entry on its own, more as
/// an array. `None` for none, so the key is left out.
pub fn export_value(entries: &[ExportEntry]) -> Option<toml::Value> {
    let value = |entry: &ExportEntry| match entry {
        ExportEntry::Inherit => toml::Value::String(INHERIT.to_owned()),
        ExportEntry::Target(declaration)
            if declaration.branch.is_none()
                && !declaration.branches
                && declaration.name.is_none() =>
        {
            toml::Value::String(declaration.url.clone())
        }
        ExportEntry::Target(declaration) => {
            toml::Value::try_from(declaration).expect("a declaration is a table")
        }
    };
    match entries {
        [] => None,
        [one] => Some(value(one)),
        many => Some(toml::Value::Array(many.iter().map(value).collect())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTarget {
    /// The repository's name (`newgit` for `github.com/Spheroman/newgit`),
    /// or the declaration's `name`: the remote's name in each workspace and
    /// what `--remote` takes.
    pub name: String,
    /// As written: a Git URL, a path, or `<host>/<path>`.
    pub url: String,
    /// The branch the export is pushed to.
    pub branch: String,
    /// Instances' branches go here too, each time a workspace pushes one —
    /// so a pull request can be opened against the target.
    pub branches: bool,
    /// The view is rooted at this tracker, the one declaring the target:
    /// its own files at the top, and while it mounts no tracker in Git, its
    /// own history, ids and signatures intact. `None`: declared by `root`,
    /// the project's own view.
    pub root: Option<String>,
    /// Every tracker that ships: the declaring one and every tracker below
    /// it that inherits its export.
    pub trackers: Vec<String>,
}

impl ExportTarget {
    /// The tracker declaring it.
    pub fn declared_by(&self) -> &str {
        self.root.as_deref().unwrap_or(ROOT_TRACKER)
    }

    /// Whether tracker `name` ships.
    pub fn ships(&self, name: &str) -> bool {
        self.trackers.iter().any(|tracker| tracker == name)
    }

    /// Where Git pushes: `<host>/<path>` is `https://<host>/<path>.git`;
    /// anything else (a URL, an `scp`-style address, a path) is what Git
    /// already understands, used as written.
    pub fn push_url(&self) -> String {
        push_url(&self.url)
    }

    /// The `owner/repo` of a target on GitHub, as `gh -R` takes it.
    pub fn github_repo(&self) -> Option<String> {
        github_repo(&self.url)
    }
}

/// Every target the definitions declare, each with the trackers that ship
/// to it. `definitions` is every tracker of the project, outside Git too:
/// one of those with an `export` is refused when it is read, so here every
/// tracker with one is in Git.
pub fn resolve_targets(definitions: &[TrackerDefinition]) -> Result<Vec<ExportTarget>> {
    let mut targets: Vec<ExportTarget> = Vec::new();
    for definition in definitions.iter().filter(|definition| definition.git) {
        for entry in &definition.export {
            let ExportEntry::Target(declaration) = entry else {
                continue;
            };
            let target = declare(definition, declaration)?;
            if let Some(same) = targets.iter().find(|other| other.name == target.name) {
                return Err(invalid(
                    definition,
                    format!(
                        "`export` names a target `{}`, as `{}`'s does: give one a `name`",
                        target.name,
                        tracker_path(same.declared_by())
                    ),
                ));
            }
            if let Some(same) = targets
                .iter()
                .find(|other| other.push_url() == target.push_url())
            {
                return Err(invalid(
                    definition,
                    format!(
                        "`export` pushes to {}, as `{}`'s does: one repository is one view, so \
                         say `\"inherit\"` to ship inside it",
                        target.url,
                        tracker_path(same.declared_by())
                    ),
                ));
            }
            targets.push(target);
        }
    }
    for target in &mut targets {
        let declared_by = target.declared_by().to_owned();
        target.trackers = definitions
            .iter()
            .filter(|definition| definition.git && inherits(definition, &declared_by, definitions))
            .map(|definition| definition.name.clone())
            .collect();
    }
    Ok(targets)
}

/// The tracker whose export `"inherit"` takes: the one that defines it in its
/// own `.newgit/trackers/` (`t3code` for `t3code/repos`), and `root` for
/// what `access` defines.
pub fn export_parent(name: &str) -> Option<&str> {
    if name == ROOT_TRACKER {
        return None;
    }
    Some(
        name.rsplit_once(ID_SEPARATOR)
            .map_or(ROOT_TRACKER, |(parent, _)| parent),
    )
}

/// Whether `definition` ships in the target `declared_by` declares: it is
/// that tracker, or it inherits, all the way up to it.
fn inherits(definition: &TrackerDefinition, declared_by: &str, all: &[TrackerDefinition]) -> bool {
    let mut at = definition;
    loop {
        if at.name == declared_by {
            return true;
        }
        if !at.export.contains(&ExportEntry::Inherit) {
            return false;
        }
        let Some(parent) = export_parent(&at.name) else {
            return false;
        };
        let Some(found) = all.iter().find(|definition| definition.name == parent) else {
            return false;
        };
        at = found;
    }
}

fn declare(
    definition: &TrackerDefinition,
    declaration: &ExportDeclaration,
) -> Result<ExportTarget> {
    let url = declaration.url.trim();
    if url.is_empty() {
        return Err(invalid(
            definition,
            "`export` has an empty `url`".to_owned(),
        ));
    }
    if is_bare_owner_repo(url) {
        return Err(invalid(
            definition,
            format!(
                "`export` URL `{url}` could be GitHub's or a directory: write \
                 `github.com/{url}` for GitHub, or `./{url}` for a path"
            ),
        ));
    }
    let name = match &declaration.name {
        Some(name) => name.clone(),
        None => repo_name(url).ok_or_else(|| {
            invalid(
                definition,
                format!("`export` URL `{url}` has no repository name in it: give it a `name`"),
            )
        })?,
    };
    // The name is also the remote's name in a workspace, beside `origin`,
    // and a ref's component.
    if name == "origin" {
        return Err(invalid(
            definition,
            format!(
                "`export` to {url} would be named `origin`, a workspace's own remote: give it \
                 another `name`"
            ),
        ));
    }
    if !is_ref_name(&name) || name.contains('/') {
        return Err(invalid(
            definition,
            format!("`export` target name `{name}` is not a remote's name: give it a `name`"),
        ));
    }
    let branch = declaration
        .branch
        .clone()
        .unwrap_or_else(|| "main".to_owned());
    if !is_ref_name(&branch) {
        return Err(invalid(
            definition,
            format!("`export` branch `{branch}` is not a branch name"),
        ));
    }
    Ok(ExportTarget {
        name,
        url: url.to_owned(),
        branch,
        branches: declaration.branches,
        root: (definition.name != ROOT_TRACKER).then(|| definition.name.clone()),
        trackers: Vec::new(),
    })
}

fn invalid(definition: &TrackerDefinition, reason: String) -> NewgitError {
    NewgitError::InvalidDefinition {
        kind: "tracker",
        name: tracker_path(&definition.name),
        reason,
    }
}

/// What `git check-ref-format --branch` would refuse, where it matters here:
/// a branch that is not one, or an option in disguise.
fn is_ref_name(name: &str) -> bool {
    !(name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
}

/// `owner/repo`: one slash, no scheme, host, or path. Which it means is not
/// for newgit to guess.
fn is_bare_owner_repo(url: &str) -> bool {
    let Some((owner, repo)) = url.split_once('/') else {
        return false;
    };
    let part = |text: &str| {
        !text.is_empty()
            && !text.starts_with('.')
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    part(owner) && part(repo) && !is_host(owner)
}

/// `github.com`, `git.example.org`: a dot inside, nothing a path has.
fn is_host(text: &str) -> bool {
    text.contains('.')
        && !text.starts_with('.')
        && !text.ends_with('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c))
}

/// `<host>/<path>`, no scheme: `github.com/Spheroman/newgit`.
fn host_form(url: &str) -> Option<(&str, &str)> {
    if url.contains("://") || url.contains(':') {
        return None;
    }
    let (host, path) = url.split_once('/')?;
    (is_host(host) && !path.is_empty()).then_some((host, path))
}

/// See [`ExportTarget::push_url`].
pub fn push_url(url: &str) -> String {
    match host_form(url) {
        Some((host, path)) => {
            let path = path.trim_end_matches('/');
            if path.ends_with(".git") {
                format!("https://{host}/{path}")
            } else {
                format!("https://{host}/{path}.git")
            }
        }
        None => url.to_owned(),
    }
}

/// Whether `a` and `b` name the same repository, however each is spelled:
/// `github.com/a/b`, `https://github.com/a/b.git`, `git@github.com:a/b`, and
/// `ssh://git@github.com/a/b/` are one repository; a path is compared once
/// made absolute. Errs toward saying they are the same (case is ignored on a
/// host), since what asks refuses when they are.
pub fn same_repository(a: &str, b: &str) -> bool {
    repository_key(a) == repository_key(b)
}

/// One spelling for every way of writing a repository's address.
fn repository_key(url: &str) -> String {
    let url = push_url(url.trim());
    let bare = |path: &str| {
        let path = path.trim_end_matches('/');
        path.strip_suffix(".git").unwrap_or(path).to_owned()
    };
    let local = |path: &str| {
        let path = path.trim_end_matches('/');
        std::fs::canonicalize(path)
            .ok()
            .and_then(|path| path.to_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| path.to_owned())
    };
    if let Some(path) = url.strip_prefix("file://") {
        return local(path);
    }
    if let Some((_, rest)) = url.split_once("://") {
        // `[user@]host[:port]/path`.
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        let host = host.split_once(':').map_or(host, |(host, _)| host);
        return format!("{}/{}", host, bare(path)).to_lowercase();
    }
    // `[user@]host:path`, as Git reads it: a colon before any slash.
    if let Some((host, path)) = url.split_once(':')
        && !host.contains('/')
    {
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        return format!("{}/{}", host, bare(path.trim_start_matches('/'))).to_lowercase();
    }
    local(&url)
}

/// The `owner/repo` a URL is on GitHub, or `None` when it is elsewhere.
pub fn github_repo(url: &str) -> Option<String> {
    let rest = push_url(url);
    let rest = rest
        .strip_prefix("https://github.com/")
        .or_else(|| rest.strip_prefix("git@github.com:"))
        .or_else(|| rest.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| format!("{owner}/{repo}"))
}

/// The repository's own name: the last part of its path, without `.git`.
fn repo_name(url: &str) -> Option<String> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let last = path.rsplit(['/', ':']).next()?;
    let last = last.strip_suffix(".git").unwrap_or(last);
    (!last.is_empty() && last != "." && last != "..").then(|| last.to_owned())
}

/// A target's place in errors: the declaring tracker's definition file.
pub fn declared_in(target: &ExportTarget) -> String {
    let declared_by = target.declared_by();
    match declared_by.rsplit_once(ID_SEPARATOR) {
        Some((parent, leaf)) => {
            format!("`{}`'s .newgit/trackers/{leaf}.toml", tracker_path(parent))
        }
        None => format!(".newgit/trackers/{declared_by}.toml"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> std::result::Result<Vec<ExportEntry>, String> {
        let table: toml::Table = toml::from_str(text).expect("toml");
        parse_export(table.get("export").expect("export"))
    }

    #[test]
    fn one_repository_is_one_repository_however_it_is_spelled() {
        for spelling in [
            "github.com/Spheroman/newgit",
            "https://github.com/Spheroman/newgit.git",
            "https://github.com/spheroman/newgit/",
            "git@github.com:Spheroman/newgit.git",
            "ssh://git@github.com/Spheroman/newgit",
            "ssh://git@github.com:22/Spheroman/newgit.git",
        ] {
            assert!(
                same_repository("github.com/Spheroman/newgit", spelling),
                "{spelling}"
            );
        }
        assert!(!same_repository(
            "github.com/Spheroman/newgit",
            "github.com/Spheroman/other"
        ));
        assert!(!same_repository(
            "github.com/Spheroman/newgit",
            "gitlab.com/Spheroman/newgit"
        ));
        let temp = std::env::temp_dir();
        let temp = temp
            .to_str()
            .expect("a UTF-8 temp dir")
            .trim_end_matches('/');
        assert!(same_repository(temp, &format!("file://{temp}/.")));
    }

    #[test]
    fn host_form_is_https_and_everything_else_is_used_as_written() {
        assert_eq!(
            push_url("github.com/acme/widget"),
            "https://github.com/acme/widget.git"
        );
        assert_eq!(
            push_url("gitlab.example.org/group/sub/widget.git"),
            "https://gitlab.example.org/group/sub/widget.git"
        );
        for url in [
            "git@github.com:acme/widget.git",
            "https://gitlab.com/acme/widget.git",
            "../mirror.git",
            "./mirror",
            "/srv/git/widget.git",
        ] {
            assert_eq!(push_url(url), url);
        }
    }

    #[test]
    fn github_repos_are_read_from_every_spelling() {
        for url in [
            "github.com/Acme/widget",
            "https://github.com/Acme/widget.git",
            "git@github.com:Acme/widget.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("Acme/widget"), "{url}");
        }
        assert_eq!(github_repo("gitlab.com/acme/widget"), None);
        assert_eq!(github_repo("/srv/git/widget.git"), None);
    }

    #[test]
    fn a_target_is_named_after_its_repository() {
        assert_eq!(
            repo_name("github.com/Spheroman/newgit").as_deref(),
            Some("newgit")
        );
        assert_eq!(repo_name("git@github.com:a/b.git").as_deref(), Some("b"));
        assert_eq!(repo_name("/srv/git/mirror.git/").as_deref(), Some("mirror"));
        assert_eq!(repo_name("."), None);
    }

    #[test]
    fn every_spelling_of_export_reads() {
        assert_eq!(
            parsed("export = \"inherit\"").expect("reads"),
            vec![ExportEntry::Inherit]
        );
        let both = parsed(
            "export = [\"inherit\", { url = \"github.com/x/lib\", branch = \"dev\", branches = true }]",
        )
        .expect("reads");
        assert_eq!(both.len(), 2);
        assert_eq!(both[0], ExportEntry::Inherit);
        let ExportEntry::Target(lib) = &both[1] else {
            panic!("a target");
        };
        assert_eq!(lib.branch.as_deref(), Some("dev"));
        assert!(lib.branches);
        for refused in [
            "export = []",
            "export = [\"inherit\", \"inherit\"]",
            "export = 3",
            "export = { url = \"github.com/a/b\", trackers = [\"root\"] }",
        ] {
            assert!(parsed(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn export_writes_back_as_it_reads() {
        for text in [
            "export = \"inherit\"",
            "export = \"github.com/a/b\"",
            "export = [\"inherit\", { url = \"github.com/a/b\", branch = \"dev\", branches = true }]",
        ] {
            let entries = parsed(text).expect("reads");
            let mut table = toml::Table::new();
            table.insert(
                "export".to_owned(),
                export_value(&entries).expect("a value"),
            );
            let written = toml::to_string(&table).expect("writes");
            assert_eq!(parsed(&written).expect("reads"), entries, "{written}");
        }
    }

    #[test]
    fn bare_owner_repo_is_refused_and_a_branch_must_be_one() {
        assert!(is_bare_owner_repo("acme/widget"));
        assert!(!is_bare_owner_repo("github.com/acme"));
        assert!(!is_bare_owner_repo("../acme/widget"));
        assert!(is_ref_name("release/1.x"));
        for branch in ["", "-f", "a..b", "a b", "main.lock", "feature/", "a:b"] {
            assert!(!is_ref_name(branch), "`{branch}` should be refused");
        }
    }
}
