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

impl ProjectConfig {
    pub fn new(project_name: impl Into<String>, source: SourceSubstrate) -> Self {
        Self {
            project: ProjectSection {
                name: project_name.into(),
                source,
            },
            workspace: None,
        }
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
