//! A live instance against the definitions it was bound from (#85).
//!
//! Agents write `.newgit/resources/*.toml` and iterate on it while an
//! instance runs. Binding is a moment, not a subscription: an export, a
//! render, or a `prepare` edited afterwards changes nothing in an instance
//! bound before, and the agent then debugs behavior that comes from the old
//! binding. So `status` compares what newgit would write now with what the
//! binding record says it wrote, and names every difference with the
//! commands that apply it.
//!
//! Compared rather than fingerprinted: ports, exports and rendered values
//! are in the record already, so each is re-resolved and held against it,
//! which names the very export or file and also catches a value that moved
//! because a dependency's did. Only `prepare` leaves nothing in the record,
//! so the binding keeps [`crate::resource::ResourceDefinition::prepare_rev`]
//! of the one it ran. A running process is held against the command its
//! log says it was started with.
//!
//! What newgit reads live is never flagged: `[ready]`, `depends_on`,
//! `[identity] paths`, ownership, and the checkpoint, restore and cleanup
//! hooks take effect the next time they are read. A port's `start` is only
//! where an allocation began: the port is the instance's for life.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use camino::{Utf8Path, Utf8PathBuf};

use super::{BranchManager, StopStartResource, StopStartStatus};
use crate::branch::{BranchInstance, ResourceBinding};
use crate::error::{NewgitError, Result};
use crate::exports::RenderContext;
use crate::render::{self, AppliedReplacement};
use crate::resource::ResourceDefinition;
use crate::supervisor::{StopOutcome, Supervisor};

/// What `newgit resource rebind` did for a resource whose definition is
/// gone: it took the binding out, and with it the ports it claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundResource {
    pub instance: String,
    pub ports: BTreeMap<String, u16>,
}

impl BranchManager {
    /// The one problem line for a bound, ready resource that differs from
    /// its definition, if it does: what changed, and the commands that
    /// apply it.
    pub(super) fn definition_drift(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        binding: &ResourceBinding,
        supervisor: &Supervisor,
    ) -> Option<String> {
        let name = &definition.name;
        let instance = &branch.name;
        let path = self.definition_path(name);
        let reasons = self.bind_drift(branch, definition, binding);
        if !reasons.is_empty() {
            // `rebind` refuses while the process runs (#288), so a running
            // one is stopped around it, and `start` brings back the rest of
            // what `stop` stopped.
            let apply = if supervisor.running_pid(name).is_some() {
                format!(
                    "`newgit stop {instance} && newgit resource rebind {name} {instance} && \
                     newgit start {instance}`"
                )
            } else {
                format!("`newgit resource rebind {name} {instance}`")
            };
            return Some(format!(
                "`{name}` was bound from an older {path}: {} — apply it with {apply}",
                reasons.join("; ")
            ));
        }
        let action = self.process_drift(branch, definition, supervisor)?;
        Some(format!(
            "`{name}.{action}` is running a command {path} no longer gives — restart it with \
             `newgit stop {instance} && newgit start {instance}`"
        ))
    }

    /// Where `name`'s definition is read from: the project root's
    /// `.newgit/resources/`, never a workspace's copy.
    fn definition_path(&self, name: &str) -> Utf8PathBuf {
        self.store.paths().resources.join(format!("{name}.toml"))
    }

    /// What binding again would change: each port, export, render and
    /// `prepare` that differs from what the record holds.
    fn bind_drift(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        binding: &ResourceBinding,
    ) -> Vec<String> {
        let mut reasons = Vec::new();

        let added: Vec<&String> = definition
            .ports
            .keys()
            .filter(|port| !binding.resolved_ports.contains_key(*port))
            .collect();
        if !added.is_empty() {
            reasons.push(format!(
                "{} declared since, so nothing claims {} for this instance",
                listed("port", &added),
                if added.len() == 1 { "it" } else { "them" }
            ));
        }
        let gone: Vec<&String> = binding
            .resolved_ports
            .keys()
            .filter(|port| !definition.ports.contains_key(*port))
            .collect();
        if !gone.is_empty() {
            reasons.push(format!(
                "{} no longer declared, still claimed here",
                listed("port", &gone)
            ));
        }

        reasons.extend(self.export_drift(branch, definition, binding));
        reasons.extend(self.render_drift_against_definition(branch, definition, binding));

        if binding
            .prepare_rev
            .as_ref()
            .is_some_and(|rev| *rev != definition.prepare_rev())
        {
            reasons.push("`prepare` changed since it ran here".to_owned());
        }
        reasons
    }

    /// Each export resolved as a rebind would resolve it — over every other
    /// binding's exports, on the ports it holds — against the record. A
    /// name an action captures is the capture's, not `[exports]`'s.
    fn export_drift(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        binding: &ResourceBinding,
    ) -> Vec<String> {
        let captured: BTreeSet<&String> = definition
            .actions
            .values()
            .flat_map(|action| action.captures.iter())
            .collect();
        let mut others = BTreeMap::new();
        for other in &self.graph.bind_order {
            if other == &definition.name {
                continue;
            }
            if let Some(bound) = branch.resources.get(other) {
                others.extend(bound.resolved_exports.clone());
            }
        }
        let (now, unresolved) =
            self.resolve_exports(branch, definition, &binding.resolved_ports, &others);
        let unresolved: BTreeSet<&String> = unresolved.iter().map(|(key, _)| key).collect();

        let (mut added, mut changed) = (Vec::new(), Vec::new());
        for key in definition.exports.keys() {
            if captured.contains(key) {
                continue;
            }
            match binding.resolved_exports.get(key) {
                None => added.push(key),
                Some(was) if unresolved.contains(key) || now.get(key) != Some(was) => {
                    changed.push(key);
                }
                Some(_) => {}
            }
        }
        let dropped: Vec<&String> = binding
            .resolved_exports
            .keys()
            .filter(|key| !definition.exports.contains_key(*key) && !captured.contains(key))
            .collect();

        let mut reasons = Vec::new();
        if !added.is_empty() {
            reasons.push(format!("{} declared since", listed("export", &added)));
        }
        if !changed.is_empty() {
            reasons.push(format!("{} changed", listed("export", &changed)));
        }
        if !dropped.is_empty() {
            reasons.push(format!(
                "{} no longer declared, still in every command's environment",
                listed("export", &dropped)
            ));
        }
        reasons
    }

    /// Each `[[render]]` resolved as a rebind would render it, against what
    /// the record says was written: per file, the same claims in the same
    /// order. A render into the resource's own built tree is the install
    /// store's, and left out on both sides.
    fn render_drift_against_definition(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        binding: &ResourceBinding,
    ) -> Vec<String> {
        let exports = self.bound_exports(branch);
        let bound = self.bound_vars(branch);
        let context = RenderContext {
            branch_name: &branch.name,
            branch_slug: &branch.slug,
            workspace: branch.workspace_path.as_str(),
            scripts: self.scripts_dir(),
            ports: Some(&binding.resolved_ports),
            exports: Some(&exports),
            bound: Some(&bound),
            ..RenderContext::default()
        };
        type Claims = Vec<Option<Vec<AppliedReplacement>>>;
        let mut expected: BTreeMap<&Utf8Path, Claims> = BTreeMap::new();
        for spec in &definition.render {
            if definition.renders_into_produced(spec) {
                continue;
            }
            expected
                .entry(&spec.path)
                .or_default()
                .push(render::resolve(&definition.name, spec, &context).ok());
        }
        let produced = definition.identity_produces();
        let mut recorded: BTreeMap<&Utf8Path, Claims> = BTreeMap::new();
        for record in &binding.rendered {
            if produced
                .iter()
                .any(|tree| record.path.starts_with(tree.as_path()))
            {
                continue;
            }
            recorded
                .entry(&record.path)
                .or_default()
                .push(Some(record.applied.clone()));
        }

        let paths: BTreeSet<&Utf8Path> = expected.keys().chain(recorded.keys()).copied().collect();
        let (mut added, mut changed, mut dropped) = (Vec::new(), Vec::new(), Vec::new());
        for path in paths {
            match (expected.get(path), recorded.get(path)) {
                (Some(_), None) => added.push(path),
                (None, Some(_)) => dropped.push(path),
                (Some(now), Some(was)) if now != was => changed.push(path),
                _ => {}
            }
        }
        let mut reasons = Vec::new();
        if !added.is_empty() {
            reasons.push(format!("`[[render]]` of {} added", files(&added)));
        }
        if !changed.is_empty() {
            reasons.push(format!("`[[render]]` of {} changed", files(&changed)));
        }
        if !dropped.is_empty() {
            reasons.push(format!(
                "`[[render]]` of {} removed, its values still in the file",
                files(&dropped)
            ));
        }
        reasons
    }

    /// The action whose running process was started with a command its
    /// definition no longer gives, if one was. A process whose action has
    /// since been removed, or is no longer long-running, counts.
    fn process_drift(
        &self,
        branch: &BranchInstance,
        definition: &ResourceDefinition,
        supervisor: &Supervisor,
    ) -> Option<String> {
        let name = &definition.name;
        supervisor.running_pid(name)?;
        let log = supervisor.last_log(name)?;
        let action_name = logged_action(&log, name)?;
        let Some(action) = definition
            .actions
            .get(&action_name)
            .filter(|action| action.long_running)
        else {
            return Some(action_name);
        };
        let (command, _) = self
            .action_invocation(branch, definition, action, &action_name)
            .ok()?;
        (!started_with(&log, &command)).then_some(action_name)
    }

    /// Problem lines for what an instance binds or parks that no definition
    /// names any more: a definition file deleted by hand rather than with
    /// `newgit resource remove`, whose process `stop` and `remove` still
    /// stop and whose ports the record still claims.
    pub(super) fn undefined_binding_problems(
        &self,
        branch: &BranchInstance,
        supervisor: &Supervisor,
    ) -> Vec<String> {
        let instance = &branch.name;
        self.undefined_bindings(branch)
            .into_iter()
            .map(|(name, binding, parked)| {
                let path = self.definition_path(name);
                let held = if parked {
                    "parked here (its tracker is dropped)"
                } else {
                    "bound here"
                };
                let release = format!("`newgit resource rebind {name} {instance}`");
                if let Some(pid) = supervisor.running_pid(name) {
                    return format!(
                        "`{name}` is {held}, but {path} is gone and its process (pid {pid}) is \
                         still running — stop it with `newgit stop {instance}`, then release \
                         its binding with {release}"
                    );
                }
                let ports: Vec<&String> = binding.resolved_ports.keys().collect();
                let claims = if ports.is_empty() {
                    String::new()
                } else {
                    format!(" and it still claims {}", listed("port", &ports))
                };
                format!("`{name}` is {held}, but {path} is gone{claims} — release its binding with {release}")
            })
            .collect()
    }

    /// Problem lines for each resource defined and active in `branch` that
    /// it does not bind: one added (or brought by a bound tracker) after the
    /// instance bound its resources. Not one that binding would do nothing
    /// for: a definition holding only what newgit reads live, such as
    /// `disposable` (what an eviction may drop), is in effect already.
    pub(super) fn unbound_definition_problems(&self, branch: &BranchInstance) -> Vec<String> {
        self.resources
            .iter()
            .filter(|definition| {
                !(definition.ports.is_empty()
                    && definition.exports.is_empty()
                    && definition.render.is_empty()
                    && definition.actions.is_empty()
                    && definition.identity.is_none()
                    && definition.checkpoint.is_none()
                    && definition.restore.is_none()
                    && definition.cleanup.is_none()
                    && definition.ready.is_none())
            })
            .map(|definition| &definition.name)
            .filter(|name| {
                !branch.resources.contains_key(*name)
                    && !branch.parked.contains_key(*name)
                    && self.resource_active(branch, name)
            })
            .map(|name| {
                format!(
                    "`{name}` is defined in {} but not bound here — bind it with `newgit \
                     resource rebind {name} {}`",
                    self.definition_path(name),
                    branch.name
                )
            })
            .collect()
    }

    /// Every binding, bound or parked, whose definition is gone: the name,
    /// the binding, and whether it is parked.
    fn undefined_bindings<'b>(
        &self,
        branch: &'b BranchInstance,
    ) -> Vec<(&'b String, &'b ResourceBinding, bool)> {
        let bound = branch
            .resources
            .iter()
            .map(|(name, binding)| (name, binding, false));
        let parked = branch
            .parked
            .iter()
            .map(|(name, parked)| (name, &parked.binding, true));
        bound
            .chain(parked)
            .filter(|(name, _, _)| self.resource_definition(name).is_err())
            .collect()
    }

    /// Stop the process of each binding whose definition is gone, as `stop`
    /// and `remove` stop a defined one. With no definition there is no
    /// signal or action to read, so it gets the default `TERM`, and nothing
    /// is recorded for `start` to bring back.
    pub(super) fn stop_undefined(&self, branch: &BranchInstance) -> Vec<StopStartResource> {
        let supervisor = self.supervisor(branch);
        let mut stopped = Vec::new();
        for (name, _, _) in self.undefined_bindings(branch) {
            if supervisor.running_pid(name).is_none() {
                continue;
            }
            let (status, error) = match supervisor.stop(name, "term") {
                Ok(StopOutcome::Stopped(_)) => (StopStartStatus::Stopped, None),
                Ok(StopOutcome::NotRunning) => continue,
                Ok(StopOutcome::StillRunning(pid)) => (
                    StopStartStatus::Failed,
                    Some(format!(
                        "pid {pid} ignored `term`; escalate with `kill -9 -- -{pid}` if needed"
                    )),
                ),
                Err(error) => (StopStartStatus::Failed, Some(error.to_string())),
            };
            let action = supervisor
                .last_log(name)
                .and_then(|log| logged_action(&log, name))
                .unwrap_or_else(|| "process".to_owned());
            stopped.push(StopStartResource {
                name: name.clone(),
                action,
                status,
                error,
            });
        }
        stopped
    }

    /// `newgit resource rebind` for a resource whose definition is gone:
    /// binding again by what is defined is binding nothing, so the binding
    /// comes out, its values come out of the files it rendered, and its
    /// ports are released. Its `[cleanup]` went with its definition, so
    /// whatever it made outside the workspace is left. Refused while its
    /// process runs, as a rebind is.
    pub fn unbind_undefined(&self, instance: &str, resource: &str) -> Result<UnboundResource> {
        let (_lock, mut branch) = self.locked_branch(instance)?;
        if self.resource_definition(resource).is_ok() {
            return Err(NewgitError::Unsupported(format!(
                "resource `{resource}` is defined; `newgit resource rebind` binds it again"
            )));
        }
        if let Some(pid) = self.supervisor(&branch).running_pid(resource) {
            return Err(NewgitError::Unsupported(format!(
                "resource `{resource}`'s process is running (pid {pid}); stop it first with \
                 `newgit stop {}`",
                branch.name
            )));
        }
        let ports = if let Some(binding) = branch.resources.get(resource) {
            let ports = binding.resolved_ports.clone();
            self.unbind_resource(&mut branch, resource)?;
            ports
        } else if let Some(parked) = branch.parked.remove(resource) {
            // Parking took its values out of its files already.
            parked.binding.resolved_ports
        } else {
            return Err(NewgitError::Unsupported(format!(
                "no resource named `{resource}` is defined in {}, and `{}` binds none",
                self.store.paths().resources,
                branch.name
            )));
        };
        branch.updated_at = chrono::Utc::now();
        self.store.save_branch_record(&branch)?;
        Ok(UnboundResource {
            instance: branch.name,
            ports,
        })
    }
}

/// The action a supervised run's log is for, from its name:
/// `<resource>.<action>-<YYYYmmddTHHMMSS>-<nanos>Z.log`, as
/// `MetadataStore::action_log_path` names it.
fn logged_action(log: &Utf8Path, resource: &str) -> Option<String> {
    let stem = log.file_name()?.strip_suffix(".log")?;
    let label = stem.rsplitn(3, '-').nth(2)?;
    label
        .strip_prefix(resource)?
        .strip_prefix('.')
        .map(ToOwned::to_owned)
}

/// Whether the log of a supervised run opens with `command`, as the
/// supervisor writes it: `[newgit] $ ` before its first line and
/// `[newgit] > ` before each further one (#89). Only that much is read,
/// since a server's log can be long.
fn started_with(log: &Utf8Path, command: &str) -> bool {
    let mut opening = String::new();
    for (index, line) in command.trim_end().lines().enumerate() {
        opening.push_str(if index == 0 {
            "[newgit] $ "
        } else {
            "[newgit] > "
        });
        opening.push_str(line);
        opening.push('\n');
    }
    let Ok(file) = std::fs::File::open(log) else {
        // Nothing to compare with is not a difference.
        return true;
    };
    let mut head = Vec::with_capacity(opening.len());
    if file
        .take(opening.len() as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return true;
    }
    head == opening.as_bytes()
}

/// "port `a`" or "ports `a`, `b`".
fn listed(noun: &str, names: &[&String]) -> String {
    format!(
        "{noun}{} {}",
        if names.len() == 1 { "" } else { "s" },
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn files(paths: &[&Utf8Path]) -> String {
    paths
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_action_comes_from_the_log_name() {
        let log = Utf8Path::new("/x/logs/a/web.start-20261118T194320-173047000Z.log");
        assert_eq!(logged_action(log, "web").as_deref(), Some("start"));
        let log = Utf8Path::new("/x/logs/a/web.dev-server-20261118T194320-173047000Z.log");
        assert_eq!(logged_action(log, "web").as_deref(), Some("dev-server"));
        assert_eq!(logged_action(log, "we"), None);
    }
}
