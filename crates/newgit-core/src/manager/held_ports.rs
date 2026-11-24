//! Who holds the ports an instance claims.
//!
//! A claim is a line in a binding record: nothing holds the port until the
//! instance's own process binds it, and only newgit respects a claim. So the
//! port can be taken first, by another instance's dev tool picking a port for
//! itself or by any other program, and then the claimant's server fails to
//! bind, or worse, the claimant reads as up because *something* answers on
//! its port. `status` (and so the end of `spawn`, `start` and `rebind`) and
//! `ports --check` decide whose a port is by the process listening on it,
//! named with its pid, directory and instance (see [`crate::ports::owner`]).

use std::collections::{BTreeMap, BTreeSet};

use super::BranchManager;
use crate::branch::{BranchInstance, ResourceStatus};
use crate::error::Result;
use crate::ports::{self, Claim, InstanceProcesses, PortCheck};
use crate::supervisor::{ProcessState, Supervisor};

impl BranchManager {
    /// Every instance, with the processes newgit supervises for it that are
    /// running.
    fn instance_processes(&self, branches: &[BranchInstance]) -> Vec<InstanceProcesses> {
        branches
            .iter()
            .map(|branch| {
                let supervisor = self.supervisor(branch);
                let pids = branch
                    .resources
                    .keys()
                    .filter_map(|name| supervisor.running_pid(name))
                    .collect();
                InstanceProcesses::new(&branch.name, &branch.workspace_path, pids)
            })
            .collect()
    }

    /// `newgit ports --check`: every port an instance of this project claims,
    /// judged by who listens on it, and every port an instance's process
    /// listens on that nothing claims (see [`ports::check_ports`]).
    pub fn check_claimed_ports(&self) -> Result<Vec<PortCheck>> {
        let branches = self.store.load_branches()?;
        let instances = self.instance_processes(&branches);
        let mut claims = Vec::new();
        for branch in &branches {
            let supervisor = self.supervisor(branch);
            for (resource, binding) in branch.port_claims() {
                let supervised = self
                    .resource_definition(resource)
                    .is_ok_and(|definition| definition.has_long_running_action());
                let running = (!branch.parked.contains_key(resource))
                    .then(|| supervisor.running_pid(resource))
                    .flatten();
                for (name, port) in &binding.resolved_ports {
                    claims.push(Claim {
                        instance: branch.name.clone(),
                        resource: resource.clone(),
                        name: name.clone(),
                        port: *port,
                        supervised,
                        running,
                    });
                }
            }
        }
        let listeners = ports::listeners(None)?;
        Ok(ports::check_ports(&claims, &listeners, &instances))
    }

    /// For [`Self::readiness`]: each resource of `branch` that should be up,
    /// with a problem for every claimed port of it somebody else holds. That
    /// is another instance's process, always; or, for a resource whose
    /// process newgit supervises and which is not running, anything that is
    /// not this instance's (it cannot be its own server). A resource newgit
    /// runs no process of may hold its ports through one nothing here can
    /// tie to it (Docker's proxy), so only another instance's process counts
    /// against it.
    ///
    /// Asks the OS only about ports something listens on, so an instance
    /// with nothing up costs no `lsof`. When `lsof` cannot run, says nothing.
    pub(super) fn ports_held_elsewhere(
        &self,
        branch: &BranchInstance,
        supervisor: &Supervisor,
        suspended: &BTreeSet<String>,
    ) -> BTreeMap<String, Vec<String>> {
        let mut wanted: Vec<(&String, &String, u16, bool)> = Vec::new();
        for (resource, binding) in &branch.resources {
            if binding.status != ResourceStatus::Ready || suspended.contains(resource) {
                continue;
            }
            let Ok(definition) = self.resource_definition(resource) else {
                continue;
            };
            let died = definition.has_long_running_action()
                && matches!(supervisor.state(resource), ProcessState::Died(_));
            for (name, port) in &binding.resolved_ports {
                if ports::listening(*port) {
                    wanted.push((resource, name, *port, died));
                }
            }
        }
        if wanted.is_empty() {
            return BTreeMap::new();
        }
        let asked: Vec<u16> = wanted.iter().map(|(_, _, port, _)| *port).collect();
        let Ok(listeners) = ports::listeners(Some(&asked)) else {
            return BTreeMap::new();
        };
        let Ok(branches) = self.store.load_branches() else {
            return BTreeMap::new();
        };
        let instances = self.instance_processes(&branches);
        let mut held: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (resource, name, port, died) in wanted {
            let mut seen = BTreeSet::new();
            for listener in listeners
                .iter()
                .filter(|listener| listener.port == port && seen.insert(listener.pid))
            {
                let owner = ports::owner(listener, &instances);
                if owner == Some(branch.name.as_str()) || (owner.is_none() && !died) {
                    continue;
                }
                let restart = if died {
                    format!("`newgit start {}`", branch.name)
                } else {
                    format!("`newgit stop {0} && newgit start {0}`", branch.name)
                };
                let cause = if owner.is_some() {
                    "A tool there picked a port by itself: declare every port it opens in \
                     `[ports]` and pass each to it explicitly (`newgit reference ports`); "
                } else {
                    ""
                };
                held.entry(resource.clone()).or_default().push(format!(
                    "`{resource}`'s port {port} (`{name}`) is held by {}, not by `{resource}`. \
                     {cause}{restart} moves the claim to a free port",
                    ports::describe_holder(listener, owner)
                ));
            }
        }
        held
    }
}
