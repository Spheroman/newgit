use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};

/// A lifecycle unit that re-establishes per-branch state that can't travel
/// as content. Parsed from `.newgit/resources/<name>.toml`; the name comes
/// from the filename.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDefinition {
    pub name: String,
    pub kind: String,
    pub ownership: Ownership,
    pub depends_on: Vec<String>,
    pub identity: Option<IdentitySpec>,
    pub ports: BTreeMap<String, PortRequest>,
    pub exports: BTreeMap<String, String>,
    pub actions: BTreeMap<String, ActionSpec>,
    /// Parsed and recorded now; consumed by checkpoint/undo in M5 and
    /// cleanup in M6.
    pub checkpoint: Option<toml::Value>,
    pub restore: Option<toml::Value>,
    pub cleanup: Option<toml::Value>,
    /// `sha256:<hex12>` of the definition file contents.
    pub definition_rev: String,
}

/// Who owns the concrete instance and what cleanup may touch. Operational,
/// not a security label.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ownership {
    Branch,
    Workspace,
    Project,
    User,
    External,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct IdentitySpec {
    pub paths: Vec<Utf8PathBuf>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PortRequest {
    pub start: u16,
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ActionSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub long_running: bool,
    /// Signal sent by `stop` for a long-running sibling `start`.
    #[serde(default)]
    pub signal: Option<String>,
    /// Output values captured from the command (held for M6).
    #[serde(default)]
    pub captures: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ResourceDefinitionFile {
    kind: String,
    ownership: Ownership,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    identity: Option<IdentitySpec>,
    #[serde(default)]
    ports: BTreeMap<String, PortRequest>,
    #[serde(default)]
    exports: BTreeMap<String, String>,
    #[serde(default)]
    actions: BTreeMap<String, ActionSpec>,
    #[serde(default)]
    checkpoint: Option<toml::Value>,
    #[serde(default)]
    restore: Option<toml::Value>,
    #[serde(default)]
    cleanup: Option<toml::Value>,
}

impl ResourceDefinition {
    pub fn from_file(name: &str, path: &Utf8Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        let file: ResourceDefinitionFile =
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
            ownership: file.ownership,
            depends_on: file.depends_on,
            identity: file.identity,
            ports: file.ports,
            exports: file.exports,
            actions: file.actions,
            checkpoint: file.checkpoint,
            restore: file.restore,
            cleanup: file.cleanup,
            definition_rev: format!("sha256:{hex}"),
        };
        definition.validate()?;
        Ok(definition)
    }

    /// The action `stop` signals, resolved: explicit `signal`, default TERM.
    pub fn stop_signal(&self) -> String {
        self.actions
            .get("stop")
            .and_then(|action| action.signal.clone())
            .unwrap_or_else(|| "term".to_owned())
    }

    pub fn has_long_running_action(&self) -> bool {
        self.actions.values().any(|action| action.long_running)
    }

    fn validate(&self) -> Result<()> {
        for (action_name, action) in &self.actions {
            let is_signal_only = action.command.is_none() && action.signal.is_some();
            if action.command.is_none() && !is_signal_only {
                return Err(self.invalid(format!(
                    "action `{action_name}` has neither a command nor a signal"
                )));
            }
            if action.long_running && action.command.is_none() {
                return Err(self.invalid(format!(
                    "action `{action_name}` is long_running but has no command"
                )));
            }
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

/// Order resources so dependencies come before dependents. Dependencies may
/// name trackers (which only need to exist) or other resources.
pub fn topological_order(
    resources: &[ResourceDefinition],
    tracker_names: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut ordered = Vec::new();
    let mut state: BTreeMap<&str, Visit> = BTreeMap::new();

    fn visit<'a>(
        name: &'a str,
        resources: &'a [ResourceDefinition],
        tracker_names: &BTreeSet<String>,
        state: &mut BTreeMap<&'a str, Visit>,
        ordered: &mut Vec<String>,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        match state.get(name) {
            Some(Visit::Done) => return Ok(()),
            Some(Visit::InProgress) => {
                stack.push(name.to_owned());
                return Err(NewgitError::DependencyCycle(stack.clone()));
            }
            None => {}
        }
        let Some(resource) = resources.iter().find(|r| r.name == name) else {
            // Caller verified membership; only reachable for dependencies.
            return Ok(());
        };
        state.insert(&resource.name, Visit::InProgress);
        stack.push(name.to_owned());
        for dependency in &resource.depends_on {
            if tracker_names.contains(dependency) {
                continue;
            }
            if !resources.iter().any(|r| &r.name == dependency) {
                return Err(NewgitError::MissingDependency {
                    resource: resource.name.clone(),
                    dependency: dependency.clone(),
                });
            }
            visit(dependency, resources, tracker_names, state, ordered, stack)?;
        }
        stack.pop();
        state.insert(&resource.name, Visit::Done);
        ordered.push(resource.name.clone());
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum Visit {
        InProgress,
        Done,
    }

    for resource in resources {
        visit(
            &resource.name,
            resources,
            tracker_names,
            &mut state,
            &mut ordered,
            &mut Vec::new(),
        )?;
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    fn resource(name: &str, deps: &[&str]) -> ResourceDefinition {
        ResourceDefinition {
            name: name.to_owned(),
            kind: "command".to_owned(),
            ownership: Ownership::Branch,
            depends_on: deps.iter().map(ToString::to_string).collect(),
            identity: None,
            ports: BTreeMap::new(),
            exports: BTreeMap::new(),
            actions: BTreeMap::new(),
            checkpoint: None,
            restore: None,
            cleanup: None,
            definition_rev: "sha256:000000000000".to_owned(),
        }
    }

    #[test]
    fn orders_dependencies_first_and_detects_cycles() {
        let trackers = BTreeSet::from(["runtime-env".to_owned()]);
        let resources = vec![
            resource("app", &["deps", "runtime-env"]),
            resource("deps", &[]),
        ];
        let order = topological_order(&resources, &trackers).expect("order");
        assert_eq!(order, vec!["deps".to_owned(), "app".to_owned()]);

        let cyclic = vec![resource("a", &["b"]), resource("b", &["a"])];
        assert!(matches!(
            topological_order(&cyclic, &trackers),
            Err(NewgitError::DependencyCycle(_))
        ));

        let missing = vec![resource("app", &["nope"])];
        assert!(matches!(
            topological_order(&missing, &trackers),
            Err(NewgitError::MissingDependency { .. })
        ));
    }
}
