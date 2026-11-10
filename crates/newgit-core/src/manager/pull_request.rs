//! Pull requests: an instance's pushed work proposed for a branch, and a
//! reviewer spawning it (#167, #170).
//!
//! A pull request names an instance, not a ref. Its head is the instance's
//! latest checkpoint on the remote, read when asked, so a push moves it and
//! each checkpoint is a revision. Checking one out is `spawn
//! --from-checkpoint` on that head: the code, the trackers the reviewer may
//! read (with their own per-user content), and the resources.

use super::{BranchManager, SpawnFromCheckpointOutcome};
use crate::error::{NewgitError, Result};
use crate::remote::{Acting, NewPullRequest, RemotePull};
use crate::source::{GitSource, run_git_env};

/// What `pr create` was asked for. Unset fields come from the workspace.
#[derive(Debug, Clone, Default)]
pub struct PullRequestSpec {
    /// The branch to propose it for; the project's default when unset.
    pub base: Option<String>,
    /// The title; the subject of the workspace's last commit when unset.
    pub title: Option<String>,
    /// The body; the rest of that commit's message when unset.
    pub body: Option<String>,
    pub draft: bool,
}

impl BranchManager {
    /// Open a pull request for `instance`'s pushed work. Refused while its
    /// workspace has commits the remote does not: what is proposed is what
    /// was pushed, never something only this machine has.
    pub fn create_pull_request(
        &self,
        instance: &str,
        spec: &PullRequestSpec,
    ) -> Result<RemotePull> {
        let link = self.remote.as_ref().ok_or(NewgitError::NoRemote)?;
        let branch = self.store.find_branch(instance)?;
        let head = GitSource::workspace_head(&branch.workspace_path)?;
        let (title, body) = match (&spec.title, &spec.body) {
            (Some(title), Some(body)) => (title.clone(), body.clone()),
            (title, body) => {
                let workspace = branch.workspace_path.as_str();
                let message =
                    |format: &str| run_git_env(&["-C", workspace, "log", "-1", format, &head], &[]);
                let title = match title {
                    Some(title) => title.clone(),
                    None => message("--format=%s")?,
                };
                let body = match body {
                    Some(body) => body.clone(),
                    None => message("--format=%b")?,
                };
                (title, body)
            }
        };
        if title.trim().is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "`{}`'s last commit has no subject to use as a title: give one with --title",
                branch.name
            )));
        }
        // As the workspace, as a push is: an agent holding only its
        // workspace's token may open one for its own instance.
        let remote = link.connect(Acting::Workspace(&branch.workspace_path))?;
        remote.create_pull(
            &link.project,
            &NewPullRequest {
                instance: &branch.slug,
                base: spec.base.as_deref(),
                title: title.trim(),
                body: body.trim_end(),
                draft: spec.draft,
                head_rev: &head,
            },
        )
    }

    /// Pull request `number`, its body and head included.
    pub fn pull_request(&self, number: u64) -> Result<RemotePull> {
        let link = self.remote.as_ref().ok_or(NewgitError::NoRemote)?;
        link.connect(Acting::Login)?.pull(&link.project, number)
    }

    /// Pull requests in `state` (`open`, `closed`, `merged`, `all`).
    pub fn pull_requests(&self, state: &str, limit: usize) -> Result<Vec<RemotePull>> {
        let link = self.remote.as_ref().ok_or(NewgitError::NoRemote)?;
        link.connect(Acting::Login)?
            .pulls(&link.project, state, limit)
    }

    /// Spawn pull request `number`'s head here as instance `name` (default
    /// `pr-<number>`): `spawn --from-checkpoint` on its latest checkpoint.
    pub fn checkout_pull_request(
        &self,
        number: u64,
        name: Option<&str>,
    ) -> Result<(RemotePull, SpawnFromCheckpointOutcome)> {
        let pull = self.pull_request(number)?;
        let Some(head) = pull.head.clone() else {
            return Err(NewgitError::Unsupported(format!(
                "pull request #{number} has no head: its instance `{}` is gone from the remote",
                pull.instance
            )));
        };
        let name = name.map_or_else(|| format!("pr-{number}"), str::to_owned);
        if self.store.find_branch(&name).is_ok() {
            return Err(NewgitError::Unsupported(format!(
                "an instance `{name}` exists here already: `newgit remove {name}` first, or pick another name with --name"
            )));
        }
        let outcome = self.spawn_from_checkpoint(&name, &head.instance, &head.id)?;
        Ok((pull, outcome))
    }
}
