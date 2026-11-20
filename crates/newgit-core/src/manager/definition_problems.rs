//! What [`crate::definition_check`] finds, as `status` and `spawn` name it:
//! for the resources and trackers an instance binds.

use super::BranchManager;
use crate::branch::BranchInstance;
use crate::definition_check::{self, DefinitionProblem, Owner};

impl BranchManager {
    /// Every definition problem in the project: names that refer to nothing
    /// and template placeholders left in. `newgit resource check` reports
    /// all of them.
    pub fn definition_problems(&self) -> Vec<DefinitionProblem> {
        definition_check::check(
            self.resource_definitions(),
            self.tracker_definitions(),
            &self.store.paths().metadata_root,
        )
    }

    /// The ones in what `branch` binds, one line each, for its readiness.
    pub(super) fn bound_definition_problems(&self, branch: &BranchInstance) -> Vec<String> {
        self.definition_problems()
            .into_iter()
            .filter(|problem| match problem.owner {
                Owner::Resource => branch.resources.contains_key(&problem.name),
                Owner::Tracker => branch.trackers.contains_key(&problem.name),
            })
            .map(|problem| problem.to_string())
            .collect()
    }
}
