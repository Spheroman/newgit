//! Processes left running on a rendered file a binding change rewrote (#249).
//!
//! `tracker use` binds resources, and binding renders: a `db` brought by the
//! tracker writes its port into `apps/admin/.env.local`. A dev server that
//! read that file when it started does not read it again, so after the use it
//! is running on the old port, and nothing said so.
//!
//! newgit cannot know which process reads which file unless the definition
//! says (`reads`). So: the bytes of every rendered path are read before the
//! change and compared after it. When none changed, nothing is said — a
//! re-render that wrote what was already there leaves nothing stale. When one
//! did, each process that was already running before the change and is still
//! running after it is either restarted (it declares `reads` covering a
//! changed file, the same restart `undo` gives what it restores), left alone
//! (it declares `reads` and none of it changed), or named as possibly stale,
//! with the command that restarts it (it declares nothing).

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};

use super::{BranchManager, StopStartResource, StopStartStatus, long_running_action};
use crate::branch::BranchInstance;
use crate::error::Result;
use crate::supervisor::StopOutcome;

/// What a change found before it touched anything: the bytes of every path a
/// render may write, and which resources had a process running.
pub(super) struct RenderBefore {
    bytes: BTreeMap<Utf8PathBuf, Option<Vec<u8>>>,
    running: BTreeSet<String>,
}

/// Rendered files a `tracker use` or `drop` rewrote, and what that meant for
/// the processes already running on them. Empty when no rendered file's bytes
/// changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaleRender {
    /// Rendered paths whose bytes on disk the change rewrote or removed.
    pub changed: Vec<Utf8PathBuf>,
    /// Processes restarted because their resource's `reads` covers a
    /// changed path.
    pub restarted: Vec<StopStartResource>,
    /// Processes still running on what they read before the change: their
    /// resource does not declare `reads`, so newgit cannot tell whether a
    /// changed path is one of them.
    pub may_be_stale: Vec<PossiblyStale>,
}

impl StaleRender {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }
}

/// A process a binding change left running on files it may have read before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PossiblyStale {
    pub name: String,
    /// Its long-running action, as `newgit action <name>.<action>` names it.
    pub action: String,
    /// The command that restarts it.
    pub restart_with: String,
}

impl BranchManager {
    /// Every path a render may write in `branch`: what each definition
    /// declares, and what the record says was rendered (a definition edited
    /// since can have moved one).
    fn render_paths(&self, branch: &BranchInstance) -> BTreeSet<Utf8PathBuf> {
        self.resources
            .iter()
            .flat_map(|definition| definition.render.iter().map(|spec| spec.path.clone()))
            .chain(
                branch
                    .resources
                    .values()
                    .flat_map(|binding| binding.rendered.iter().map(|record| record.path.clone())),
            )
            .collect()
    }

    pub(super) fn render_before(&self, instance: &str) -> Result<RenderBefore> {
        let branch = self.store.find_branch(instance)?;
        let supervisor = self.supervisor(&branch);
        let bytes = self
            .render_paths(&branch)
            .into_iter()
            .map(|path| {
                let content = read(&branch.workspace_path, &path);
                (path, content)
            })
            .collect();
        let running = self
            .resources
            .iter()
            .filter(|definition| supervisor.running_pid(&definition.name).is_some())
            .map(|definition| definition.name.clone())
            .collect();
        Ok(RenderBefore { bytes, running })
    }

    /// Compare with `before`, once the change is done, and restart or name
    /// what it left running on a rewritten file. A failed restart is
    /// reported, not raised: the binding has changed by then.
    pub(super) fn settle_stale_render(
        &self,
        instance: &str,
        before: &RenderBefore,
    ) -> Result<StaleRender> {
        let branch = self.store.find_branch(instance)?;
        let mut paths = self.render_paths(&branch);
        paths.extend(before.bytes.keys().cloned());
        let changed: Vec<Utf8PathBuf> = paths
            .into_iter()
            .filter(|path| {
                let was = before.bytes.get(path).cloned().flatten();
                read(&branch.workspace_path, path) != was
            })
            .collect();
        if changed.is_empty() {
            return Ok(StaleRender::default());
        }

        let supervisor = self.supervisor(&branch);
        let mut report = StaleRender {
            changed,
            ..StaleRender::default()
        };
        for definition in self.lifecycle_definitions() {
            let name = &definition.name;
            // Started by this change, it read the new bytes; stopped by it,
            // it reads nothing.
            if !before.running.contains(name) || supervisor.running_pid(name).is_none() {
                continue;
            }
            let action = definition
                .autostart_action()
                .or_else(|| long_running_action(definition));
            let reads = report
                .changed
                .iter()
                .map(|path| definition.reads_path(path))
                .collect::<Option<Vec<bool>>>();
            match reads {
                Some(reads) if !reads.contains(&true) => {}
                Some(_) => {
                    let result = match action {
                        None => Err(format!(
                            "resource `{name}` has no long-running action to restart"
                        )),
                        Some((action_name, action)) => {
                            match supervisor.stop(name, &definition.stop_signal()) {
                                Err(error) => Err(error.to_string()),
                                Ok(StopOutcome::StillRunning(pid)) => Err(format!(
                                    "its process (pid {pid}) did not exit when signalled"
                                )),
                                Ok(_) => self
                                    .start_supervised(&branch, definition, action, action_name)
                                    .map(|_| ())
                                    .map_err(|error| error.to_string()),
                            }
                        }
                    };
                    let (status, error) = match result {
                        Ok(()) => (StopStartStatus::Started, None),
                        Err(error) => (StopStartStatus::Failed, Some(error)),
                    };
                    report.restarted.push(StopStartResource {
                        name: name.clone(),
                        action: action.map_or_else(|| "start".to_owned(), |(n, _)| n.to_owned()),
                        status,
                        error,
                    });
                }
                None => {
                    let action = action.map_or("start", |(action_name, _)| action_name);
                    let restart_with = if definition.actions.contains_key("stop") {
                        format!(
                            "newgit action {name}.stop {0} && newgit action {name}.{action} {0}",
                            branch.name
                        )
                    } else {
                        format!("newgit stop {0} && newgit start {0}", branch.name)
                    };
                    report.may_be_stale.push(PossiblyStale {
                        name: name.clone(),
                        action: action.to_owned(),
                        restart_with,
                    });
                }
            }
        }
        Ok(report)
    }
}

/// A path's bytes, or `None` when there is no file there to read.
fn read(workspace: &Utf8Path, path: &Utf8Path) -> Option<Vec<u8>> {
    std::fs::read(workspace.join(path)).ok()
}
