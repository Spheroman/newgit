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

    /// Live HEAD of a workspace clone, short form.
    pub fn workspace_short_head(workspace: &Utf8Path) -> Result<String> {
        run_git(&["-C", workspace.as_str(), "rev-parse", "--short", "HEAD"])
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
    let output = Command::new("git")
        .args(args)
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
