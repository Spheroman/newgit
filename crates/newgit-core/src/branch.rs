use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::port::allocate_ports;
use crate::tracker::{TrackerDefinition, topological_order, validate_resource_name};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchInstance {
    pub id: String,
    pub name: String,
    pub source_ref: Option<String>,
    pub source_rev: Option<String>,
    pub workspace_path: Utf8PathBuf,
    #[serde(default)]
    pub trackers: BTreeMap<String, TrackerBinding>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerBinding {
    pub tracker_name: String,
    pub definition_rev: String,
    pub state_ref: Option<String>,
    #[serde(default)]
    pub resolved_ports: BTreeMap<String, u16>,
    #[serde(default)]
    pub resolved_exports: BTreeMap<String, String>,
    pub status: BindingStatus,
    pub last_checkpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BindingStatus {
    Pending,
    Ready,
    Running,
    Failed,
}

impl BranchInstance {
    pub fn new(
        name: impl Into<String>,
        workspace_path: Utf8PathBuf,
        tracker_definitions: &[TrackerDefinition],
    ) -> Result<Self> {
        let name = name.into();
        validate_resource_name(&name)?;

        let now = Utc::now();
        let mut trackers = BTreeMap::new();
        let mut allocated_ports = allocate_ports(&name, tracker_definitions);
        let ordered_names = topological_order(tracker_definitions)?;

        for tracker_name in ordered_names {
            let definition = tracker_definitions
                .iter()
                .find(|candidate| candidate.name == tracker_name)
                .expect("topological order only returns known definitions");
            let resolved_ports = allocated_ports.remove(&definition.name).unwrap_or_default();
            let resolved_exports = render_exports(&name, &resolved_ports, &definition.exports);
            let binding = TrackerBinding {
                tracker_name: definition.name.clone(),
                definition_rev: definition.fingerprint()?,
                state_ref: None,
                resolved_ports,
                resolved_exports,
                status: BindingStatus::Pending,
                last_checkpoint: None,
            };
            trackers.insert(definition.name.clone(), binding);
        }

        Ok(Self {
            id: format!("br_{}_{}", branch_slug(&name), now.timestamp_millis()),
            name,
            source_ref: None,
            source_rev: None,
            workspace_path,
            trackers,
            created_at: now,
            updated_at: now,
        })
    }
}

pub fn branch_slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut previous_dash = false;

    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_matches('-').to_owned()
}

fn render_exports(
    branch_name: &str,
    ports: &BTreeMap<String, u16>,
    exports: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    exports
        .iter()
        .map(|(key, value)| {
            let mut rendered = value
                .replace("{{branch.name}}", branch_name)
                .replace("{{branch.slug}}", &branch_slug(branch_name));

            for (port_name, port) in ports {
                rendered =
                    rendered.replace(&format!("{{{{ports.{port_name}}}}}"), &port.to_string());
            }

            (key.clone(), rendered)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use camino::Utf8PathBuf;

    use crate::tracker::{Ownership, PortRequest, Propagation, TrackerDefinition};

    use super::BranchInstance;

    #[test]
    fn branch_instance_binds_trackers_with_resolved_exports() {
        let mut ports = BTreeMap::new();
        ports.insert(
            "app".to_owned(),
            PortRequest {
                start: 3100,
                env: Some("PORT".to_owned()),
            },
        );

        let mut exports = BTreeMap::new();
        exports.insert(
            "APP_URL".to_owned(),
            "http://127.0.0.1:{{ports.app}}/{{branch.slug}}".to_owned(),
        );

        let definitions = vec![TrackerDefinition {
            name: "app".to_owned(),
            kind: "process".to_owned(),
            ownership: Ownership::Branch,
            propagation: Propagation::Pin,
            depends_on: Vec::new(),
            identity: None,
            materialize: None,
            ports,
            actions: BTreeMap::new(),
            capture: None,
            restore: None,
            cleanup: None,
            exports,
        }];

        let branch = BranchInstance::new(
            "auth-refactor",
            Utf8PathBuf::from("/tmp/newgit/auth-refactor"),
            &definitions,
        )
        .expect("branch should be valid");
        let binding = branch.trackers.get("app").expect("binding exists");

        assert!(binding.resolved_exports["APP_URL"].contains("auth-refactor"));
        assert!(binding.resolved_exports["APP_URL"].contains("http://127.0.0.1:"));
    }
}
