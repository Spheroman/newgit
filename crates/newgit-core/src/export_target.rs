//! Where `newgit export --remote <name>` pushes: one definition per file in
//! `.newgit/exports/<name>.toml` (docs/design/trackers.md, *Git and GitHub*).
//!
//! GitHub is an export, not the project's remote: a target receives a
//! composed projection of chosen trackers' `main`s, with their history, as an
//! ordinary Git repository. Which trackers is the definition's to say, so a
//! public mirror and a private one can show different views of one project.
//! With no `trackers`, the `public` trackers in Git ship; with `trackers`,
//! exactly those do, so a target can leave out a tracker of any audience (its
//! history stays in its own repo). A tracker outside Git never ships.

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTarget {
    pub name: String,
    /// As written: a Git URL, or `owner/repo` for GitHub.
    pub url: String,
    /// The branch the export is pushed to.
    pub branch: String,
    /// The trackers in Git shipped: exactly these. Empty ships the `public`
    /// ones.
    pub trackers: Vec<String>,
    /// Instances' branches go here too, each time a workspace pushes one —
    /// so a pull request can be opened against the target.
    pub branches: bool,
    /// Export the view rooted at this tracker instead of the project's: its
    /// own files at the top, and while it mounts no tracker in Git, its own
    /// history, ids and signatures intact (a fork that keeps its files at
    /// the root).
    pub root: Option<String>,
}

/// On-disk shape (everything but the filename-derived name).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportTargetFile {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trackers: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    branches: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
}

impl ExportTarget {
    pub fn from_file(name: &str, path: &Utf8Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        Self::from_text(name, path, &contents)
    }

    /// A target from its file's text; `path` names it in errors.
    pub fn from_text(name: &str, path: &Utf8Path, contents: &str) -> Result<Self> {
        let file: ExportTargetFile =
            toml::from_str(contents).map_err(|source| NewgitError::TomlRead {
                path: path.to_path_buf(),
                source,
            })?;
        let target = Self {
            name: name.to_owned(),
            url: file.url,
            branch: file.branch.unwrap_or_else(|| "main".to_owned()),
            trackers: file.trackers,
            branches: file.branches,
            root: file.root,
        };
        target.validate()?;
        Ok(target)
    }

    /// Whether a target without `root` ships tracker `name` of `audience`.
    pub fn lists(&self, name: &str, audience: &str) -> bool {
        if self.trackers.is_empty() {
            audience == "public"
        } else {
            self.trackers.iter().any(|tracker| tracker == name)
        }
    }

    /// Where Git pushes: `owner/repo` is GitHub's; anything else is a URL
    /// (or path) Git already understands, used as written.
    pub fn push_url(&self) -> String {
        if is_github_shorthand(&self.url) {
            format!("https://github.com/{}.git", self.url)
        } else {
            self.url.clone()
        }
    }

    fn validate(&self) -> Result<()> {
        // The name is also the remote's name in a workspace, beside `origin`.
        if self.name == "origin" {
            return Err(self.invalid(
                "`origin` is a workspace's own remote; name the target anything else".to_owned(),
            ));
        }
        if self.url.trim().is_empty() {
            return Err(self.invalid("`url` is empty".to_owned()));
        }
        // What `git check-ref-format --branch` would refuse, where it matters
        // here: a branch that is not one, or an option in disguise.
        let branch = &self.branch;
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.starts_with('/')
            || branch.ends_with('/')
            || branch.ends_with(".lock")
            || branch.contains("..")
            || branch.contains("//")
            || branch
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
        {
            return Err(self.invalid(format!("`branch = \"{branch}\"` is not a branch name")));
        }
        if let Some(root) = &self.root
            && !self.trackers.is_empty()
        {
            return Err(self.invalid(format!(
                "`root = \"{root}\"` ships `{root}` and what it mounts, so `trackers` cannot \
                 also be set: drop one"
            )));
        }
        for (index, tracker) in self.trackers.iter().enumerate() {
            if self.trackers[..index].contains(tracker) {
                return Err(self.invalid(format!("`trackers` names `{tracker}` twice")));
            }
        }
        Ok(())
    }

    fn invalid(&self, reason: String) -> NewgitError {
        NewgitError::InvalidDefinition {
            kind: "export",
            name: self.name.clone(),
            reason,
        }
    }
}

/// `owner/repo`: one slash, no scheme, host, or path. `./repo` and
/// `../repo` stay paths.
fn is_github_shorthand(url: &str) -> bool {
    let Some((owner, repo)) = url.split_once('/') else {
        return false;
    };
    let part = |text: &str| {
        !text.is_empty()
            && !text.starts_with('.')
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    part(owner) && part(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, branch: &str) -> ExportTarget {
        ExportTarget {
            name: "github".to_owned(),
            url: url.to_owned(),
            branch: branch.to_owned(),
            trackers: Vec::new(),
            branches: false,
            root: None,
        }
    }

    #[test]
    fn owner_slash_repo_is_github_and_everything_else_is_used_as_written() {
        assert_eq!(
            target("acme/widget", "main").push_url(),
            "https://github.com/acme/widget.git"
        );
        for url in [
            "git@github.com:acme/widget.git",
            "https://gitlab.com/acme/widget.git",
            "../mirror.git",
            "./mirror",
            "/srv/git/widget.git",
            "acme/widget/extra",
        ] {
            assert_eq!(target(url, "main").push_url(), url);
        }
    }

    #[test]
    fn a_branch_that_is_not_one_is_refused() {
        assert!(target("acme/widget", "release/1.x").validate().is_ok());
        for branch in ["", "-f", "a..b", "a b", "main.lock", "feature/", "a:b"] {
            assert!(
                target("acme/widget", branch).validate().is_err(),
                "`{branch}` should be refused"
            );
        }
    }

    #[test]
    fn root_and_trackers_cannot_both_be_set() {
        let mut both = target("acme/widget", "main");
        both.root = Some("t3code".to_owned());
        assert!(both.validate().is_ok());
        both.trackers = vec!["root".to_owned()];
        assert!(both.validate().is_err());
    }

    #[test]
    fn a_tracker_named_twice_is_refused() {
        let mut twice = target("acme/widget", "main");
        twice.trackers = vec!["root".to_owned(), "root".to_owned()];
        assert!(twice.validate().is_err());
    }
}
