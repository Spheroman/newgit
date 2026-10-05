use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::expand_home;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub project: ProjectSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    pub name: String,
    pub source: SourceSubstrate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceSubstrate {
    Git,
    Jj,
}

/// Optional overrides; omitted from the generated config so a committed
/// file never bakes in one user's absolute paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Utf8PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materializer: Option<String>,
}

/// The newgit remote this project syncs to: a newgit Worker, which keeps
/// binding records and checkpoints and mints the Git credentials for the
/// project's repos. Committed — every member of the project uses the same one.
///
/// ```toml
/// [remote]
/// url = "https://newgit.example.workers.dev"
/// project = "acme"   # optional; defaults to [project] name
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSection {
    pub url: String,
    /// The project's name on the remote, which also names its repos. Only
    /// needed when it differs from `[project] name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl RemoteSection {
    /// Why this section cannot be used, if it cannot.
    pub fn problem(&self) -> Option<String> {
        let url = self.url.trim_end_matches('/');
        let local = ["http://localhost", "http://127.0.0.1"]
            .iter()
            .any(|prefix| url == *prefix || url.starts_with(&format!("{prefix}:")));
        if !url.starts_with("https://") && !local {
            return Some(format!(
                "`url = \"{}\"` must be https:// — newgit tokens travel over it (plain http \
                 is accepted only for localhost, for a local `wrangler dev`)",
                self.url
            ));
        }
        if let Some(project) = &self.project
            && !is_project_key(project)
        {
            return Some(format!(
                "`project = \"{project}\"` must be `<owner>/<name>` — like a GitHub repository — \
                 each part 1-63 letters, digits, `_`, or `-`, starting with a letter or digit"
            ));
        }
        None
    }
}

/// One part of a repo name on the remote: `.` separates the parts there.
pub fn is_remote_part(name: &str) -> bool {
    let mut chars = name.chars();
    (1..=63).contains(&name.len())
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// `<owner>/<name>`.
pub fn is_project_key(key: &str) -> bool {
    matches!(key.split_once('/'), Some((owner, name)) if is_remote_part(owner) && is_remote_part(name))
}

impl ProjectConfig {
    pub fn new(project_name: impl Into<String>, source: SourceSubstrate) -> Self {
        Self {
            project: ProjectSection {
                name: project_name.into(),
                source,
            },
            workspace: None,
            remote: None,
        }
    }

    /// The project's name on the remote, when one is configured.
    /// The project on the remote, `<owner>/<name>` — set once `newgit remote
    /// add` has created or joined it.
    pub fn remote_project(&self) -> Option<&str> {
        self.remote.as_ref()?.project.as_deref()
    }

    /// Where this project's workspaces live. Defaults to
    /// `~/.newgit/workspaces/<name>-<hash>/`, where the hash disambiguates
    /// same-named projects at different paths.
    pub fn workspace_root(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        if let Some(root) = self.workspace.as_ref().and_then(|ws| ws.root.as_ref()) {
            return expand_home(root);
        }
        default_workspace_root(&self.project.name, project_root)
    }
}

pub fn default_workspace_root(project_name: &str, project_root: &Utf8Path) -> Utf8PathBuf {
    let digest = Sha256::digest(project_root.as_str().as_bytes());
    let hash: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    expand_home(Utf8Path::new("~/.newgit/workspaces")).join(format!("{project_name}-{hash}"))
}

/// This person's own choice of tracker per role for `project`, from
/// `~/.newgit/config.toml`:
///
/// ```toml
/// [projects.faretable.roles]
/// db = "staging-db"
/// ```
///
/// It overrides a role's `priority`; newgit never writes it — it exists only
/// if you do. Unreadable or absent means none.
pub fn personal_role_defaults(project: &str) -> std::collections::BTreeMap<String, String> {
    #[derive(serde::Deserialize, Default)]
    struct Personal {
        #[serde(default)]
        projects: std::collections::BTreeMap<String, PersonalProject>,
    }
    #[derive(serde::Deserialize, Default)]
    struct PersonalProject {
        #[serde(default)]
        roles: std::collections::BTreeMap<String, String>,
    }
    let Ok(home) = std::env::var("HOME") else {
        return Default::default();
    };
    let path = std::path::Path::new(&home).join(".newgit/config.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Personal>(&text).ok())
        .and_then(|mut personal| personal.projects.remove(project))
        .map(|project| project.roles)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::{ProjectConfig, SourceSubstrate, default_workspace_root};

    #[test]
    fn default_root_disambiguates_same_named_projects() {
        let a = default_workspace_root("api", Utf8Path::new("/home/u/work/api"));
        let b = default_workspace_root("api", Utf8Path::new("/home/u/other/api"));
        assert_ne!(a, b);
    }

    #[test]
    fn workspace_override_wins() {
        let mut config = ProjectConfig::new("api", SourceSubstrate::Git);
        config.workspace = Some(super::WorkspaceSection {
            root: Some("/data/ws".into()),
            materializer: None,
        });
        assert_eq!(
            config.workspace_root(Utf8Path::new("/home/u/api")),
            Utf8Path::new("/data/ws")
        );
    }
}
