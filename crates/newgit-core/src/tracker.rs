use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Ownership {
    #[default]
    Branch,
    Workspace,
    Project,
    User,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Propagation {
    Pin,
    Recompute,
    #[default]
    Manual,
    Inherit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMode {
    None,
    Hash,
    Copy,
    Command,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreMode {
    None,
    Copy,
    Command,
    Recompute,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MaterializeRule {
    pub copy_from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct IdentityRule {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortRequest {
    pub start: u16,
    pub env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ActionDefinition {
    pub command: Option<String>,
    #[serde(default)]
    pub long_running: bool,
    pub signal: Option<String>,
    #[serde(default)]
    pub captures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureRule {
    pub mode: CaptureMode,
    #[serde(default)]
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub state_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoreRule {
    pub mode: RestoreMode,
    pub action: Option<String>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CleanupRule {
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerDefinition {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ownership: Ownership,
    #[serde(default)]
    pub propagation: Propagation,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub identity: Option<IdentityRule>,
    #[serde(default)]
    pub materialize: Option<MaterializeRule>,
    #[serde(default)]
    pub ports: BTreeMap<String, PortRequest>,
    #[serde(default)]
    pub actions: BTreeMap<String, ActionDefinition>,
    #[serde(default)]
    pub capture: Option<CaptureRule>,
    #[serde(default)]
    pub restore: Option<RestoreRule>,
    #[serde(default)]
    pub cleanup: Option<CleanupRule>,
    #[serde(default)]
    pub exports: BTreeMap<String, String>,
}

impl TrackerDefinition {
    pub fn validate(&self) -> Result<()> {
        validate_resource_name(&self.name)
    }

    pub fn fingerprint(&self) -> Result<String> {
        let rendered = toml::to_string(self).map_err(|source| NewgitError::TomlWrite {
            label: format!("tracker `{}`", self.name),
            source,
        })?;
        let digest = Sha256::digest(rendered.as_bytes());
        Ok(format!("sha256:{digest:x}"))
    }
}

pub fn validate_resource_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));

    if valid {
        Ok(())
    } else {
        Err(NewgitError::InvalidName(name.to_owned()))
    }
}

pub fn topological_order(definitions: &[TrackerDefinition]) -> Result<Vec<String>> {
    let by_name = definitions
        .iter()
        .map(|definition| (definition.name.as_str(), definition))
        .collect::<BTreeMap<_, _>>();
    let mut ordered = Vec::with_capacity(definitions.len());
    let mut temporary = BTreeSet::new();
    let mut permanent = BTreeSet::new();

    for definition in definitions {
        visit(
            definition.name.as_str(),
            &by_name,
            &mut temporary,
            &mut permanent,
            &mut ordered,
        )?;
    }

    Ok(ordered)
}

fn visit<'a>(
    name: &'a str,
    by_name: &BTreeMap<&'a str, &'a TrackerDefinition>,
    temporary: &mut BTreeSet<&'a str>,
    permanent: &mut BTreeSet<&'a str>,
    ordered: &mut Vec<String>,
) -> Result<()> {
    if permanent.contains(name) {
        return Ok(());
    }

    if !temporary.insert(name) {
        return Err(NewgitError::DependencyCycle(vec![name.to_owned()]));
    }

    let definition = by_name[name];
    for dependency in &definition.depends_on {
        let Some(_) = by_name.get(dependency.as_str()) else {
            return Err(NewgitError::MissingDependency {
                tracker: name.to_owned(),
                dependency: dependency.clone(),
            });
        };
        visit(dependency, by_name, temporary, permanent, ordered)?;
    }

    temporary.remove(name);
    permanent.insert(name);
    ordered.push(name.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(name: &str, depends_on: &[&str]) -> TrackerDefinition {
        TrackerDefinition {
            name: name.to_owned(),
            kind: "test".to_owned(),
            ownership: Ownership::Branch,
            propagation: Propagation::Manual,
            depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
            identity: None,
            materialize: None,
            ports: BTreeMap::new(),
            actions: BTreeMap::new(),
            capture: None,
            restore: None,
            cleanup: None,
            exports: BTreeMap::new(),
        }
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let ordered = topological_order(&[
            tracker("app", &["deps", "runtime-env"]),
            tracker("runtime-env", &[]),
            tracker("deps", &[]),
        ])
        .expect("order should resolve");

        assert_eq!(ordered, vec!["deps", "runtime-env", "app"]);
    }
}
