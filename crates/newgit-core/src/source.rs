use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use crate::config::SourceSubstrate;
use crate::error::{NewgitError, Result};

/// Shell-out driver for the source tracker. Everything goes through Git's
/// public interface — never into `.git` directly — so it works identically
/// against a clone or, later, a projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    root: Utf8PathBuf,
}

impl GitSource {
    pub fn open(root: &Utf8Path, substrate: SourceSubstrate) -> Result<Self> {
        if root.join(".git").exists() {
            return Ok(Self {
                root: root.to_path_buf(),
            });
        }
        match substrate {
            SourceSubstrate::Jj => Err(NewgitError::Unsupported(format!(
                "{root} is a jj repository without a colocated .git; v1 drives source through \
                 Git — recreate it with `jj git init --colocate`"
            ))),
            SourceSubstrate::Git => Err(NewgitError::NotAGitRepo(root.to_path_buf())),
        }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Whether the store repo's gitignore rules cover `path` (which need not
    /// exist yet) — used to enforce the tracker-path invariant.
    pub fn is_ignored(&self, path: &str) -> Result<bool> {
        let args = ["-C", self.root.as_str(), "check-ignore", "-q", "--", path];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    /// Whether `path` is tracked by the store repo — a tracker path that is
    /// also in Git history is dual-tracked, deliberately or not.
    pub fn is_tracked(&self, path: &str) -> Result<bool> {
        self.git(&["ls-files", "--", path])
            .map(|stdout| !stdout.is_empty())
    }

    pub fn branch_exists(&self, name: &str) -> Result<bool> {
        let args = [
            "-C",
            self.root.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    pub fn create_branch(&self, name: &str, base: &str) -> Result<()> {
        self.git(&["branch", "--", name, base]).map(|_| ())
    }

    /// The branch name `revision` names, when it names one at all: the
    /// literal name if `revision` already is an existing local branch, or
    /// whatever `HEAD` currently points at when `revision` is `"HEAD"`.
    /// `None` for a bare SHA, a tag, or a detached `HEAD` — those name a
    /// fixed point, not a moving line `status` could later compare a base
    /// against.
    pub fn resolve_branch_name(&self, revision: &str) -> Result<Option<String>> {
        if revision == "HEAD" {
            let args = [
                "-C",
                self.root.as_str(),
                "symbolic-ref",
                "--short",
                "-q",
                "HEAD",
            ];
            let output = Command::new("git")
                .args(args)
                .output()
                .map_err(|source| spawn_error(&args, &source))?;
            return Ok(output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()));
        }
        if self.branch_exists(revision)? {
            return Ok(Some(revision.to_owned()));
        }
        Ok(None)
    }

    /// How many commits `range`'s right side has that its left side lacks —
    /// `status`'s "base has moved N commits" count. Computed fresh on every
    /// call against the store repo's own refs; it is never cached, so the
    /// number is exactly as current as the store's last fetch of upstream
    /// and never silently stale.
    pub fn commit_count(&self, range: &str) -> Result<u32> {
        let count = self.git(&["rev-list", "--count", range])?;
        count.parse().map_err(|_| {
            NewgitError::Unsupported(format!(
                "unexpected output from `git rev-list --count {range}`: {count}"
            ))
        })
    }

    pub fn rev_parse(&self, revision: &str) -> Result<String> {
        self.git(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")])
            .map_err(|error| {
                if revision == "HEAD" {
                    NewgitError::Unsupported(
                        "the store repository has no commits yet; make an initial commit before \
                         spawning"
                            .to_owned(),
                    )
                } else {
                    error
                }
            })
    }

    /// Materialize `branch` as a full standalone clone at `destination`.
    /// A local-path clone hardlinks objects; never `--shared`/`--reference`.
    pub fn clone_to(&self, branch: &str, destination: &Utf8Path) -> Result<()> {
        run_git(&[
            "clone",
            "--quiet",
            "--branch",
            branch,
            "--",
            self.root.as_str(),
            destination.as_str(),
        ])
        .map(|_| ())
    }

    /// Fetch a ref from another repository (typically a workspace clone)
    /// into the store. Fetch, never push: the store pulls commits in when a
    /// checkpoint blesses them, and workspaces stay passive.
    pub fn fetch_ref(&self, from: &Utf8Path, remote_ref: &str, local_ref: &str) -> Result<()> {
        self.git(&[
            "fetch",
            "--quiet",
            "--no-write-fetch-head",
            "--",
            from.as_str(),
            &format!("+{remote_ref}:{local_ref}"),
        ])
        .map(|_| ())
    }

    pub fn update_ref(&self, name: &str, rev: &str) -> Result<()> {
        self.git(&["update-ref", name, rev]).map(|_| ())
    }

    /// Every ref under a namespace, e.g. `refs/newgit/checkpoints/<slug>`.
    pub fn refs_under(&self, prefix: &str) -> Result<Vec<String>> {
        let listing = self.git(&["for-each-ref", "--format=%(refname)", prefix])?;
        Ok(listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn delete_ref(&self, name: &str) -> Result<()> {
        self.git(&["update-ref", "-d", name]).map(|_| ())
    }

    /// Resolve a ref to a commit, when it exists.
    pub fn ref_rev(&self, name: &str) -> Option<String> {
        self.git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])
        .ok()
        .filter(|rev| !rev.is_empty())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let args = [
            "-C",
            self.root.as_str(),
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    /// Live HEAD of a workspace clone, short form.
    pub fn workspace_short_head(workspace: &Utf8Path) -> Result<String> {
        run_git(&["-C", workspace.as_str(), "rev-parse", "--short", "HEAD"])
    }

    /// Live HEAD of a workspace clone, full form.
    pub fn workspace_head(workspace: &Utf8Path) -> Result<String> {
        run_git(&["-C", workspace.as_str(), "rev-parse", "HEAD"])
    }

    /// Committed content of one path in a workspace clone: the blob at HEAD,
    /// not what is on disk. `None` when HEAD has no such path.
    ///
    /// This is what a render substitutes into — see [`crate::render::apply`].
    /// Read raw, never through [`run_git`]: that trims, which is right for a
    /// rev and silently destructive for file content — it would drop the
    /// file's trailing newline on every render.
    pub fn workspace_show_head(workspace: &Utf8Path, path: &Utf8Path) -> Result<Option<String>> {
        let args = ["-C", workspace.as_str(), "show", &format!("HEAD:{path}")];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        // `git show` fails the same way for "no such path at HEAD" as for a
        // broken repo; the caller has already established the latter is not
        // the case, and treats absence as "nothing committed to render from".
        if !output.status.success() {
            return Ok(None);
        }
        // Strict rather than lossy: rendering into a file newgit cannot read
        // as text would write back mojibake where the project's bytes were.
        match String::from_utf8(output.stdout) {
            Ok(contents) => Ok(Some(contents)),
            Err(_) => Err(NewgitError::Unsupported(format!(
                "`{path}` is not valid UTF-8 at HEAD; a render substitutes text"
            ))),
        }
    }

    /// Mark paths `--skip-worktree` in a workspace clone, so this instance's
    /// rendered values never show as a modification and cannot be committed
    /// by an agent running `git add -A`.
    ///
    /// The counterpart of `.git/info/exclude` for tracker paths: newgit does
    /// not control the Git an agent runs, so what must not be committable has
    /// to be made so by construction. v1's stand-in for the projection a v2
    /// materializer does properly.
    pub fn workspace_skip_worktree(workspace: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = vec![
            "-C",
            workspace.as_str(),
            "update-index",
            "--skip-worktree",
            "--",
        ];
        args.extend(paths.iter().map(|path| path.as_str()));
        run_git(&args).map(|_| ())
    }

    /// Snapshot uncommitted and untracked (non-ignored) workspace state as a
    /// dangling commit on top of HEAD, without touching HEAD, the real
    /// index, or the worktree. Returns `None` when the worktree is clean.
    ///
    /// Mechanics: a throwaway `GIT_INDEX_FILE` seeded from HEAD, `git add -A`
    /// into it, `git write-tree`, and `git commit-tree` — all public
    /// interface, nothing reaches into `.git` internals.
    ///
    /// `rendered` paths are marked skip-worktree *in the throwaway index*.
    /// The real index carries that bit already, but a fresh index seeded from
    /// HEAD does not, so without this `git add -A` would sweep the instance's
    /// rendered ports into the checkpoint — the one place skip-worktree does
    /// not protect on its own.
    pub fn workspace_dirty_commit(
        workspace: &Utf8Path,
        message: &str,
        rendered: &[Utf8PathBuf],
    ) -> Result<Option<String>> {
        let scratch = tempfile::tempdir().map_err(|source| NewgitError::io(workspace, source))?;
        let index = scratch.path().join("index");
        let Some(index) = index.to_str() else {
            return Err(NewgitError::NonUtf8Path(index.display().to_string()));
        };
        let env = [("GIT_INDEX_FILE".to_owned(), index.to_owned())];

        let ws = workspace.as_str();
        run_git_env(&["-C", ws, "read-tree", "HEAD"], &env)?;
        if !rendered.is_empty() {
            let mut args: Vec<&str> = vec!["-C", ws, "update-index", "--skip-worktree", "--"];
            args.extend(rendered.iter().map(|path| path.as_str()));
            run_git_env(&args, &env)?;
        }
        run_git_env(&["-C", ws, "add", "-A"], &env)?;
        let tree = run_git_env(&["-C", ws, "write-tree"], &env)?;

        let head_tree = run_git(&["-C", ws, "rev-parse", "HEAD^{tree}"])?;
        if tree == head_tree {
            return Ok(None);
        }

        // A synthetic commit needs an identity even where none is configured.
        let ident = [
            ("GIT_AUTHOR_NAME".to_owned(), "newgit".to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), "newgit@localhost".to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), "newgit".to_owned()),
            (
                "GIT_COMMITTER_EMAIL".to_owned(),
                "newgit@localhost".to_owned(),
            ),
        ];
        run_git_env(
            &["-C", ws, "commit-tree", &tree, "-p", "HEAD", "-m", message],
            &ident,
        )
        .map(Some)
    }

    pub fn workspace_update_ref(workspace: &Utf8Path, name: &str, rev: &str) -> Result<()> {
        run_git(&["-C", workspace.as_str(), "update-ref", name, rev]).map(|_| ())
    }

    pub fn workspace_delete_ref(workspace: &Utf8Path, name: &str) -> Result<()> {
        run_git(&["-C", workspace.as_str(), "update-ref", "-d", name]).map(|_| ())
    }

    /// Fetch a store ref into a workspace clone (undo may need objects the
    /// workspace has since discarded).
    pub fn workspace_fetch_ref(
        workspace: &Utf8Path,
        from: &Utf8Path,
        remote_ref: &str,
    ) -> Result<()> {
        run_git(&[
            "-C",
            workspace.as_str(),
            "fetch",
            "--quiet",
            "--no-write-fetch-head",
            "--",
            from.as_str(),
            remote_ref,
        ])
        .map(|_| ())
    }

    /// Put a workspace back to a checkpointed source state: HEAD hard-reset
    /// to `head_rev`, untracked (non-ignored) files removed, then — when the
    /// checkpoint carried uncommitted state — that state reapplied to the
    /// worktree as uncommitted changes again. Ignored files (tracker paths,
    /// node_modules) are deliberately left alone; trackers and resources own
    /// their restoration.
    pub fn workspace_restore_to(
        workspace: &Utf8Path,
        head_rev: &str,
        dirty_rev: Option<&str>,
    ) -> Result<()> {
        let ws = workspace.as_str();
        run_git(&["-C", ws, "reset", "--quiet", "--hard", head_rev])?;
        run_git(&["-C", ws, "clean", "-fdq"])?;
        if let Some(dirty) = dirty_rev {
            run_git(&["-C", ws, "checkout", "--quiet", dirty, "--", "."])?;
            // Mixed reset: index back to head_rev, so the dirty snapshot
            // shows as uncommitted modifications/untracked files — exactly
            // how it looked when the checkpoint was taken.
            run_git(&["-C", ws, "reset", "--quiet", head_rev])?;
        }
        Ok(())
    }

    /// Every path the workspace's Git tracks, workspace-relative. This is
    /// what "the source content of this branch" means for export: tracked
    /// files as they stand on disk, so uncommitted edits are included and
    /// ignored junk never is.
    pub fn workspace_tracked_files(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
        let listing = run_git(&["-C", workspace.as_str(), "ls-files", "-z"])?;
        Ok(listing
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(Utf8PathBuf::from)
            .collect())
    }

    /// Turn a directory of already-placed files into an ordinary Git
    /// repository with one commit on `branch`. Returns the commit.
    ///
    /// One commit, never a history rewrite: exporting the workspace's Git
    /// history would carry every file any past commit contained, which is
    /// exactly the content the audience filter just excluded.
    pub fn init_export_repo(destination: &Utf8Path, branch: &str, message: &str) -> Result<String> {
        let dest = destination.as_str();
        run_git(&["init", "--quiet", "-b", branch, "--", dest])?;
        run_git(&["-C", dest, "add", "-A"])?;
        run_git_env(
            &["-C", dest, "commit", "--quiet", "-m", message],
            &export_identity(destination),
        )?;
        run_git(&["-C", dest, "rev-parse", "HEAD"])
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let mut full: Vec<&str> = vec!["-C", self.root.as_str()];
        full.extend_from_slice(args);
        run_git(&full)
    }
}

/// Walk upward looking for a source repo root. `.jj` wins over `.git` at the
/// same level (a colocated repo is still a jj repo).
pub fn find_repo_root(start: &Utf8Path) -> Option<(Utf8PathBuf, SourceSubstrate)> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join(".jj").is_dir() {
            return Some((current.to_path_buf(), SourceSubstrate::Jj));
        }
        if current.join(".git").exists() {
            return Some((current.to_path_buf(), SourceSubstrate::Git));
        }
        dir = current.parent();
    }
    None
}

fn run_git(args: &[&str]) -> Result<String> {
    run_git_env(args, &[])
}

/// The user's own Git identity when they have one, so an exported repo looks
/// like their work; a newgit identity only where Git would otherwise refuse
/// to commit at all.
fn export_identity(destination: &Utf8Path) -> Vec<(String, String)> {
    if run_git(&["-C", destination.as_str(), "var", "GIT_COMMITTER_IDENT"]).is_ok() {
        return Vec::new();
    }
    ["AUTHOR", "COMMITTER"]
        .iter()
        .flat_map(|role| {
            [
                (format!("GIT_{role}_NAME"), "newgit".to_owned()),
                (format!("GIT_{role}_EMAIL"), "newgit@localhost".to_owned()),
            ]
        })
        .collect()
}

fn run_git_env(args: &[&str], env: &[(String, String)]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|source| spawn_error(args, &source))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(NewgitError::SourceCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn spawn_error(args: &[&str], source: &std::io::Error) -> NewgitError {
    NewgitError::SourceCommand {
        command: format!("git {}", args.join(" ")),
        stderr: source.to_string(),
    }
}
