use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::tracker::TrackerDefinition;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectConfig {
    pub project: ProjectSection,
    pub workspace: WorkspaceSection,
    #[serde(default)]
    pub trackers: Vec<TrackerDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSection {
    pub name: String,
    pub source: SourceSubstrate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceSubstrate {
    Git,
    Jj,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSection {
    pub root: Utf8PathBuf,
    pub materializer: String,
}

impl ProjectConfig {
    pub fn new(project_name: impl Into<String>, source: SourceSubstrate) -> Self {
        let project_name = project_name.into();
        Self {
            project: ProjectSection {
                name: project_name.clone(),
                source,
            },
            workspace: WorkspaceSection {
                root: Utf8PathBuf::from(format!("~/.newgit/workspaces/{project_name}")),
                materializer: "real-dir".to_owned(),
            },
            trackers: Vec::new(),
        }
    }
}
