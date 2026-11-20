//! Names in a definition that must refer to something that exists, and
//! template placeholders nobody filled in, found where the definitions are
//! read rather than where the hook that needs them first runs.
//!
//! A `[checkpoint] into_tracker` naming a tracker nobody created loads,
//! binds, spawns and reports ready, and fails at the first `checkpoint`. A
//! `{{ports.ap}}` in a command runs with the literal text in it. An
//! `EDIT ME` left in `[identity] paths` hashes a file that is not there.
//! Each is a mistake in the definition that is certain to surface later,
//! in a hook nobody is watching, so this reads every definition once and
//! names each one with what fixes it. `newgit resource check` fails on
//! them, and `newgit spawn` and `newgit status` name them for every
//! instance that binds the resource (or the tracker) they are in.
//!
//! What was already checked elsewhere stays there: a `depends_on` naming
//! nothing is a graph problem ([`crate::resource::resolve_graph`]), and a
//! `[restore] action` naming no action, `autostart` on more than one
//! action, or a `hash` checkpoint without `[identity]` refuse the
//! definition when it loads ([`crate::resource::ResourceDefinition`]).

use std::collections::BTreeSet;
use std::fmt;

use camino::Utf8Path;

use crate::resource::ResourceDefinition;
use crate::tracker::TrackerDefinition;

/// How every bundled template marks a value that is yours to fill in:
/// `EDIT ME: your lockfile, …` in a sentence, `EDIT_ME_PROJECT` where the
/// value has to stay one token.
const PLACEHOLDER_MARKERS: [&str; 2] = ["EDIT ME", "EDIT_ME"];

/// Whether `text` holds a template placeholder nobody replaced.
pub fn is_placeholder(text: &str) -> bool {
    PLACEHOLDER_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
}

/// The lines of a definition file that hold a placeholder, as (1-based
/// line number, line). Comments are skipped: a template explains its
/// placeholders in them, and only a value is something to edit.
pub fn placeholder_lines(contents: &str) -> Vec<(usize, &str)> {
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && is_placeholder(line))
        .map(|(index, line)| (index + 1, line))
        .collect()
}

/// Whose definition a problem is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Resource,
    Tracker,
}

/// One thing wrong in one definition: where, and what fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionProblem {
    pub owner: Owner,
    /// The resource's or tracker's name.
    pub name: String,
    /// Where in the file: `[checkpoint] into_tracker`, `[actions.prepare]
    /// command`, `[[render]] replace.find`.
    pub field: String,
    /// What is wrong, and the fix.
    pub reason: String,
}

impl fmt::Display for DefinitionProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.owner {
            Owner::Resource => "resource",
            Owner::Tracker => "tracker",
        };
        write!(
            f,
            "{kind} `{}`: {} — {}",
            self.name, self.field, self.reason
        )
    }
}

/// Everything wrong with `resources` and `trackers` that this module checks.
/// `metadata_root` is the project root's `.newgit/`, where the definitions
/// and `scripts/` are read from.
pub fn check(
    resources: &[ResourceDefinition],
    trackers: &[TrackerDefinition],
    metadata_root: &Utf8Path,
) -> Vec<DefinitionProblem> {
    let mut problems = Vec::new();
    let tracker_names: BTreeSet<&str> = trackers.iter().map(|t| t.name.as_str()).collect();
    let resource_names: BTreeSet<&str> = resources.iter().map(|r| r.name.as_str()).collect();
    // Every name some resource exports, by `[exports]` or a capture.
    let exported: BTreeSet<&str> = resources
        .iter()
        .flat_map(|resource| {
            resource.exports.keys().map(String::as_str).chain(
                resource
                    .actions
                    .values()
                    .flat_map(|action| action.captures.iter().map(String::as_str)),
            )
        })
        .collect();
    let scripts = metadata_root.join("scripts");

    for resource in resources {
        let file = metadata_root
            .join("resources")
            .join(format!("{}.toml", resource.name));
        let mut push = |field: String, reason: String| {
            problems.push(DefinitionProblem {
                owner: Owner::Resource,
                name: resource.name.clone(),
                field,
                reason,
            });
        };

        // Placeholders first, read from the file itself so every field is
        // covered, including ones a later check would also trip over.
        let mut placeholder_fields = BTreeSet::new();
        if let Ok(contents) = std::fs::read_to_string(&file)
            && let Ok(table) = toml::from_str::<toml::Table>(&contents)
        {
            let mut found = Vec::new();
            placeholder_values(&toml::Value::Table(table), &mut Vec::new(), &mut found);
            for (field, value) in found {
                placeholder_fields.insert(field.clone());
                push(
                    field,
                    format!(
                        "still the template's placeholder `{value}`: replace it with this \
                         project's value in .newgit/resources/{}.toml",
                        resource.name
                    ),
                );
            }
        }

        if let Some(tracker) = resource
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.into_tracker.as_deref())
            && !tracker_names.contains(tracker)
        {
            push(
                "[checkpoint] into_tracker".to_owned(),
                format!(
                    "names `{tracker}`, which is not a tracker: create it with `newgit tracker \
                     create {tracker}`"
                ),
            );
        }

        let own_exports: BTreeSet<&str> = resource
            .exports
            .keys()
            .map(String::as_str)
            .chain(
                resource
                    .actions
                    .values()
                    .flat_map(|action| action.captures.iter().map(String::as_str)),
            )
            .collect();
        let context = Context {
            resource,
            own_exports: &own_exports,
            exported: &exported,
            scripts: &scripts,
        };
        for (field, scope, template) in templates(resource) {
            // A placeholder is the one thing wrong with that value until it
            // is replaced; what it would otherwise name is not the point.
            if placeholder_fields.contains(&field) {
                continue;
            }
            for reason in context.check_template(scope, template) {
                push(field.clone(), reason);
            }
        }
    }

    for tracker in trackers {
        for resource in &tracker.resources {
            if !resource_names.contains(resource.as_str()) {
                problems.push(DefinitionProblem {
                    owner: Owner::Tracker,
                    name: tracker.name.clone(),
                    field: "resources".to_owned(),
                    reason: format!(
                        "names `{resource}`, which is not a resource: add it with `newgit \
                         resource add {resource} --template <template>`, or take it out of \
                         .newgit/trackers/{}.toml",
                        tracker.name
                    ),
                });
            }
        }
    }
    problems
}

/// Which template variables have a value where, per the scope table in the
/// reference's *Template variables*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `[exports]` values and `[[render]]` `with`: every export bound so
    /// far, whichever resource owns it.
    Bind,
    /// An action, `key_command`, or `[ready]`: no `{{exports.*}}`.
    Action,
    /// `[checkpoint] command`: this resource's exports and the snapshot dir.
    Checkpoint,
    /// `[checkpoint] state_ref`: this resource's exports.
    StateRef,
    /// `[restore]` and `[cleanup]` commands: this resource's exports and
    /// the state ref.
    Restore,
}

/// Every templated string a resource holds, with where it is and its scope.
fn templates(resource: &ResourceDefinition) -> Vec<(String, Scope, &str)> {
    let mut out = Vec::new();
    for (key, value) in &resource.exports {
        out.push((format!("[exports] {key}"), Scope::Bind, value.as_str()));
    }
    for spec in &resource.render {
        for replacement in &spec.replace {
            out.push((
                "[[render]] replace.with".to_owned(),
                Scope::Bind,
                replacement.with.as_str(),
            ));
        }
    }
    for (name, action) in &resource.actions {
        if let Some(command) = &action.command {
            out.push((
                format!("[actions.{name}] command"),
                Scope::Action,
                command.as_str(),
            ));
        }
    }
    if let Some(command) = resource
        .identity
        .as_ref()
        .and_then(|identity| identity.key_command.as_deref())
    {
        out.push(("[identity] key_command".to_owned(), Scope::Action, command));
    }
    if let Some(ready) = &resource.ready {
        if let Some(http) = &ready.http {
            out.push(("[ready] http".to_owned(), Scope::Action, http.as_str()));
        }
        if let Some(command) = &ready.command {
            out.push((
                "[ready] command".to_owned(),
                Scope::Action,
                command.as_str(),
            ));
        }
    }
    if let Some(checkpoint) = &resource.checkpoint {
        if let Some(command) = &checkpoint.command {
            out.push((
                "[checkpoint] command".to_owned(),
                Scope::Checkpoint,
                command.as_str(),
            ));
        }
        if let Some(state_ref) = &checkpoint.state_ref {
            out.push((
                "[checkpoint] state_ref".to_owned(),
                Scope::StateRef,
                state_ref.as_str(),
            ));
        }
    }
    if let Some(command) = resource
        .restore
        .as_ref()
        .and_then(|restore| restore.command.as_deref())
    {
        out.push(("[restore] command".to_owned(), Scope::Restore, command));
    }
    if let Some(command) = resource
        .cleanup
        .as_ref()
        .and_then(|cleanup| cleanup.command.as_deref())
    {
        out.push(("[cleanup] command".to_owned(), Scope::Restore, command));
    }
    out
}

struct Context<'a> {
    resource: &'a ResourceDefinition,
    own_exports: &'a BTreeSet<&'a str>,
    exported: &'a BTreeSet<&'a str>,
    scripts: &'a Utf8Path,
}

impl Context<'_> {
    /// What is wrong with each newgit variable `template` names. Anything
    /// in `{{ }}` that is not newgit's (`docker inspect --format
    /// '{{.State.Status}}'`) is the command's own business and left alone.
    fn check_template(&self, scope: Scope, template: &str) -> Vec<String> {
        let mut problems = Vec::new();
        for (variable, after) in variables(template) {
            let shown = format!("{{{{{variable}}}}}");
            if let Some(port) = variable.strip_prefix("ports.") {
                if !self.resource.ports.contains_key(port) {
                    let declared = self
                        .resource
                        .ports
                        .keys()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>();
                    let declared = if declared.is_empty() {
                        "none".to_owned()
                    } else {
                        declared.join(", ")
                    };
                    problems.push(format!(
                        "names `{shown}`, but this resource declares no port `{port}` (its \
                         ports: {declared}); add `[ports.{port}]` or fix the name"
                    ));
                }
            } else if let Some(name) = variable.strip_prefix("exports.") {
                problems.extend(self.check_export(scope, &shown, name));
            } else if let Some(member) = variable.strip_prefix("branch.") {
                if !matches!(member, "name" | "slug") {
                    problems.push(unknown(&shown, "`{{branch.name}}` or `{{branch.slug}}`"));
                }
            } else if let Some(member) = variable.strip_prefix("bound.") {
                if !matches!(member, "trackers" | "paths") {
                    problems.push(unknown(&shown, "`{{bound.trackers}}` or `{{bound.paths}}`"));
                }
            } else if let Some(member) = variable.strip_prefix("snapshot.") {
                if member != "path" {
                    problems.push(unknown(&shown, "`{{snapshot.path}}`"));
                } else if scope != Scope::Checkpoint {
                    problems.push(format!(
                        "names `{shown}`, which has a value only in `[checkpoint] command`; here \
                         it stays the literal text"
                    ));
                } else if self
                    .resource
                    .checkpoint
                    .as_ref()
                    .is_none_or(|checkpoint| checkpoint.into_tracker.is_none())
                {
                    // The directory exists to be deposited: with no tracker
                    // to deposit into, newgit makes none.
                    problems.push(format!(
                        "names `{shown}`, which has a value only when `[checkpoint] \
                         into_tracker` names the tracker its contents go into; without one it \
                         stays the literal text"
                    ));
                }
            } else if variable == "state_ref" {
                if scope != Scope::Restore {
                    problems.push(format!(
                        "names `{shown}`, which has a value only in `[restore] command` and \
                         `[cleanup] command`; here it stays the literal text"
                    ));
                }
            } else if variable == "scripts"
                && let Some(script) = script_name(after)
                && !self.scripts.join(script).exists()
            {
                problems.push(format!(
                    "names `{{{{scripts}}}}/{script}`, and there is no .newgit/scripts/{script} \
                     in the project root: add it there (a workspace's copy is not read)"
                ));
            }
        }
        problems
    }

    fn check_export(&self, scope: Scope, shown: &str, name: &str) -> Option<String> {
        match scope {
            Scope::Bind if !self.exported.contains(name) => Some(format!(
                "names `{shown}`, which no resource exports: declare `{name}` in some \
                 resource's `[exports]` or `captures`, or fix the name"
            )),
            Scope::Bind => None,
            Scope::Action => Some(format!(
                "names `{shown}`, but a command like this one does not get `{{{{exports.*}}}}`: \
                 every export reaches it as an environment variable, so use `${name}`"
            )),
            Scope::Checkpoint | Scope::StateRef | Scope::Restore
                if !self.own_exports.contains(name) =>
            {
                Some(if self.exported.contains(name) {
                    format!(
                        "names `{shown}`, but a hook sees only its own resource's exports as \
                         `{{{{exports.*}}}}`; another resource's reaches it as the environment \
                         variable `${name}`"
                    )
                } else {
                    format!(
                        "names `{shown}`, which this resource does not export: declare `{name}` \
                         in its `[exports]` or `captures`, or fix the name"
                    )
                })
            }
            _ => None,
        }
    }
}

fn unknown(shown: &str, known: &str) -> String {
    format!(
        "names `{shown}`, which is not a template variable (did you mean {known}? `newgit \
         reference template-variables` lists them); it would stay the literal text"
    )
}

/// The newgit variables in `template`, each with the text after it:
/// `{{ports.app}}`, `{{exports.X}}`, `{{branch.slug}}`, `{{workspace}}`,
/// `{{scripts}}`, `{{bound.*}}`, `{{snapshot.path}}`, `{{state_ref}}`,
/// or a misspelling of a namespaced one.
fn variables(template: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    for (start, _) in template.match_indices("{{") {
        let rest = &template[start + 2..];
        let Some(end) = rest.find("}}") else {
            continue;
        };
        let inner = &rest[..end];
        if inner.contains("{{") {
            continue;
        }
        let ours = ["ports.", "exports.", "branch.", "bound.", "snapshot."]
            .iter()
            .any(|prefix| inner.starts_with(prefix) && inner.len() > prefix.len())
            || matches!(inner, "workspace" | "scripts" | "state_ref");
        if ours {
            found.push((inner, &rest[end + 2..]));
        }
    }
    found
}

/// The script a `{{scripts}}` is followed by: `/seed.sh` in
/// `{{scripts}}/seed.sh --fast`. `None` for a bare `{{scripts}}`, or one
/// whose path goes on through another variable or a shell expansion, which
/// only the running command can resolve.
fn script_name(after: &str) -> Option<&str> {
    let path = after.strip_prefix('/')?;
    let end = path
        .find(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | ';' | '&' | '|' | ')' | '<' | '>' | '`')
        })
        .unwrap_or(path.len());
    let path = &path[..end];
    if path.is_empty() || path.contains("{{") || path.contains('$') || path.contains('*') {
        return None;
    }
    Some(path)
}

/// Every string value under `value` that holds a placeholder, with its
/// field as a reader finds it in the file: `[identity] paths`, `[exports]
/// COMPOSE_PROJECT_NAME`, `[[render]] replace.find`.
fn placeholder_values(
    value: &toml::Value,
    path: &mut Vec<String>,
    found: &mut Vec<(String, String)>,
) {
    match value {
        toml::Value::String(text) if is_placeholder(text) => {
            let field = field_name(path);
            if !found.iter().any(|(existing, _)| existing == &field) {
                found.push((field, text.clone()));
            }
        }
        toml::Value::Table(table) => {
            for (key, value) in table {
                path.push(key.clone());
                placeholder_values(value, path, found);
                path.pop();
            }
        }
        toml::Value::Array(items) => {
            // Marked so the field reads `[[render]]`, not `[render]`.
            path.push("[]".to_owned());
            for item in items {
                placeholder_values(item, path, found);
            }
            path.pop();
        }
        _ => {}
    }
}

/// `["identity", "paths", "[]"]` → `[identity] paths`; `["render", "[]",
/// "replace", "[]", "find"]` → `[[render]] replace.find`; `["depends_on",
/// "[]"]` → `depends_on`.
fn field_name(path: &[String]) -> String {
    let keys: Vec<&str> = path
        .iter()
        .map(String::as_str)
        .filter(|key| *key != "[]")
        .collect();
    match keys.as_slice() {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [table, rest @ ..] => {
            let array_of_tables = path.get(1).is_some_and(|key| key == "[]");
            if array_of_tables {
                format!("[[{table}]] {}", rest.join("."))
            } else if rest.len() > 1 && path.get(2).is_none_or(|key| key != "[]") {
                // `[actions.prepare] command`: a table of tables.
                format!(
                    "[{table}.{}] {}",
                    rest[..rest.len() - 1].join("."),
                    rest[rest.len() - 1]
                )
            } else {
                format!("[{table}] {}", rest.join("."))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{field_name, script_name, variables};

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn fields_read_the_way_the_file_spells_them() {
        assert_eq!(
            field_name(&path(&["identity", "paths", "[]"])),
            "[identity] paths"
        );
        assert_eq!(
            field_name(&path(&["actions", "prepare", "command"])),
            "[actions.prepare] command"
        );
        assert_eq!(
            field_name(&path(&["exports", "COMPOSE_PROJECT_NAME"])),
            "[exports] COMPOSE_PROJECT_NAME"
        );
        assert_eq!(
            field_name(&path(&["render", "[]", "replace", "[]", "find"])),
            "[[render]] replace.find"
        );
        assert_eq!(field_name(&path(&["depends_on", "[]"])), "depends_on");
    }

    #[test]
    fn only_newgit_variables_are_read() {
        let found: Vec<&str> = variables(
            "docker inspect --format '{{.State.Status}}' {{exports.A}} {{ports.app}} {{ json . }} {{workspace}}",
        )
        .into_iter()
        .map(|(name, _)| name)
        .collect();
        assert_eq!(found, vec!["exports.A", "ports.app", "workspace"]);
    }

    #[test]
    fn a_script_is_the_path_up_to_the_end_of_the_word() {
        assert_eq!(script_name("/seed.sh --fast"), Some("seed.sh"));
        assert_eq!(script_name("/db/up.sh\""), Some("db/up.sh"));
        assert_eq!(script_name("/$NAME.sh"), None);
        assert_eq!(script_name(" seed.sh"), None);
        assert_eq!(script_name("/{{branch.slug}}.sh"), None);
    }
}
