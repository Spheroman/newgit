use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};
use crate::render::RenderSpec;

/// A lifecycle unit that re-establishes per-branch state that can't travel
/// as content. Parsed from `.newgit/resources/<name>.toml`; the name comes
/// from the filename.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDefinition {
    pub name: String,
    pub ownership: Ownership,
    pub depends_on: Vec<String>,
    pub identity: Option<IdentitySpec>,
    /// Where every command this resource runs is spawned, relative to the
    /// workspace root. Applied as the child process's `current_dir`, never
    /// as a shell prefix — see [`ActionSpec::workdir`] for the per-action
    /// override. Content paths (`[identity].paths`, `[[render]].path`) are
    /// unaffected: they stay workspace-root-relative
    /// regardless of `workdir`, so a definition never has to reason about
    /// two roots at once.
    pub workdir: Option<Utf8PathBuf>,
    pub ports: BTreeMap<String, PortRequest>,
    pub exports: BTreeMap<String, String>,
    /// Files this resource substitutes per-instance values into, before
    /// `prepare`. See [`crate::render`].
    pub render: Vec<RenderSpec>,
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
#[serde(deny_unknown_fields)]
pub struct IdentitySpec {
    pub paths: Vec<Utf8PathBuf>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortRequest {
    pub start: u16,
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    #[serde(default)]
    pub command: Option<String>,
    /// Replaces (does not append to) the resource-level `workdir` for this
    /// action only.
    #[serde(default)]
    pub workdir: Option<Utf8PathBuf>,
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
#[serde(deny_unknown_fields)]
pub struct CleanupSpec {
    /// May use `{{state_ref}}` (from the instance's latest checkpoint) and
    /// `{{exports.<name>}}` (from the binding).
    pub command: Option<String>,
}

/// How a resource captures branch-local state at checkpoint time.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointSpec {
    pub mode: CheckpointMode,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct ResourceDefinitionFile {
    ownership: Ownership,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    identity: Option<IdentitySpec>,
    #[serde(default)]
    workdir: Option<Utf8PathBuf>,
    #[serde(default)]
    ports: BTreeMap<String, PortRequest>,
    #[serde(default)]
    exports: BTreeMap<String, String>,
    #[serde(default)]
    render: Vec<RenderSpec>,
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
            ownership: file.ownership,
            depends_on: file.depends_on,
            identity: file.identity,
            workdir: file.workdir,
            ports: file.ports,
            exports: file.exports,
            render: file.render,
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

    /// The `workdir` in effect for one command: an action's own `workdir`
    /// replaces the resource-level one entirely rather than nesting under
    /// it. `action` is `None` for hooks that are not actions (`checkpoint`,
    /// `restore`, `cleanup`), which only ever see the resource-level value.
    pub fn workdir_for<'a>(&'a self, action: Option<&'a ActionSpec>) -> Option<&'a Utf8Path> {
        action
            .and_then(|action| action.workdir.as_deref())
            .or(self.workdir.as_deref())
    }

    /// The paths whose content this resource is derived from — the single
    /// declaration a `hash` checkpoint captures and a `recompute` restore
    /// compares against.
    pub fn identity_paths(&self) -> &[Utf8PathBuf] {
        self.identity
            .as_ref()
            .map(|spec| spec.paths.as_slice())
            .unwrap_or(&[])
    }

    fn validate(&self) -> Result<()> {
        self.check_identity_paths()?;
        if let Some(checkpoint) = &self.checkpoint {
            match checkpoint.mode {
                CheckpointMode::None => {}
                // `hash` captures what the resource is derived *from*, which
                // is what `[identity]` declares. It has no path list of its
                // own: two places stating the same fact is how they come to
                // disagree, and a checkpoint describing different inputs than
                // identity claims would be wrong in the direction nobody
                // checks.
                CheckpointMode::Hash => {
                    if self
                        .identity
                        .as_ref()
                        .is_none_or(|spec| spec.paths.is_empty())
                    {
                        return Err(self.invalid(
                            "checkpoint mode `hash` records a hash of `[identity] paths`, which this resource does not declare"
                                .to_owned(),
                        ));
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
        self.check_checkpoint_restore_pairing()?;
        if let Some(workdir) = &self.workdir {
            self.check_workdir_is_workspace_relative(workdir)?;
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
            if let Some(workdir) = &action.workdir {
                self.check_workdir_is_workspace_relative(workdir)?;
            }
        }
        for spec in &self.render {
            if spec.replace.is_empty() {
                return Err(self.invalid(format!(
                    "render into `{}` declares no replacements",
                    spec.path
                )));
            }
            // A render target is workspace-relative. v1 does not render into
            // user-level or system config: the skip-worktree and
            // reverse-on-capture story only holds inside a workspace.
            if spec.path.is_absolute()
                || spec
                    .path
                    .components()
                    .any(|part| part.as_str() == ".." || part.as_str() == ".newgit")
            {
                return Err(self.invalid(format!(
                    "render path `{}` must be workspace-relative and outside `.newgit/`",
                    spec.path
                )));
            }
            for replacement in &spec.replace {
                if replacement.find.is_empty() {
                    return Err(
                        self.invalid(format!("render into `{}` has an empty `find`", spec.path))
                    );
                }
                if replacement.count == 0 {
                    return Err(self.invalid(format!(
                        "render into `{}` declares `count = 0` for `{}`; a replacement that \
                         matches nothing is a definition that does nothing",
                        spec.path, replacement.find
                    )));
                }
            }
        }
        Ok(())
    }

    /// `workdir` is resolved against the workspace root at run time
    /// ([`crate::manager`]), never against the store; an absolute path or a
    /// `..` component would silently escape that root, so both are refused
    /// here rather than left to whatever the shell does with them.
    /// Identity paths name content inside the workspace, so they carry the
    /// same restrictions tracker paths do. Without this an `[identity]`
    /// could hash `/etc/passwd` or climb out with `..`, which a checkpoint
    /// would then faithfully record.
    fn check_identity_paths(&self) -> Result<()> {
        let Some(identity) = &self.identity else {
            return Ok(());
        };
        for path in &identity.paths {
            if path.is_absolute()
                || path.as_str().is_empty()
                || path.components().any(|part| part.as_str() == "..")
            {
                return Err(
                    self.invalid(format!("identity path `{path}` must be workspace-relative"))
                );
            }
            let first = path
                .components()
                .next()
                .map(|part| part.as_str().to_owned());
            if matches!(first.as_deref(), Some(".git" | ".newgit")) {
                return Err(self.invalid(format!(
                    "identity path `{path}` may not reach into `{}`",
                    first.unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    /// `[checkpoint]` and `[restore]` are two halves of one mechanism: the
    /// checkpoint records a state ref and the restore is what consumes it.
    /// Checked only per section, all sixteen pairings load, and three of them
    /// cannot mean anything — the worst hands a restore command a content
    /// hash where it expected a handle, which is not a no-op, it is a wrong
    /// argument. They are refused here, when the definition is written,
    /// rather than during the undo someone is relying on.
    ///
    /// The rest are left alone. A `command` checkpoint with a `recompute`
    /// restore ignores the ref it recorded, and a `hash` checkpoint with an
    /// `external` restore rewinds nothing, but both are inert rather than
    /// wrong: the record still reads back in `newgit checkpoints`.
    fn check_checkpoint_restore_pairing(&self) -> Result<()> {
        let Some(restore) = &self.restore else {
            return Ok(());
        };
        // An absent `[checkpoint]` records exactly what `mode = "none"` does.
        let checkpoint = self
            .checkpoint
            .as_ref()
            .map_or(CheckpointMode::None, |spec| spec.mode);

        match (checkpoint, restore.mode) {
            (CheckpointMode::Hash, RestoreMode::Command) => Err(self.invalid(
                "checkpoint mode `hash` records a content hash of `[identity] paths`, which \
                 restore mode `command` cannot use as an argument — a hash identifies inputs, \
                 never a thing to restore. Pair `hash` with `recompute`, which rebuilds from \
                 those inputs, or with `none`."
                    .to_owned(),
            )),
            (CheckpointMode::External, RestoreMode::Recompute) => Err(self.invalid(
                "checkpoint mode `external` records a handle to state another system owns, and \
                 restore mode `recompute` never reads it — it re-runs an action locally. Pair \
                 `external` with `command`, which receives the handle as `{{state_ref}}`, or \
                 with `external`, which leaves the other system alone."
                    .to_owned(),
            )),
            (CheckpointMode::None, RestoreMode::Command)
                if restore
                    .command
                    .as_deref()
                    .is_some_and(|command| command.contains("{{state_ref}}")) =>
            {
                Err(self.invalid(format!(
                    "restore command uses `{{{{state_ref}}}}`, but {} — there is never a ref to \
                     interpolate. Record one with a `command` or `external` checkpoint, or drop \
                     the placeholder.",
                    match &self.checkpoint {
                        Some(_) => "checkpoint mode `none` records nothing",
                        None => "this resource declares no `[checkpoint]`",
                    }
                )))
            }
            _ => Ok(()),
        }
    }

    fn check_workdir_is_workspace_relative(&self, workdir: &Utf8Path) -> Result<()> {
        if workdir.is_absolute() || workdir.components().any(|part| part.as_str() == "..") {
            return Err(self.invalid(format!(
                "workdir `{workdir}` must be a workspace-relative path with no `..`"
            )));
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
            ownership: Ownership::Branch,
            depends_on: deps.iter().map(ToString::to_string).collect(),
            identity: None,
            workdir: None,
            ports: BTreeMap::new(),
            exports: BTreeMap::new(),
            render: Vec::new(),
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

    fn write_and_load(contents: &str) -> Result<ResourceDefinition> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(temp.path().join("db.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write definition");
        ResourceDefinition::from_file("db", &path)
    }

    /// A misspelling is the case that matters: `ownership` decides whether
    /// per-branch teardown may delete the concrete resource, so a silently
    /// ignored `owneship` is the difference between `newgit remove` tearing
    /// down a dev server and it reaching a shared store.
    #[test]
    fn a_misspelled_top_level_key_is_rejected_rather_than_ignored() {
        let error = write_and_load(
            r#"owneship = "branch"

[actions.start]
command = "npm run dev"
"#,
        )
        .expect_err("a key newgit does not understand is an error");
        let message = error.to_string() + &format!("{:?}", error);
        assert!(message.contains("owneship"), "names the offending key");
        assert!(message.contains("ownership"), "names the valid keys");
    }

    /// Nested sections deny too, not just the top level: `long_runing` on an
    /// action would otherwise leave a supervised process silently unsupervised.
    #[test]
    fn a_misspelled_key_inside_a_section_is_rejected() {
        let error = write_and_load(
            r#"ownership = "branch"

[actions.start]
command = "npm run dev"
long_runing = true
"#,
        )
        .expect_err("an unknown key in an action is an error");
        let message = error.to_string() + &format!("{:?}", error);
        assert!(message.contains("long_runing"));
        assert!(message.contains("long_running"));
    }

    /// The removal of `kind` (#27) left stale definitions parsing happily,
    /// which is how it survived two rebases unnoticed. It is now loud.
    #[test]
    fn a_key_newgit_no_longer_understands_is_rejected() {
        let error = write_and_load(
            r#"kind = "process"
ownership = "branch"
"#,
        )
        .expect_err("a removed key is an error, not a no-op");
        assert!((error.to_string() + &format!("{:?}", error)).contains("kind"));
    }

    #[test]
    fn an_action_workdir_replaces_rather_than_nests_under_the_resource_level_one() {
        let definition = write_and_load(
            r#"ownership = "workspace"
workdir = "packages/db"

[actions.prepare]
command = "true"

[actions.migrate]
workdir = "packages/db/supabase"
command = "true"
"#,
        )
        .expect("valid definition");

        assert_eq!(
            definition.workdir_for(definition.actions.get("prepare")),
            Some(Utf8Path::new("packages/db")),
            "an action with no workdir of its own falls back to the resource-level one"
        );
        assert_eq!(
            definition.workdir_for(definition.actions.get("migrate")),
            Some(Utf8Path::new("packages/db/supabase")),
            "an action's own workdir replaces the resource-level one, not nests under it"
        );
        assert_eq!(
            definition.workdir_for(None),
            Some(Utf8Path::new("packages/db")),
            "hooks that are not actions (checkpoint/restore/cleanup) see only the resource-level workdir"
        );
    }

    #[test]
    fn a_workdir_escaping_the_workspace_is_refused() {
        for workdir in ["../outside", "/etc"] {
            let result = write_and_load(&format!(
                r#"ownership = "workspace"
workdir = "{workdir}"

[actions.prepare]
command = "true"
"#
            ));
            assert!(
                matches!(result, Err(NewgitError::InvalidDefinition { .. })),
                "workdir `{workdir}` should have been refused, got {result:?}"
            );
        }

        let result = write_and_load(
            r#"ownership = "workspace"

[actions.prepare]
command = "true"
workdir = "../outside"
"#,
        );
        assert!(
            matches!(result, Err(NewgitError::InvalidDefinition { .. })),
            "an action-level workdir is held to the same rule: {result:?}"
        );
    }

    /// The pairing that matters: `hash` + `command` used to load, and then
    /// handed the restore command `hash:0fa284b46875` — a content hash where
    /// it expected something to restore.
    #[test]
    fn a_checkpoint_mode_a_restore_cannot_consume_is_refused() {
        let error = write_and_load(
            r#"ownership = "branch"

[identity]
paths = ["package-lock.json"]

[actions.prepare]
command = "npm ci"

[checkpoint]
mode = "hash"

[restore]
mode = "command"
command = "restore-from {{state_ref}}"
"#,
        )
        .expect_err("a hash ref is not something a command can restore from");
        let message = error.to_string();
        assert!(message.contains("hash"), "names the checkpoint mode");
        assert!(message.contains("recompute"), "names the fix: {message}");

        // `recompute` re-runs an action locally and never looks at the handle,
        // so an `external` checkpoint has nothing to say to it.
        let error = write_and_load(
            r#"ownership = "external"

[actions.prepare]
command = "cloudctl preview create"

[checkpoint]
mode = "external"
state_ref = "{{exports.PREVIEW_ID}}"

[restore]
mode = "recompute"
"#,
        )
        .expect_err("recompute ignores an external handle");
        assert!(error.to_string().contains("external"));

        // Nothing records a ref, so `{{state_ref}}` can never resolve.
        let error = write_and_load(
            r#"ownership = "branch"

[actions.prepare]
command = "true"

[restore]
mode = "command"
command = "restore-from {{state_ref}}"
"#,
        )
        .expect_err("a restore command cannot interpolate a ref nothing records");
        assert!(error.to_string().contains("[checkpoint]"));
    }

    /// The pairings that do mean something keep loading — including the inert
    /// ones, which record a ref nobody reads but are not wrong.
    #[test]
    fn the_checkpoint_and_restore_pairings_that_mean_something_still_load() {
        let ok = |checkpoint: &str, restore: &str| {
            let contents = format!(
                r#"ownership = "branch"

[identity]
paths = ["package-lock.json"]

[actions.prepare]
command = "true"

[checkpoint]
{checkpoint}

[restore]
{restore}
"#
            );
            let result = write_and_load(&contents);
            assert!(result.is_ok(), "{checkpoint} + {restore}: {result:?}");
        };

        ok("mode = \"hash\"", "mode = \"recompute\"");
        ok("mode = \"hash\"", "mode = \"none\"");
        ok("mode = \"hash\"", "mode = \"external\"");
        ok(
            "mode = \"command\"\ncommand = \"pg_dump\"",
            "mode = \"command\"\ncommand = \"psql < {{state_ref}}\"",
        );
        ok(
            "mode = \"command\"\ncommand = \"pg_dump\"",
            "mode = \"recompute\"",
        );
        ok(
            "mode = \"external\"\nstate_ref = \"pv_9\"",
            "mode = \"command\"\ncommand = \"cloudctl restore {{state_ref}}\"",
        );

        // A `none` checkpoint only refuses a restore command that asks for a
        // ref; one that does not is a perfectly ordinary rebuild.
        ok("mode = \"none\"", "mode = \"command\"\ncommand = \"true\"");
    }
}
