use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};
use crate::exports::STATE_REF_PLACEHOLDER;
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
        if matches!(self.checkpoint_mode(), CheckpointMode::External)
            && self
                .restore
                .as_ref()
                .is_some_and(|restore| restore.mode == RestoreMode::Recompute)
        {
            return Err(self.invalid(
                "checkpoint mode `external` records a handle to state another system owns, and \
                 restore mode `recompute` does not restore it — it re-runs an action locally, \
                 which for an external resource mints a *second* instance and orphans the one \
                 the handle names. Pair `external` with `command`, which receives the handle as \
                 `{{state_ref}}`, or with `external`, which leaves the other system alone."
                    .to_owned(),
            ));
        }
        self.check_state_ref_is_recordable()
    }

    /// An absent `[checkpoint]` records exactly what `mode = "none"` does.
    fn checkpoint_mode(&self) -> CheckpointMode {
        self.checkpoint
            .as_ref()
            .map_or(CheckpointMode::None, |spec| spec.mode)
    }

    /// `{{state_ref}}` in a restore or cleanup command, under a checkpoint
    /// that can never record one for it.
    ///
    /// Two modes never produce a ref a command can be handed. `none` records
    /// nothing at all. `hash` records the content hash of `[identity] paths`,
    /// which answers whether the inputs moved and never names a concrete
    /// thing to restore or tear down — `restore-from hash:0fa284b468` is not
    /// a no-op, it is a wrong argument.
    ///
    /// Both are decidable from the definition alone, so both are refused
    /// where the mistake was made. The guard that refuses a cleanup hook with
    /// an unresolved placeholder still stands behind this: it catches the
    /// same mistake in a record written under an older definition, which no
    /// amount of reading the current one can predict.
    fn check_state_ref_is_recordable(&self) -> Result<()> {
        let hint = match self.checkpoint_mode() {
            CheckpointMode::None if self.checkpoint.is_some() => {
                "checkpoint mode `none` records nothing — there is never a ref to interpolate. \
                 Record one with a `command` or `external` checkpoint, or drop the placeholder."
            }
            CheckpointMode::None => {
                "this resource declares no `[checkpoint]` — there is never a ref to interpolate. \
                 Record one with a `command` or `external` checkpoint, or drop the placeholder."
            }
            CheckpointMode::Hash => {
                "checkpoint mode `hash` records a content hash of `[identity] paths`, which \
                 identifies inputs and never a thing to act on. Rebuild from those inputs with \
                 restore mode `recompute`, or drop the placeholder."
            }
            CheckpointMode::Command | CheckpointMode::External => return Ok(()),
        };

        let restore_command = self
            .restore
            .as_ref()
            .filter(|restore| restore.mode == RestoreMode::Command)
            .and_then(|restore| restore.command.as_deref());
        let cleanup_command = self
            .cleanup
            .as_ref()
            .and_then(|cleanup| cleanup.command.as_deref());

        for (section, command) in [("restore", restore_command), ("cleanup", cleanup_command)] {
            if command.is_some_and(|command| command.contains(STATE_REF_PLACEHOLDER)) {
                return Err(self.invalid(format!(
                    "{section} command uses `{STATE_REF_PLACEHOLDER}`, but {hint}"
                )));
            }
        }
        Ok(())
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
            kind: "resource",
            name: self.name.clone(),
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
    /// One environment variable name claimed by two or more declarations.
    /// Claimants are pre-formatted and already sorted — see
    /// [`check_env_names`].
    EnvNameCollision {
        name: String,
        claimants: Vec<String>,
        /// Whether an `[exports]` or `captures` declaration claims this name,
        /// and so whether `{{exports.<name>}}` is a remedy worth suggesting.
        /// Ports are resource-scoped: two colliding port `env` vars have
        /// nothing to compose, only to rename.
        composable: bool,
    },
    /// A declaration claiming a name newgit itself sets for every command.
    ReservedEnvName {
        name: String,
        claimant: String,
    },
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
            Self::EnvNameCollision {
                name,
                claimants,
                composable,
            } => NewgitError::EnvNameCollision {
                remedy: if composable {
                    format!(
                        "rename all but one, and compose it elsewhere with `{{{{exports.{name}}}}}`"
                    )
                } else {
                    // A port's value reaches another resource only by being
                    // exported, so there is no composition to point at here.
                    "rename all but one".to_owned()
                },
                name,
                claimants: claimants.join(", "),
            },
            Self::ReservedEnvName { name, claimant } => {
                NewgitError::ReservedEnvName { name, claimant }
            }
        }
    }
}

impl fmt::Display for GraphProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.clone().into_error())
    }
}

/// Resource -> the resources its templates read exports from, each mapped to
/// the export names that caused the edge. The names are kept because an edge
/// nobody wrote down has to be able to explain itself.
pub type DataEdges = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// Which resource owns each name reachable as `{{exports.<name>}}`.
///
/// [`check_env_names`] guarantees one owner per name, so this is a function
/// rather than a guess. When the graph has a collision the map is arbitrary
/// between the claimants — but a collision is already a refusal, so no
/// command that acts on the order ever sees it.
fn export_owners(resources: &[ResourceDefinition]) -> BTreeMap<&str, &str> {
    let mut owners = BTreeMap::new();
    for resource in resources {
        for name in resource.exports.keys() {
            owners.insert(name.as_str(), resource.name.as_str());
        }
        for spec in resource.actions.values() {
            for name in &spec.captures {
                owners.insert(name.as_str(), resource.name.as_str());
            }
        }
    }
    owners
}

/// Edges inferred from `{{exports.<name>}}` — "I need your value," not "I
/// need your lifecycle."
///
/// `depends_on` used to be the only way to say either one. A `[[render]]`
/// substituting another resource's URL into a config file needs that
/// resource *bound* before it renders, and nothing more — but the only key
/// that produced that ordering also reversed into teardown, so a pure data
/// edge had to be declared as a lifecycle edge and silently acquired an
/// ordering claim it never asked for (#43).
///
/// The template is already the statement. `{{exports.EXPO_URL}}` names what
/// it needs, so the edge is read from it rather than restated in
/// `depends_on`, and it orders binding only.
///
/// Only bind-time consumers count: `[exports]` values and `[[render]]`
/// replacements, both rendered while the graph is being bound. `[cleanup]`
/// and `[checkpoint]` may use `{{exports.*}}` too, but they read a binding
/// record that is already complete — there is nothing left to order.
pub fn data_edges(resources: &[ResourceDefinition]) -> DataEdges {
    let owners = export_owners(resources);
    let mut edges: DataEdges = BTreeMap::new();

    for resource in resources {
        let templates = resource.exports.values().chain(
            resource
                .render
                .iter()
                .flat_map(|spec| spec.replace.iter().map(|replacement| &replacement.with)),
        );
        for template in templates {
            for name in crate::exports::export_placeholders(template) {
                let Some(owner) = owners.get(name) else {
                    // An unowned name is not an edge. It is a template that
                    // will not resolve, which `[exports]` and `[[render]]`
                    // already refuse at bind time with a better message than
                    // a graph error could give.
                    continue;
                };
                // A resource composing its own exports is the documented
                // sibling case, not an edge to itself.
                if *owner != resource.name {
                    edges
                        .entry(resource.name.clone())
                        .or_default()
                        .entry((*owner).to_owned())
                        .or_default()
                        .insert(name.to_owned());
                }
            }
        }
    }
    edges
}

/// The resolved resource graph: the two orders commands need, the edges that
/// were inferred rather than declared, and everything wrong with it.
///
/// Two orders because `depends_on` was being asked to mean two things at once
/// (#43). Ordering `prepare` at spawn and ordering teardown in reverse are
/// separate claims, and a data edge only ever makes the first one.
#[derive(Debug, Clone, Default)]
pub struct ResourceGraph {
    /// Dependencies before dependents, over `depends_on` *and* the inferred
    /// data edges. Binding, rendering, and environment assembly use this: a
    /// value has to exist before the template that reads it renders.
    pub bind_order: Vec<String>,
    /// Dependencies before dependents, over `depends_on` alone. Teardown and
    /// checkpoint walk it in reverse, so needing one string from a resource
    /// never claims anything about the order the two are torn down in.
    pub lifecycle_order: Vec<String>,
    /// Resource -> the resources whose exports its templates read. Reported
    /// by `newgit resource list`, since an edge nobody declared still has to
    /// be legible.
    pub data_edges: DataEdges,
    pub problems: Vec<GraphProblem>,
}

/// Resolve the whole graph. Never fails: an unresolvable dependency is
/// skipped and reported, so a half-built graph still loads. Commands that act
/// on the graph must check `problems` first; commands that build it may
/// proceed and warn.
pub fn resolve_graph(
    resources: &[ResourceDefinition],
    tracker_names: &BTreeSet<String>,
) -> ResourceGraph {
    let data_edges = data_edges(resources);
    let (bind_order, mut problems) = resolve_order(resources, tracker_names, &data_edges);

    // The lifecycle pass re-reports whatever the bind pass already found over
    // the `depends_on` subset, so its problems are dropped rather than
    // duplicated. A cycle that exists only through a data edge is real and is
    // reported by the bind pass alone.
    let (lifecycle_order, _) = resolve_order(resources, tracker_names, &BTreeMap::new());

    problems.extend(check_env_names(resources));
    ResourceGraph {
        bind_order,
        lifecycle_order,
        data_edges,
        problems,
    }
}

/// Order resources so dependencies come before dependents. Dependencies may
/// name trackers (which only need to exist) or other resources; `data` adds
/// edges no one declared. See [`resolve_graph`].
fn resolve_order(
    resources: &[ResourceDefinition],
    tracker_names: &BTreeSet<String>,
    data: &DataEdges,
) -> (Vec<String>, Vec<GraphProblem>) {
    /// What the walk reads. Split from what it writes so the recursion takes
    /// two arguments instead of eight.
    struct Edges<'a> {
        resources: &'a [ResourceDefinition],
        tracker_names: &'a BTreeSet<String>,
        data: &'a DataEdges,
    }

    /// What the walk writes.
    #[derive(Default)]
    struct Walk<'a> {
        state: BTreeMap<&'a str, Visit>,
        ordered: Vec<String>,
        problems: Vec<GraphProblem>,
        stack: Vec<String>,
    }

    #[derive(Clone, Copy)]
    enum Visit {
        InProgress,
        Done,
    }

    // `name` is not tied to `'a`: the visit state is keyed by names borrowed
    // from `resources`, but a name may also arrive from the inferred edges,
    // which are owned elsewhere.
    fn visit<'a>(name: &str, edges: &Edges<'a>, walk: &mut Walk<'a>) {
        match walk.state.get(name) {
            Some(Visit::Done) => return,
            Some(Visit::InProgress) => {
                // Report the back-edge and stop descending; the resource is
                // already on the stack and will still be ordered by its caller.
                let mut cycle = walk.stack.clone();
                cycle.push(name.to_owned());
                walk.problems.push(GraphProblem::Cycle(cycle));
                return;
            }
            None => {}
        }
        let Some(resource) = edges.resources.iter().find(|r| r.name == name) else {
            // Caller verified membership; only reachable for dependencies.
            return;
        };
        walk.state.insert(&resource.name, Visit::InProgress);
        walk.stack.push(name.to_owned());
        for dependency in &resource.depends_on {
            if edges.tracker_names.contains(dependency) {
                continue;
            }
            if !edges.resources.iter().any(|r| &r.name == dependency) {
                walk.problems.push(GraphProblem::MissingDependency {
                    resource: resource.name.clone(),
                    dependency: dependency.clone(),
                });
                continue;
            }
            visit(dependency, edges, walk);
        }
        // Inferred edges are visited after declared ones so a graph with no
        // data edges orders exactly as it did before, and every resource
        // they name exists by construction — they were derived from it.
        for dependency in edges
            .data
            .get(name)
            .into_iter()
            .flatten()
            .map(|(owner, _)| owner)
        {
            visit(dependency, edges, walk);
        }
        walk.stack.pop();
        walk.state.insert(&resource.name, Visit::Done);
        walk.ordered.push(resource.name.clone());
    }

    let edges = Edges {
        resources,
        tracker_names,
        data,
    };
    let mut walk = Walk::default();

    for resource in resources {
        visit(&resource.name, &edges, &mut walk);
        // Each root starts from an empty stack; a cycle is named by the path
        // that reached it, not by everything visited before it.
        walk.stack.clear();
    }
    (walk.ordered, walk.problems)
}

/// Names newgit sets on every command it runs, after every resource's. A
/// declaration claiming one of these is dead on arrival.
const RESERVED_ENV_NAMES: [&str; 2] = ["NEWGIT_BRANCH", "NEWGIT_WORKSPACE"];

/// Check that no environment variable name is claimed by two declarations.
///
/// The command environment is assembled by layering exports in dependency
/// order and port `env` vars over those, so a name claimed twice used to
/// resolve silently to whichever declaration happened to come last. That is
/// never something a definition wants, and it is unreadable when it bites:
/// the losing declaration is not wrong anywhere you can see, it is simply
/// absent from the process that needed it. The set of names is fully known
/// from the definitions, so the collision is reported when the graph loads
/// rather than discovered in a subprocess.
///
/// Three kinds of declaration claim a name: an `[exports]` key, an action's
/// `captures` entry, and a port's `env`. Exports and captures *within one
/// resource* may share a name — a capture is how an action refines its own
/// resource's export once the value exists, and the owner is unambiguous
/// either way. Everything else is a collision, including a port `env` that
/// shadows its own resource's export.
pub fn check_env_names(resources: &[ResourceDefinition]) -> Vec<GraphProblem> {
    // name -> claimants. One entry per (resource, name) for exports and
    // captures together; port env vars always claim separately.
    let mut claims: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    // Names at least one *export* claims, which decides the remedy: a value
    // only travels between resources as an export, so a collision among port
    // `env` vars alone has nothing to compose and is a rename either way.
    let mut exported_anywhere: BTreeSet<&str> = BTreeSet::new();

    for resource in resources {
        let mut exported: BTreeMap<&str, String> = BTreeMap::new();
        for name in resource.exports.keys() {
            exported.insert(name, format!("`{}` [exports]", resource.name));
        }
        for (action, spec) in &resource.actions {
            for name in &spec.captures {
                // An `[exports]` key already names this resource as the
                // owner; the capture is the same owner, so it adds nothing.
                exported
                    .entry(name)
                    .or_insert_with(|| format!("`{}` [actions.{action}] captures", resource.name));
            }
        }
        for (name, claimant) in exported {
            claims.entry(name).or_default().push(claimant);
            exported_anywhere.insert(name);
        }

        for (port, request) in &resource.ports {
            if let Some(name) = &request.env {
                claims
                    .entry(name)
                    .or_default()
                    .push(format!("`{}` [ports.{port}] env", resource.name));
            }
        }
    }

    let mut problems = Vec::new();
    for (name, mut claimants) in claims {
        claimants.sort();
        if RESERVED_ENV_NAMES.contains(&name) {
            // Reported per claimant: each one has to be renamed, and a
            // collision between two dead declarations is not the point.
            for claimant in claimants {
                problems.push(GraphProblem::ReservedEnvName {
                    name: name.to_owned(),
                    claimant,
                });
            }
            continue;
        }
        if claimants.len() > 1 {
            problems.push(GraphProblem::EnvNameCollision {
                name: name.to_owned(),
                claimants,
                composable: exported_anywhere.contains(name),
            });
        }
    }
    problems
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
        let graph = resolve_graph(&resources, &trackers);
        assert!(graph.problems.is_empty());
        assert_eq!(graph.bind_order, vec!["deps".to_owned(), "app".to_owned()]);
        // With no data edges the two orders are the same graph walked twice.
        assert_eq!(graph.lifecycle_order, graph.bind_order);

        let cyclic = vec![resource("a", &["b"]), resource("b", &["a"])];
        assert!(matches!(
            resolve_graph(&cyclic, &trackers).problems.first(),
            Some(GraphProblem::Cycle(_))
        ));

        let missing = vec![resource("app", &["nope"])];
        assert!(matches!(
            resolve_graph(&missing, &trackers).problems.first(),
            Some(GraphProblem::MissingDependency { .. })
        ));
    }

    fn exporting(name: &str, exports: &[&str]) -> ResourceDefinition {
        let mut definition = resource(name, &[]);
        definition.exports = exports
            .iter()
            .map(|export| ((*export).to_owned(), "value".to_owned()))
            .collect();
        definition
    }

    fn with_port_env(
        mut definition: ResourceDefinition,
        port: &str,
        env: &str,
    ) -> ResourceDefinition {
        definition.ports.insert(
            port.to_owned(),
            PortRequest {
                start: 3000,
                env: Some(env.to_owned()),
            },
        );
        definition
    }

    fn with_captures(
        mut definition: ResourceDefinition,
        action: &str,
        captures: &[&str],
    ) -> ResourceDefinition {
        definition.actions.insert(
            action.to_owned(),
            ActionSpec {
                command: Some("true".to_owned()),
                workdir: None,
                long_running: false,
                signal: None,
                captures: captures.iter().map(ToString::to_string).collect(),
            },
        );
        definition
    }

    /// The whole point of the check: before it, the loser was simply absent
    /// from the environment with nothing anywhere saying why.
    #[test]
    fn two_resources_may_not_export_the_same_name() {
        let problems = check_env_names(&[
            exporting("metro", &["EXPO_URL"]),
            exporting("supabase", &["EXPO_URL", "SUPABASE_URL"]),
        ]);
        assert_eq!(
            problems,
            vec![GraphProblem::EnvNameCollision {
                name: "EXPO_URL".to_owned(),
                claimants: vec![
                    "`metro` [exports]".to_owned(),
                    "`supabase` [exports]".to_owned(),
                ],
                composable: true,
            }],
            "SUPABASE_URL is claimed once and is not a problem"
        );
    }

    /// Port env vars are layered *over* exports unconditionally, so this
    /// shadowing does not even depend on dependency order to bite.
    #[test]
    fn a_port_env_var_may_not_shadow_an_export() {
        let problems = check_env_names(&[
            exporting("metro", &["PORT"]),
            with_port_env(resource("app", &[]), "web", "PORT"),
        ]);
        assert_eq!(
            problems,
            vec![GraphProblem::EnvNameCollision {
                name: "PORT".to_owned(),
                claimants: vec![
                    "`app` [ports.web] env".to_owned(),
                    "`metro` [exports]".to_owned(),
                ],
                composable: true,
            }]
        );

        // Including within one resource, where it is just as silent.
        let own = with_port_env(exporting("app", &["PORT"]), "web", "PORT");
        assert_eq!(check_env_names(&[own]).len(), 1);
    }

    /// Two `service`-shaped resources both claiming `PORT` is the common way
    /// to meet this. Suggesting `{{exports.PORT}}` there would be advice that
    /// does not work: ports are scoped to their own resource, so there is no
    /// export to compose until someone publishes one.
    #[test]
    fn colliding_port_env_vars_are_a_rename_not_a_composition() {
        let problems = check_env_names(&[
            with_port_env(resource("api", &[]), "app", "PORT"),
            with_port_env(resource("web", &[]), "app", "PORT"),
        ]);
        assert_eq!(problems.len(), 1);
        assert!(matches!(
            &problems[0],
            GraphProblem::EnvNameCollision {
                composable: false,
                ..
            }
        ));
        let message = problems[0].clone().into_error().to_string();
        assert!(
            message.ends_with("rename all but one"),
            "no unusable compose hint: {message}"
        );
    }

    /// A capture is how an action publishes a value that does not exist until
    /// it runs, so it names its own resource's export on purpose.
    #[test]
    fn a_capture_shares_a_name_with_its_own_resource_but_not_another() {
        let refines_own = with_captures(
            exporting("preview", &["PREVIEW_URL"]),
            "prepare",
            &["PREVIEW_URL"],
        );
        assert!(check_env_names(&[refines_own]).is_empty());

        let two_owners = vec![
            with_captures(resource("preview", &[]), "prepare", &["PREVIEW_URL"]),
            exporting("tunnel", &["PREVIEW_URL"]),
        ];
        assert_eq!(
            check_env_names(&two_owners),
            vec![GraphProblem::EnvNameCollision {
                name: "PREVIEW_URL".to_owned(),
                claimants: vec![
                    "`preview` [actions.prepare] captures".to_owned(),
                    "`tunnel` [exports]".to_owned(),
                ],
                composable: true,
            }]
        );
    }

    /// Reported even though only one declaration claims it: newgit's own
    /// layer wins regardless, so the declaration is dead either way.
    #[test]
    fn a_reserved_name_is_reported_against_its_single_claimant() {
        assert_eq!(
            check_env_names(&[exporting("app", &["NEWGIT_BRANCH"])]),
            vec![GraphProblem::ReservedEnvName {
                name: "NEWGIT_BRANCH".to_owned(),
                claimant: "`app` [exports]".to_owned(),
            }]
        );
    }

    fn rendering(name: &str, with: &str) -> ResourceDefinition {
        let mut definition = resource(name, &[]);
        definition.render = vec![RenderSpec {
            path: Utf8PathBuf::from("config.toml"),
            replace: vec![crate::render::Replacement {
                find: "placeholder".to_owned(),
                with: with.to_owned(),
                count: 1,
            }],
        }];
        definition
    }

    /// The case from #43: Supabase's config needs Metro's URL and nothing
    /// else. Before, the only way to get the value was `depends_on`, which
    /// also asserted a lifecycle relationship that does not exist.
    #[test]
    fn a_render_reading_an_export_orders_binding_without_declaring_a_dependency() {
        let trackers = BTreeSet::new();
        let resources = vec![
            rendering("supabase", "{{exports.EXPO_URL}}"),
            exporting("metro", &["EXPO_URL"]),
        ];
        let graph = resolve_graph(&resources, &trackers);
        assert!(graph.problems.is_empty());

        // Bind order respects the inferred edge, even though `supabase` comes
        // first in definition order and declares no dependency at all.
        assert_eq!(
            graph.bind_order,
            vec!["metro".to_owned(), "supabase".to_owned()]
        );
        assert_eq!(
            graph.data_edges["supabase"]["metro"],
            BTreeSet::from(["EXPO_URL".to_owned()]),
            "the edge names the export that caused it"
        );

        // Teardown is the whole point: neither resource is torn down on the
        // strength of the other, so the lifecycle order keeps them in the
        // order they were defined.
        assert_eq!(
            graph.lifecycle_order,
            vec!["supabase".to_owned(), "metro".to_owned()]
        );
        assert!(
            supabase_declares_nothing(&resources),
            "the fix is that no `depends_on` was needed"
        );
    }

    fn supabase_declares_nothing(resources: &[ResourceDefinition]) -> bool {
        resources
            .iter()
            .all(|resource| resource.depends_on.is_empty())
    }

    /// An `[exports]` value composing another resource's export is the same
    /// edge; a resource composing its *own* is the documented sibling case
    /// and must not become a self-edge that reads as a cycle.
    #[test]
    fn exports_compose_across_resources_but_a_self_reference_is_not_an_edge() {
        let trackers = BTreeSet::new();
        let mut api = resource("api", &[]);
        api.exports = BTreeMap::from([
            ("BASE_URL".to_owned(), "http://127.0.0.1".to_owned()),
            (
                "HEALTH_URL".to_owned(),
                "{{exports.BASE_URL}}/health".to_owned(),
            ),
            ("DB".to_owned(), "{{exports.PG_URL}}".to_owned()),
        ]);
        let resources = vec![api, exporting("db", &["PG_URL"])];

        let graph = resolve_graph(&resources, &trackers);
        assert!(graph.problems.is_empty());
        assert_eq!(
            graph.data_edges["api"],
            BTreeMap::from([("db".to_owned(), BTreeSet::from(["PG_URL".to_owned()]))]),
            "BASE_URL is api's own and is not an edge to itself"
        );
        assert_eq!(graph.bind_order, vec!["db".to_owned(), "api".to_owned()]);
    }

    /// Two resources each needing a value the other publishes cannot be bound
    /// in any order. A data edge is a weaker claim than `depends_on`, but it
    /// is still an ordering claim, so this is a real cycle.
    #[test]
    fn a_cycle_through_data_edges_alone_is_still_a_cycle() {
        let trackers = BTreeSet::new();
        let mut a = resource("a", &[]);
        a.exports = BTreeMap::from([("A".to_owned(), "{{exports.B}}".to_owned())]);
        let mut b = resource("b", &[]);
        b.exports = BTreeMap::from([("B".to_owned(), "{{exports.A}}".to_owned())]);

        let graph = resolve_graph(&[a, b], &trackers);
        assert!(matches!(
            graph.problems.first(),
            Some(GraphProblem::Cycle(_))
        ));
        // The lifecycle graph has no edges at all, so it still resolves —
        // which is why the cycle has to be reported from the bind pass.
        assert_eq!(graph.lifecycle_order, vec!["a".to_owned(), "b".to_owned()]);
    }

    /// A name no resource exports is a template that will not resolve, which
    /// `[exports]` and `[[render]]` already refuse at bind time with a better
    /// message. The graph stays quiet rather than inventing an edge.
    #[test]
    fn an_unowned_export_name_is_not_an_edge() {
        let graph = resolve_graph(&[rendering("app", "{{exports.NOPE}}")], &BTreeSet::new());
        assert!(graph.data_edges.is_empty());
        assert!(graph.problems.is_empty());
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

    #[test]
    fn invalid_definition_error_names_resource() {
        let error = write_and_load(
            r#"ownership = "workspace"
workdir = "../outside"
"#,
        )
        .expect_err("a resource workdir outside the workspace is invalid");

        assert_eq!(
            error.to_string(),
            "resource `db` is invalid: workdir `../outside` must be a workspace-relative path with no `..`"
        );
    }

    /// The case that matters: this loaded, and then handed the restore
    /// command `hash:0fa284b46875` — a content hash where it expected
    /// something to restore.
    #[test]
    fn a_state_ref_no_checkpoint_can_record_is_refused() {
        let hash_definition = |section: &str, command: &str| {
            format!(
                r#"ownership = "branch"

[identity]
paths = ["package-lock.json"]

[actions.prepare]
command = "npm ci"

[checkpoint]
mode = "hash"

[{section}]
{command}
"#
            )
        };

        let error = write_and_load(&hash_definition(
            "restore",
            "mode = \"command\"\ncommand = \"restore-from {{state_ref}}\"",
        ))
        .expect_err("a hash ref is not something a command can restore from");
        let message = error.to_string();
        assert!(message.contains("restore command"), "{message}");
        assert!(message.contains("recompute"), "names the fix: {message}");

        // The same mistake in a teardown command, which is the one that does
        // damage: `delete-environment hash:0fa284b468` is a wrong argument,
        // and the runtime guard only ever saw a placeholder that had resolved.
        let error = write_and_load(&hash_definition(
            "cleanup",
            "command = \"delete-environment {{state_ref}}\"",
        ))
        .expect_err("a hash ref is not something a command can tear down");
        assert!(error.to_string().contains("cleanup command"));

        // Nothing records a ref at all, so the placeholder can never resolve —
        // in either section.
        for section in [
            "[restore]\nmode = \"command\"\ncommand",
            "[cleanup]\ncommand",
        ] {
            let error = write_and_load(&format!(
                r#"ownership = "branch"

[actions.prepare]
command = "true"

{section} = "act-on {{{{state_ref}}}}"
"#
            ))
            .expect_err("no checkpoint records a ref to interpolate");
            assert!(error.to_string().contains("[checkpoint]"));
        }
    }

    /// `recompute` does not *ignore* an external handle, which would be inert
    /// and allowed — it re-runs `prepare`, which mints a second external
    /// instance and orphans the one the handle names.
    #[test]
    fn an_external_checkpoint_with_a_recompute_restore_is_refused() {
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
        .expect_err("recompute would mint a second preview");
        assert!(error.to_string().contains("external"));
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

        // The refusal is about the *placeholder*, not the mode. A restore
        // command that never asks for a state ref is an ordinary rebuild,
        // whatever the checkpoint records — refusing it would force a
        // working definition to be rewritten to satisfy the validator.
        ok(
            "mode = \"hash\"",
            "mode = \"command\"\ncommand = \"npm ci\"",
        );
        ok("mode = \"none\"", "mode = \"command\"\ncommand = \"true\"");
    }
}
