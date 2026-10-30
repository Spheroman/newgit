//! The base a Claude Code worktree starts from: the project branch as it
//! stands, the newest this machine knows of it, rather than whatever the
//! project root happens to have checked out.
//!
//! `newgit spawn` with no `--from` starts at the project root's `HEAD`, as
//! `git branch` and `git worktree add` do. Claude Code's own worktrees do
//! not: by default (`worktree.baseRef = "fresh"`) they branch from the
//! repository's default branch on the remote, fetching it when the
//! repository has not been fetched for a day. The `WorktreeCreate` hook
//! replaces that behaviour, so it keeps that contract, in newgit's terms:
//!
//! - The line is the *project branch* (`.newgit/local/project-branch`, the
//!   project root's own branch), not whatever the root has checked out.
//! - Its upstream is fetched first when nothing in the root has fetched for
//!   [`FETCH_WINDOW`], bounded by [`FETCH_TIMEOUT`] and never prompting; a
//!   failed fetch falls back to the ref already there, and is not retried
//!   for [`RETRY_AFTER_FAILURE`].
//! - Of the local branch and its upstream, the base is whichever contains
//!   the other: the upstream when the root's branch is only behind it (a PR
//!   merged on GitHub that nobody has pulled), the local branch when it is
//!   ahead (a `newgit merge` not yet exported) or the two have diverged (the
//!   project's own line; nothing here can say which side is right).
//!
//! All of it runs under the store's fresh-base lock
//! ([`MetadataStore::lock_fresh_base`]), so subagents launched together —
//! whose hooks run at the same moment — see one fetch and one answer.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use camino::Utf8Path;

use crate::error::{NewgitError, Result};
use crate::source::run_git_env;
use crate::store::MetadataStore;

/// How long a fetch in the project root counts as fresh: Claude Code's own.
pub const FETCH_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the fetch may take before the cached ref is used instead:
/// Claude Code's own cap.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long after a failed fetch the next hook skips fetching and uses the
/// ref already there: long enough to cover subagents launched together.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// What a new worktree's instance starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshBase {
    /// What to hand `spawn --from`: the project branch's name when the base
    /// is the local branch (so the instance records it as its base and
    /// `status` can report drift), or the commit itself when it is the
    /// upstream.
    pub from: String,
    /// The same, for people: `main`, `origin/main`, or `HEAD`.
    pub label: String,
    /// Why the upstream could not be fetched, when it could not.
    pub fetch_warning: Option<String>,
}

/// Resolve the base a Claude Code worktree starts from (see the module
/// docs), fetching the project branch's upstream first if the root has not
/// fetched within [`FETCH_WINDOW`].
pub fn fresh_base(store: &MetadataStore) -> Result<FreshBase> {
    let root = store.paths().project_root.clone();
    let git = |args: &[&str]| -> Option<String> {
        let mut full = vec!["-C", root.as_str()];
        full.extend_from_slice(args);
        run_git_env(&full, &[])
            .ok()
            .filter(|output| !output.is_empty())
    };
    let head = || FreshBase {
        from: "HEAD".to_owned(),
        label: "HEAD".to_owned(),
        fetch_warning: None,
    };

    let branch = std::fs::read_to_string(store.paths().project_branch_file())
        .ok()
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty())
        .or_else(|| git(&["symbolic-ref", "--short", "-q", "HEAD"]));
    let Some(branch) = branch else {
        return Ok(head());
    };

    let _lock = store.lock_fresh_base()?;

    let upstream = git(&[
        "rev-parse",
        "--symbolic-full-name",
        &format!("{branch}@{{upstream}}"),
    ]);
    let mut fetch_warning = None;
    if let Some(upstream) = upstream.as_deref()
        && upstream.starts_with("refs/remotes/")
        && let (Some(remote), Some(merge)) = (
            git(&["config", "--get", &format!("branch.{branch}.remote")]),
            git(&["config", "--get", &format!("branch.{branch}.merge")]),
        )
    {
        let fetch_head = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "FETCH_HEAD",
        ]);
        // A failed fetch leaves FETCH_HEAD as it was, so on its own every
        // hook in a burst would try again, and one that got through after
        // others had given up would start from a different commit. The lock
        // file remembers the failure for a moment, and the rest of the burst
        // uses the ref already here, as the one that failed did.
        let failed = &store.paths().fresh_base_lock;
        if !fetched_within(fetch_head.as_deref().map(Utf8Path::new), FETCH_WINDOW) {
            if recently_failed(failed) {
                fetch_warning = Some(format!(
                    "a fetch from {remote} failed within the last {}s",
                    RETRY_AFTER_FAILURE.as_secs()
                ));
            } else {
                fetch_warning = fetch(&root, &remote, &format!("+{merge}:{upstream}")).err();
                let mark = if fetch_warning.is_some() {
                    "failed\n"
                } else {
                    ""
                };
                std::fs::write(failed, mark).map_err(|source| NewgitError::io(failed, source))?;
            }
        }
    }

    let local = git(&[
        "rev-parse",
        "--verify",
        "-q",
        &format!("refs/heads/{branch}^{{commit}}"),
    ]);
    let theirs = upstream.as_deref().and_then(|upstream| {
        git(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{upstream}^{{commit}}"),
        ])
        .map(|rev| (upstream.to_owned(), rev))
    });
    let is_ancestor = |ancestor: &str, descendant: &str| {
        Command::new("git")
            .args(["-C", root.as_str(), "merge-base", "--is-ancestor"])
            .args([ancestor, descendant])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let ours = |fetch_warning| FreshBase {
        from: branch.clone(),
        label: branch.clone(),
        fetch_warning,
    };
    Ok(match (local, theirs) {
        (None, None) => FreshBase {
            fetch_warning,
            ..head()
        },
        (Some(_), None) => ours(fetch_warning),
        (Some(local), Some((_, rev))) if local == rev || !is_ancestor(&local, &rev) => {
            ours(fetch_warning)
        }
        (_, Some((upstream, rev))) => FreshBase {
            from: rev,
            label: upstream
                .strip_prefix("refs/remotes/")
                .unwrap_or(&upstream)
                .to_owned(),
            fetch_warning,
        },
    })
}

/// Whether the last fetch, recorded in `mark` (the fresh-base lock file),
/// failed within [`RETRY_AFTER_FAILURE`].
fn recently_failed(mark: &Utf8Path) -> bool {
    std::fs::read_to_string(mark).is_ok_and(|text| text.trim() == "failed")
        && fetched_within(Some(mark), RETRY_AFTER_FAILURE)
}

/// Whether `fetch_head` was written within `window`: anything in the root
/// fetching counts, the user's own `git fetch` or `git pull` included.
fn fetched_within(fetch_head: Option<&Utf8Path>, window: Duration) -> bool {
    fetch_head
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < window)
}

/// `git fetch <remote> <refspec>` in `root`, given up after
/// [`FETCH_TIMEOUT`], and never waiting on a password, a passphrase, or a
/// new host key: a hook has no terminal to ask on.
fn fetch(root: &Utf8Path, remote: &str, refspec: &str) -> std::result::Result<(), String> {
    let mut command = Command::new("git");
    command
        .args(["-C", root.as_str(), "fetch", "--quiet", remote, refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run git fetch: {error}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(format!("git fetch {remote} failed: {}", stderr.trim()));
            }
            Ok(None) if started.elapsed() >= FETCH_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git fetch {remote} took longer than {}s",
                    FETCH_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("waiting on git fetch: {error}")),
        }
    }
}
