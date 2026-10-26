//! Where `newgit export --remote <name>` pushes: one definition per file in
//! `.newgit/exports/<name>.toml` (newgit-trackers.md, *Git and GitHub*).
//!
//! GitHub is an export, not the project's remote: a target receives a
//! composed projection of chosen trackers' `main`s, with their history, as an
//! ordinary Git repository. Which trackers is the definition's to say, so a
//! public mirror and a private one can show different views of one project.
//! `public` code trackers always ship; `trackers` adds others by name. A
//! secret tracker never ships.

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
    /// Code trackers shipped besides the `public` ones.
    pub trackers: Vec<String>,
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
}

impl ExportTarget {
    pub fn from_file(name: &str, path: &Utf8Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
        let file: ExportTargetFile =
            toml::from_str(&contents).map_err(|source| NewgitError::TomlRead {
                path: path.to_path_buf(),
                source,
            })?;
        let target = Self {
            name: name.to_owned(),
            url: file.url,
            branch: file.branch.unwrap_or_else(|| "main".to_owned()),
            trackers: file.trackers,
        };
        target.validate()?;
        Ok(target)
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
    fn a_tracker_named_twice_is_refused() {
        let mut twice = target("acme/widget", "main");
        twice.trackers = vec!["root".to_owned(), "root".to_owned()];
        assert!(twice.validate().is_err());
    }
}
