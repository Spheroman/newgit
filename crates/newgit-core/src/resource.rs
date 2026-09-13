use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

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
    pub checkpoint: Option<CheckpointSpec>,
    pub restore: Option<RestoreSpec>,
    pub cleanup: Option<CleanupSpec>,
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

impl Ownership {
    /// Whether tearing down one branch instance may touch the concrete
    /// resource. `project` is shared by the project's instances and `user`
    /// is shared beyond it, so per-branch teardown must leave both alone —
    /// this is the conservative half of the ownership table, and the reason
    /// a pnpm store survives `newgit remove`.
    pub fn per_branch_teardown_may_touch(self) -> bool {
        match self {
            Self::Branch | Self::Workspace | Self::External => true,
            Self::Project | Self::User => false,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::User => "user",
            Self::External => "external",
        }
    }
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
    /// Names to read out of the command's stdout and merge into this
    /// resource's binding exports — how a resource that mints an external
    /// handle (a preview id, a tunnel URL) publishes it. See
    /// [`parse_captures`] for the accepted output shapes.
    #[serde(default)]
    pub captures: Vec<String>,
}

/// How a resource tears its concrete instance down. Ownership decides
/// whether the hook may run at all; this decides what running it means.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CleanupSpec {
    /// May use `{{state_ref}}` (from the instance's latest checkpoint) and
    /// `{{exports.<name>}}` (from the binding).
    pub command: Option<String>,
}

/// How a resource captures branch-local state at checkpoint time.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CheckpointSpec {
    pub mode: CheckpointMode,
    /// `hash`: identity files whose content hash is the captured state.
    #[serde(default)]
    pub paths: Vec<Utf8PathBuf>,
    /// `command`: emits the state; trimmed stdout becomes the state ref.
    #[serde(default)]
    pub command: Option<String>,
    /// `command`: deposit `{{snapshot.path}}` into this tracker's lane.
    #[serde(default)]
    pub into_tracker: Option<String>,
    /// `external`: template for the opaque ref (may use `{{exports.<name>}}`).
    #[serde(default)]
    pub state_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CheckpointMode {
    None,
    Hash,
    Command,
    External,
}

/// How a resource re-establishes checkpointed state during undo.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RestoreSpec {
    pub mode: RestoreMode,
    /// `command`: may use `{{state_ref}}`.
    #[serde(default)]
    pub command: Option<String>,
    /// `recompute`: the action to re-run (default `prepare`).
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreMode {
    None,
    Command,
    Recompute,
    External,
}

impl RestoreSpec {
    /// The action a `recompute` restore re-runs.
    pub fn recompute_action(&self) -> &str {
        self.action.as_deref().unwrap_or("prepare")
    }
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
    checkpoint: Option<CheckpointSpec>,
    #[serde(default)]
    restore: Option<RestoreSpec>,
    #[serde(default)]
    cleanup: Option<CleanupSpec>,
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
        if let Some(checkpoint) = &self.checkpoint {
            match checkpoint.mode {
                CheckpointMode::None => {}
                CheckpointMode::Hash => {
                    if checkpoint.paths.is_empty() {
                        return Err(
                            self.invalid("checkpoint mode `hash` requires `paths`".to_owned())
                        );
                    }
                }
                CheckpointMode::Command => {
                    if checkpoint.command.is_none() {
                        return Err(
                            self.invalid("checkpoint mode `command` requires `command`".to_owned())
                        );
                    }
                }
                CheckpointMode::External => {
                    if checkpoint.state_ref.is_none() {
                        return Err(self.invalid(
                            "checkpoint mode `external` requires `state_ref`".to_owned(),
                        ));
                    }
                }
            }
        }
        if let Some(restore) = &self.restore {
            match restore.mode {
                RestoreMode::Command if restore.command.is_none() => {
                    return Err(
                        self.invalid("restore mode `command` requires `command`".to_owned())
                    );
                }
                RestoreMode::Recompute
                    if !self.actions.contains_key(restore.recompute_action()) =>
                {
                    return Err(self.invalid(format!(
                        "restore mode `recompute` re-runs action `{}`, which is not defined",
                        restore.recompute_action()
                    )));
                }
                _ => {}
            }
        }
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

/// What an action's stdout yielded against the names it declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captures {
    pub found: BTreeMap<String, String>,
    /// Declared names that stdout did not contain. Reported rather than
    /// silently dropped: a capture that never appears is almost always a bug
    /// in the command — most often noisy output on stdout, which belongs to
    /// newgit when `captures` is set — and the resource is otherwise marked
    /// ready with an empty handle nobody notices until something 401s.
    pub missing: Vec<String>,
}

/// Read an action's declared `captures` out of its stdout.
///
/// Two shapes are accepted, because both are what a real command already
/// emits: stdout whose first non-whitespace character is `{` is parsed as a
/// flat JSON object (`cloudctl ... --json`), and anything else is read as
/// `KEY=VALUE` lines (`echo PREVIEW_ID=pv_9`). Only declared names are
/// taken and JSON scalars are stringified. A name the command did not emit
/// is not an error — a resource may legitimately publish a handle only on
/// some runs — but it is always reported in `missing`.
pub fn parse_captures(stdout: &str, wanted: &[String]) -> Captures {
    if wanted.is_empty() {
        return Captures::default();
    }
    let trimmed = stdout.trim_start();

    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    if trimmed.starts_with('{') {
        if let Ok(serde_json::Value::Object(object)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            for (key, value) in object {
                if let Some(text) = json_scalar(&value) {
                    seen.insert(key, text);
                }
            }
        }
    } else {
        for line in stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                seen.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
    }

    let mut captures = Captures::default();
    for name in wanted {
        match seen.remove_entry(name) {
            Some((key, value)) => {
                captures.found.insert(key, value);
            }
            None => captures.missing.push(name.clone()),
        }
    }
    captures
}

/// JSON scalars render as themselves; containers have no obvious env-var
/// spelling, so they are skipped rather than guessed at.
fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// A dependency graph that does not hold together. Reported rather than
/// raised, because the commands that build the graph are the ones most likely
/// to run while it is still incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphProblem {
    MissingDependency {
        resource: String,
        dependency: String,
    },
    Cycle(Vec<String>),
}

impl GraphProblem {
    /// The error a graph-acting command raises when it meets this problem.
    pub fn into_error(self) -> NewgitError {
        match self {
            Self::MissingDependency {
                resource,
                dependency,
            } => NewgitError::MissingDependency {
                resource,
                dependency,
            },
            Self::Cycle(stack) => NewgitError::DependencyCycle(stack),
        }
    }
}

impl fmt::Display for GraphProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.clone().into_error())
    }
}

/// Order resources so dependencies come before dependents. Dependencies may
/// name trackers (which only need to exist) or other resources.
///
/// Never fails: an unresolvable dependency is skipped and reported, so a
/// half-built graph still loads. Commands that act on the graph must check the
/// reported problems first; commands that build it may proceed and warn.
pub fn resolve_order(
    resources: &[ResourceDefinition],
    tracker_names: &BTreeSet<String>,
) -> (Vec<String>, Vec<GraphProblem>) {
    let mut ordered = Vec::new();
    let mut problems = Vec::new();
    let mut state: BTreeMap<&str, Visit> = BTreeMap::new();

    fn visit<'a>(
        name: &'a str,
        resources: &'a [ResourceDefinition],
        tracker_names: &BTreeSet<String>,
        state: &mut BTreeMap<&'a str, Visit>,
        ordered: &mut Vec<String>,
        problems: &mut Vec<GraphProblem>,
        stack: &mut Vec<String>,
    ) {
        match state.get(name) {
            Some(Visit::Done) => return,
            Some(Visit::InProgress) => {
                // Report the back-edge and stop descending; the resource is
                // already on the stack and will still be ordered by its caller.
                let mut cycle = stack.clone();
                cycle.push(name.to_owned());
                problems.push(GraphProblem::Cycle(cycle));
                return;
            }
            None => {}
        }
        let Some(resource) = resources.iter().find(|r| r.name == name) else {
            // Caller verified membership; only reachable for dependencies.
            return;
        };
        state.insert(&resource.name, Visit::InProgress);
        stack.push(name.to_owned());
        for dependency in &resource.depends_on {
            if tracker_names.contains(dependency) {
                continue;
            }
            if !resources.iter().any(|r| &r.name == dependency) {
                problems.push(GraphProblem::MissingDependency {
                    resource: resource.name.clone(),
                    dependency: dependency.clone(),
                });
                continue;
            }
            visit(
                dependency,
                resources,
                tracker_names,
                state,
                ordered,
                problems,
                stack,
            );
        }
        stack.pop();
        state.insert(&resource.name, Visit::Done);
        ordered.push(resource.name.clone());
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
            &mut problems,
            &mut Vec::new(),
        );
    }
    (ordered, problems)
}

/// [`resolve_order`] for callers that require a whole graph.
pub fn topological_order(
    resources: &[ResourceDefinition],
    tracker_names: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let (ordered, problems) = resolve_order(resources, tracker_names);
    match problems.into_iter().next() {
        Some(problem) => Err(problem.into_error()),
        None => Ok(ordered),
    }
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
    fn captures_read_json_objects_and_key_value_lines() {
        let wanted = ["PREVIEW_ID".to_owned(), "PREVIEW_URL".to_owned()];

        let json = parse_captures(
            r#"{"PREVIEW_ID": "pv_9", "PREVIEW_URL": "https://pv9.example", "extra": 1}"#,
            &wanted,
        );
        assert_eq!(json.found["PREVIEW_ID"], "pv_9");
        assert_eq!(json.found["PREVIEW_URL"], "https://pv9.example");
        assert_eq!(json.found.len(), 2, "undeclared keys are not captured");
        assert!(json.missing.is_empty());

        let lines = parse_captures("noise\nPREVIEW_ID=pv_9\n", &wanted);
        assert_eq!(lines.found["PREVIEW_ID"], "pv_9");
        assert!(
            !lines.found.contains_key("PREVIEW_URL"),
            "a name the command did not emit is absent, not empty"
        );
        assert_eq!(
            lines.missing,
            vec!["PREVIEW_URL".to_owned()],
            "and it is reported, not silently dropped"
        );

        // Non-string scalars stringify; unparseable output captures nothing.
        assert_eq!(
            parse_captures(r#"{"PORT": 5432}"#, &["PORT".to_owned()]).found["PORT"],
            "5432"
        );
        let unparseable = parse_captures("{not json", &wanted);
        assert!(unparseable.found.is_empty());
        assert_eq!(
            unparseable.missing, wanted,
            "every declared name is missing"
        );

        // Nothing declared means nothing wanted, so nothing is missing either.
        let undeclared = parse_captures("PREVIEW_ID=pv_9", &[]);
        assert!(undeclared.found.is_empty() && undeclared.missing.is_empty());
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
