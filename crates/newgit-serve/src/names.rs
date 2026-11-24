//! Names: projects, repos, and where each repo lives on disk.
//!
//! The API still names repos as the Worker did (`<owner>.<project>.t.<tracker>`,
//! `.t.<tracker>.<login>`, `.i.<instance>.<tracker>`), so the CLI is unchanged.
//! On disk they are laid out by home, so a project's state and stores are one
//! directory: `homes/<owner>/<project>/`. That directory is the unit a
//! second node would take over.
//!
//! None of those repos is a repository of its own: each is a Git namespace
//! in its audience's store (`crate::repos`). A tracker is `t/<tracker>`; an
//! instance's fork of it is `t/<tracker>/i/<instance>`, inside it, so making
//! one copies refs, not objects.

use camino::{Utf8Path, Utf8PathBuf};

/// A project, as `<owner>/<name>`. The owner is a GitHub login, lowercased.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectId {
    pub owner: String,
    pub name: String,
}

impl ProjectId {
    /// `<owner>/<name>`: the project's key, and its home.
    pub fn key(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The project's home: its stores.
    pub fn dir(&self, data: &Utf8Path) -> Utf8PathBuf {
        data.join("homes").join(&self.owner).join(&self.name)
    }

    /// The prefix every repo name of this project starts with.
    pub fn repo_prefix(&self) -> String {
        format!("{}.{}.", self.owner, self.name)
    }
}

pub fn parse_project_key(key: &str) -> Option<ProjectId> {
    let mut parts = key.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || !is_part(owner) || !is_part(name) {
        return None;
    }
    Some(ProjectId {
        owner: owner.to_lowercase(),
        name: name.to_owned(),
    })
}

/// One repo, as the API names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRef {
    Tracker {
        project: ProjectId,
        tracker: String,
        /// A per-user tracker's content for one user.
        login: Option<String>,
    },
    Fork {
        project: ProjectId,
        instance: String,
        tracker: String,
    },
}

impl RepoRef {
    pub fn tracker(project: &ProjectId, tracker: &str) -> Self {
        Self::Tracker {
            project: project.clone(),
            tracker: tracker.to_owned(),
            login: None,
        }
    }

    pub fn project(&self) -> &ProjectId {
        match self {
            Self::Tracker { project, .. } | Self::Fork { project, .. } => project,
        }
    }

    pub fn tracker_name(&self) -> &str {
        match self {
            Self::Tracker { tracker, .. } | Self::Fork { tracker, .. } => tracker,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Tracker {
                project,
                tracker,
                login: None,
            } => format!("{}t.{tracker}", project.repo_prefix()),
            Self::Tracker {
                project,
                tracker,
                login: Some(login),
            } => format!("{}t.{tracker}.{login}", project.repo_prefix()),
            Self::Fork {
                project,
                instance,
                tracker,
            } => format!("{}i.{instance}.{tracker}", project.repo_prefix()),
        }
    }
}

pub fn parse_repo_name(name: &str) -> Option<RepoRef> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.iter().any(|part| !is_part(part)) {
        return None;
    }
    let project = ProjectId {
        owner: parts.first()?.to_lowercase(),
        name: (*parts.get(1)?).to_owned(),
    };
    match &parts[2..] {
        ["t", tracker] => Some(RepoRef::Tracker {
            project,
            tracker: (*tracker).to_owned(),
            login: None,
        }),
        ["t", tracker, login] => Some(RepoRef::Tracker {
            project,
            tracker: (*tracker).to_owned(),
            login: Some(login.to_lowercase()),
        }),
        ["i", instance, tracker] => Some(RepoRef::Fork {
            project,
            instance: (*instance).to_owned(),
            tracker: (*tracker).to_owned(),
        }),
        _ => None,
    }
}

/// A repo name given as itself or as a remote URL ending in it.
pub fn repo_from_url_or_name(value: &str) -> &str {
    if !value.contains('/') {
        return value;
    }
    let last = value.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last)
}

/// The namespace a tracker's history is, in its audience's store.
pub fn tracker_namespace(tracker: &str) -> String {
    format!("t/{tracker}")
}

/// The namespace one user's content of a per-user tracker is, in that
/// user's store. Not `t/`: a tracker later defined for `user:<login>` would
/// otherwise land on top of it.
pub fn user_namespace(tracker: &str) -> String {
    format!("u/{tracker}")
}

/// The namespace an instance's fork of a tracker is: inside the tracker's
/// own, so it moves with it, and for a private instance in that instance's
/// store instead.
pub fn fork_namespace(tracker: &str, instance: &str) -> String {
    format!("t/{tracker}/i/{instance}")
}

/// Where `namespace`'s refs are stored, as Git lays them out.
pub fn namespace_prefix(namespace: &str) -> String {
    namespace
        .split('/')
        .map(|part| format!("refs/namespaces/{part}/"))
        .collect()
}

/// Whether `value` may be one part of a repo name: letters, digits, `_` or
/// `-`, starting with a letter or digit, at most 63 characters. Every path
/// on disk is joined only from parts that pass this.
pub fn is_part(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

pub fn part_problem(what: &str, value: &str) -> Option<String> {
    (!is_part(value)).then(|| {
        format!(
            "{what} `{value}` cannot name a repo on the remote: use letters, digits, `_` or `-` (no `.`), starting with a letter or digit, at most 63 characters"
        )
    })
}

/// The tracker a tracker id is mounted in: `t3code__repos` in `t3code`,
/// `t3code` in `root`; none for `root`.
pub fn parent_of(id: &str) -> Option<&str> {
    if id == "root" {
        return None;
    }
    Some(id.rfind("__").map_or("root", |at| &id[..at]))
}

/// `id` and every tracker above it, nearest first, ending at `root`.
pub fn lineage(id: &str) -> Vec<&str> {
    let mut chain = vec![id];
    let mut at = id;
    while let Some(up) = parent_of(at) {
        chain.push(up);
        at = up;
    }
    chain
}

/// Whether `id` is `anchor` or mounted, however deep, inside it.
pub fn within(id: &str, anchor: &str) -> bool {
    lineage(id).contains(&anchor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_round_trip() {
        let project = parse_project_key("Jack/newgit").expect("a key");
        assert_eq!(project.key(), "jack/newgit");
        for name in [
            "jack.newgit.t.root",
            "jack.newgit.t.env.ana",
            "jack.newgit.i.fix-1.root",
        ] {
            assert_eq!(parse_repo_name(name).expect(name).name(), name);
        }
        assert_eq!(parse_repo_name("jack.newgit.x.root"), None);
        assert_eq!(parse_repo_name("jack.newgit.t.../etc"), None);
        assert_eq!(
            repo_from_url_or_name("http://h/_git/jack.newgit.t.root.git"),
            "jack.newgit.t.root"
        );
    }

    #[test]
    fn repos_are_nested_namespaces() {
        assert_eq!(
            namespace_prefix(&fork_namespace("root", "fix")),
            "refs/namespaces/t/refs/namespaces/root/refs/namespaces/i/refs/namespaces/fix/"
        );
        assert!(
            namespace_prefix(&fork_namespace("root", "fix"))
                .starts_with(&namespace_prefix(&tracker_namespace("root")))
        );
        assert_ne!(user_namespace("env"), tracker_namespace("env"));
    }

    #[test]
    fn lineage_follows_ids() {
        assert_eq!(lineage("a__b"), vec!["a__b", "a", "root"]);
        assert!(within("a__b", "a"));
        assert!(!within("a", "a__b"));
    }
}
