//! Who may read and write what: each tracker's committed definition, read
//! from the tracker that holds its file at that tracker's `main`, and the
//! rules over it. A port of the Worker's `names.ts` `decide` and
//! `index.ts` `loadDefinitions`, unchanged in meaning
//! (docs/design/trackers.md, *On the remote*).

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};

use rusqlite::Connection;

use crate::names::{ProjectId, RepoRef};
use crate::repos;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    Public,
    ProjectDevs,
    /// Each user has their own content: one repo per user.
    User,
    /// Exactly one named user.
    Named(String),
}

pub fn parse_audience(value: &str) -> Option<Audience> {
    match value {
        "public" => Some(Audience::Public),
        "project-devs" => Some(Audience::ProjectDevs),
        "user" => Some(Audience::User),
        _ => value
            .strip_prefix("user:")
            .filter(|login| !login.is_empty())
            .map(|login| Audience::Named(login.to_lowercase())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Maintainers {
    Owner,
    ProjectDevs,
    Named(String),
}

pub fn parse_maintainers(value: &str) -> Option<Maintainers> {
    match value {
        "owner" => Some(Maintainers::Owner),
        "project-devs" | "public" | "user" => Some(Maintainers::ProjectDevs),
        _ => value
            .strip_prefix("user:")
            .filter(|login| !login.is_empty())
            .map(|login| Maintainers::Named(login.to_lowercase())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub audience: Audience,
    pub git: bool,
    pub maintainers: Maintainers,
    /// Where its root sits in the view: `.` unless the definition says.
    pub mount: String,
    /// Its claims as written, relative to `mount`; `!` excludes.
    pub paths: Vec<String>,
    /// The tracker whose history holds this definition's file.
    pub source: String,
    pub pinned: bool,
    /// Another project is its home: `<owner>/<project>[/<path>]`.
    pub link: Option<String>,
}

/// Where `access` is mounted: every definition is `<name>.toml` at its root.
pub const ACCESS_MOUNT: &str = ".newgit/trackers";
const ID_SEPARATOR: &str = "__";

/// Trackers that always exist, file or not.
pub fn implicit_facts(tracker: &str) -> Option<Facts> {
    let base = |maintainers, mount: &str| Facts {
        audience: Audience::ProjectDevs,
        git: true,
        maintainers,
        mount: mount.to_owned(),
        paths: vec![".".to_owned()],
        source: "access".to_owned(),
        pinned: false,
        link: None,
    };
    match tracker {
        "root" => Some(base(Maintainers::ProjectDevs, ".")),
        "access" => Some(base(Maintainers::Owner, ACCESS_MOUNT)),
        _ => None,
    }
}

pub fn parse_facts(text: &str, source: &str) -> Option<Facts> {
    let table: toml::Table = text.parse().ok()?;
    let string = |key: &str| table.get(key).and_then(toml::Value::as_str);
    let audience_text = string("audience")?;
    let audience = parse_audience(audience_text)?;
    let maintainers = parse_maintainers(string("maintainers").unwrap_or(audience_text))?;
    let paths = table
        .get("paths")
        .and_then(toml::Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(Facts {
        audience,
        git: table.get("git").and_then(toml::Value::as_bool) == Some(true),
        maintainers,
        mount: normalize_path(string("mount").unwrap_or(".")),
        paths,
        source: source.to_owned(),
        pinned: table.get("pinned").and_then(toml::Value::as_bool) == Some(true),
        link: string("link").map(str::to_owned),
    })
}

/// `./a/b/` → `a/b`; the empty path is `.`.
pub fn normalize_path(path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn holds(prefix: &str, path: &str) -> bool {
    prefix == "." || path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn relative_to(mount: &str, path: &str) -> String {
    if mount == "." {
        path.to_owned()
    } else if path == mount {
        ".".to_owned()
    } else {
        path[mount.len() + 1..].to_owned()
    }
}

pub fn place_claim(mount: &str, claim: &str) -> String {
    let relative = normalize_path(claim);
    if mount == "." {
        relative
    } else if relative == "." {
        mount.to_owned()
    } else {
        format!("{mount}/{relative}")
    }
}

fn claim_on(facts: &Facts, path: &str) -> Option<usize> {
    let mut best = None;
    for written in &facts.paths {
        let excluded = written.starts_with('!');
        let claim = place_claim(&facts.mount, written.trim_start_matches('!'));
        if !holds(&claim, path) {
            continue;
        }
        if excluded {
            return None;
        }
        let length = if claim == "." { 0 } else { claim.len() };
        if best.is_none_or(|known| length > known) {
            best = Some(length);
        }
    }
    best
}

fn depth(mount: &str) -> usize {
    if mount == "." {
        0
    } else {
        mount.split('/').count()
    }
}

/// The tracker in Git owning workspace `path`, by the ownership rules: the
/// deepest mount holding it, then the longest claim, ties to the lesser name.
fn owner_of(facts: &BTreeMap<String, Facts>, path: &str) -> Option<String> {
    let in_git: Vec<(&String, &Facts)> = facts
        .iter()
        .filter(|(_, candidate)| candidate.git && holds(&candidate.mount, path))
        .collect();
    let deepest = in_git.iter().map(|(_, f)| depth(&f.mount)).max()?;
    let mut owner: Option<(&String, usize)> = None;
    for (name, candidate) in in_git {
        if depth(&candidate.mount) != deepest {
            continue;
        }
        let Some(claim) = claim_on(candidate, path) else {
            continue;
        };
        if owner.is_none_or(|(known, best)| claim > best || (claim == best && name < known)) {
            owner = Some((name, claim));
        }
    }
    owner.map(|(name, _)| name.clone())
}

/// Which tracker holds each definition file next. Governance only delegates
/// downward, and every chain of holders ends at `access` (see the Worker's
/// `nextHolders`, which this follows step for step).
fn next_holders(
    facts: &BTreeMap<String, Facts>,
    holder: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut next = holder.clone();
    for name in holder.keys() {
        if name == "access" {
            next.insert(name.clone(), "access".to_owned());
            continue;
        }
        let current = &holder[name];
        let above = |next: &BTreeMap<String, String>, start: &str, target: &str| {
            let mut at = Some(start.to_owned());
            for _ in 0..=next.len() {
                match at {
                    Some(ref here) if here == target => return true,
                    Some(ref here) if here == "access" => return false,
                    Some(ref here) => at = next.get(here).cloned(),
                    None => return false,
                }
            }
            true
        };
        let mut eligible = BTreeMap::new();
        for (candidate, candidate_facts) in facts {
            if candidate == name {
                continue;
            }
            let delegated = next.get(candidate).map_or("access", String::as_str) == current;
            let keeps = candidate == current;
            if !delegated && !keeps {
                continue;
            }
            if above(&next, candidate, name) {
                continue;
            }
            eligible.insert(candidate.clone(), candidate_facts.clone());
        }
        let owner = owner_of(&eligible, &format!("{ACCESS_MOUNT}/{name}.toml"))
            .unwrap_or_else(|| "access".to_owned());
        next.insert(name.clone(), owner);
    }
    next
}

/// Reads files at a tracker's `main`, wherever its store is.
pub struct Reader<'a> {
    pub data: &'a Utf8Path,
    pub db: &'a Connection,
    /// Trackers read at a commit, from a repository, rather than at `main`:
    /// what a landing's definitions will be before it lands.
    pub at: &'a BTreeMap<String, (Utf8PathBuf, String)>,
}

impl Reader<'_> {
    fn read(&self, project: &ProjectId, tracker: &str, path: &str) -> Option<String> {
        if let Some((dir, commit)) = self.at.get(tracker) {
            return repos::read_blob(dir, commit, path);
        }
        repos::tracker(self.data, self.db, project, tracker)
            .ok()?
            .read_file("main", path)
    }
}

/// Every named tracker's definition on `project`'s trackers' `main`s, with
/// `root` and `access` always. A link's policy is its home's; only where it is
/// mounted is this project's.
pub fn load_definitions<'n>(
    data: &Utf8Path,
    db: &Connection,
    project: &ProjectId,
    names: impl IntoIterator<Item = &'n str>,
) -> BTreeMap<String, Facts> {
    load_definitions_at(data, db, project, names, &BTreeMap::new())
}

/// [`load_definitions`], with each tracker in `at` read at its commit, from
/// its repository, instead of at its `main`.
pub fn load_definitions_at<'n>(
    data: &Utf8Path,
    db: &Connection,
    project: &ProjectId,
    names: impl IntoIterator<Item = &'n str>,
    at: &BTreeMap<String, (Utf8PathBuf, String)>,
) -> BTreeMap<String, Facts> {
    let reader = Reader { data, db, at };
    let mut all: BTreeSet<String> = ["root".to_owned(), "access".to_owned()].into();
    all.extend(names.into_iter().map(str::to_owned));
    let mut facts = read_definitions(&reader, project, &all);
    let linked: Vec<(String, Facts)> = facts
        .iter()
        .filter(|(_, fact)| fact.link.is_some())
        .map(|(name, fact)| (name.clone(), fact.clone()))
        .collect();
    for (name, fact) in linked {
        let Some((home, tracker)) = home_of(&fact) else {
            continue;
        };
        if let Some(there) = load_definitions(data, db, &home, [tracker.as_str()]).remove(&tracker)
        {
            facts.insert(
                name,
                Facts {
                    mount: fact.mount,
                    source: fact.source,
                    link: fact.link,
                    ..there
                },
            );
        }
    }
    facts
}

fn read_definitions(
    reader: &Reader<'_>,
    project: &ProjectId,
    all: &BTreeSet<String>,
) -> BTreeMap<String, Facts> {
    let file = |name: &str| format!("{ACCESS_MOUNT}/{name}.toml");
    let mut source: BTreeMap<String, (String, String)> = all
        .iter()
        .map(|name| (name.clone(), ("access".to_owned(), ACCESS_MOUNT.to_owned())))
        .collect();
    let mut facts = BTreeMap::new();
    let mut written = BTreeSet::new();
    for _round in 0..4 {
        facts = BTreeMap::new();
        for name in all {
            let (tracker, mount) = &source[name];
            let path = relative_to(mount, &file(name));
            let mut text = if path == "." {
                None
            } else {
                reader.read(project, tracker, &path)
            };
            // The layout before mounts kept `access`'s files at their
            // workspace paths.
            if text.is_none() && tracker == "access" {
                text = reader.read(project, "access", &file(name));
            }
            if text.is_some() {
                written.insert(name.clone());
            }
            let read = match &text {
                None => implicit_facts(name),
                Some(text) => parse_facts(text, tracker),
            };
            if let Some(read) = read {
                facts.insert(
                    name.clone(),
                    Facts {
                        source: tracker.clone(),
                        ..read
                    },
                );
            }
        }
        let holders = next_holders(
            &facts,
            &source
                .iter()
                .map(|(name, (tracker, _))| (name.clone(), tracker.clone()))
                .collect(),
        );
        let mut moved = false;
        for name in all {
            let owner = holders
                .get(name)
                .cloned()
                .unwrap_or_else(|| "access".to_owned());
            let mount = facts
                .get(&owner)
                .map_or_else(|| ACCESS_MOUNT.to_owned(), |f| f.mount.clone());
            if source[name] != (owner.clone(), mount.clone()) {
                source.insert(name.clone(), (owner, mount));
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    add_nested(reader, project, &mut facts, all, &written);
    facts
}

/// A tracker named `t3code__repos` is defined in `t3code`'s own tree, mounted
/// relative to it.
fn add_nested(
    reader: &Reader<'_>,
    project: &ProjectId,
    facts: &mut BTreeMap<String, Facts>,
    all: &BTreeSet<String>,
    written: &BTreeSet<String>,
) {
    let mut nested: Vec<&String> = all
        .iter()
        .filter(|name| !written.contains(*name) && name.contains(ID_SEPARATOR))
        .collect();
    nested.sort_by_key(|name| name.matches(ID_SEPARATOR).count());
    for name in nested {
        let Some(at) = name.rfind(ID_SEPARATOR) else {
            continue;
        };
        let (parent, stem) = (&name[..at], &name[at + ID_SEPARATOR.len()..]);
        let Some(holder) = facts.get(parent).cloned() else {
            continue;
        };
        if !holder.git || holder.mount == "." || holder.mount == ACCESS_MOUNT {
            continue;
        }
        let text = reader.read(project, parent, &format!("{ACCESS_MOUNT}/{stem}.toml"));
        if let Some(read) = text.and_then(|text| parse_facts(&text, parent)) {
            let mount = place_claim(&holder.mount, &read.mount);
            facts.insert(name.clone(), Facts { mount, ..read });
        }
    }
}

/// A linked tracker's home: its project, and its id there.
pub fn home_of(facts: &Facts) -> Option<(ProjectId, String)> {
    let link = facts.link.as_deref()?;
    let mut parts = link.split('/');
    let project = crate::names::parse_project_key(&format!("{}/{}", parts.next()?, parts.next()?))?;
    let path: Vec<&str> = parts.collect();
    let tracker = if path.is_empty() {
        "root".to_owned()
    } else {
        path.join(ID_SEPARATOR)
    };
    Some((project, tracker))
}

#[derive(Debug, Clone)]
pub struct Requester {
    pub login: String,
    pub member: bool,
}

/// What `decide` needs to know besides the tracker's definition.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// For a fork: its instance's owner, or `None` when there is no such instance.
    pub instance_owner: Option<String>,
    pub instance_private: bool,
}

pub type Decision = Result<(), String>;

fn may_read(audience: &Audience, who: &Requester, project: &str) -> Decision {
    match audience {
        Audience::Public => Ok(()),
        Audience::ProjectDevs | Audience::User if who.member => Ok(()),
        Audience::Named(login) if who.member => {
            if *login == who.login {
                Ok(())
            } else {
                Err(format!("this is for user:{login} only"))
            }
        }
        _ => Err(format!("{} is not a member of {project}", who.login)),
    }
}

pub fn may_maintain(
    maintainers: &Maintainers,
    who: &Requester,
    owner: &str,
    tracker: &str,
) -> Decision {
    if !who.member {
        return Err(format!("{} is not a member", who.login));
    }
    match maintainers {
        Maintainers::ProjectDevs => Ok(()),
        Maintainers::Owner if who.login == owner => Ok(()),
        Maintainers::Owner => Err(format!(
            "only {owner}, the project's owner, merges `{tracker}`"
        )),
        Maintainers::Named(login) if *login == who.login => Ok(()),
        Maintainers::Named(login) => Err(format!("only {login} maintains `{tracker}`")),
    }
}

/// May `who` read (or write) `repo`? The table in docs/design/trackers.md
/// (*On the remote*), as code.
pub fn decide(
    repo: &RepoRef,
    write: bool,
    who: &Requester,
    tracker: Option<&Facts>,
    context: &Context,
) -> Decision {
    let name = repo.name();
    let project = repo.project().key();
    let Some(tracker) = tracker else {
        return Err(format!(
            "tracker `{}` is not defined on {project}'s default branch",
            repo.tracker_name()
        ));
    };
    match repo {
        RepoRef::Fork {
            instance,
            tracker: tracker_name,
            ..
        } => {
            if !tracker.git {
                return Err(format!(
                    "`{tracker_name}` is not in Git; instances do not fork it"
                ));
            }
            let owner = match &context.instance_owner {
                Some(owner) if !(context.instance_private && *owner != who.login) => owner,
                _ => return Err(format!("no instance `{instance}` in {project}")),
            };
            may_read(&tracker.audience, who, &project)?;
            if write && *owner != who.login {
                return Err(format!(
                    "instance `{instance}` belongs to {owner}; only its owner pushes to {name}"
                ));
            }
            Ok(())
        }
        RepoRef::Tracker {
            tracker: tracker_name,
            login,
            ..
        } => {
            if tracker.audience == Audience::User {
                let Some(login) = login else {
                    return Err(format!(
                        "tracker `{tracker_name}` is per-user; its content lives in {name}.<login>"
                    ));
                };
                if !who.member {
                    return Err(format!("{} is not a member of {project}", who.login));
                }
                if *login != who.login {
                    return Err(format!(
                        "{name} is {login}'s own `{tracker_name}`; each user reads only their own"
                    ));
                }
                return Ok(());
            }
            if login.is_some() {
                return Err(format!(
                    "tracker `{tracker_name}` is not per-user, so it has no per-user repos"
                ));
            }
            may_read(&tracker.audience, who, &project)?;
            if !write {
                return Ok(());
            }
            if tracker.git {
                return Err(format!(
                    "nobody pushes to {name}: the history of a tracker in Git moves only through `newgit merge`; an instance pushes to its fork"
                ));
            }
            if tracker.audience == Audience::Public && !who.member {
                return Err(format!("{} is not a member of {project}", who.login));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(text: &str) -> Facts {
        parse_facts(text, "access").expect("parses")
    }

    #[test]
    fn audiences_decide_reads_and_writes() {
        let project = crate::names::parse_project_key("jack/p").expect("key");
        let member = Requester {
            login: "ana".into(),
            member: true,
        };
        let stranger = Requester {
            login: "eve".into(),
            member: false,
        };
        let root = implicit_facts("root").expect("root");
        let repo = RepoRef::tracker(&project, "root");
        let none = Context::default();
        assert!(decide(&repo, false, &member, Some(&root), &none).is_ok());
        assert!(decide(&repo, false, &stranger, Some(&root), &none).is_err());
        assert!(decide(&repo, true, &member, Some(&root), &none).is_err());

        let public = facts("audience = \"public\"\n");
        assert!(
            decide(
                &RepoRef::tracker(&project, "site"),
                false,
                &stranger,
                Some(&public),
                &none
            )
            .is_ok()
        );

        let fork = RepoRef::Fork {
            project: project.clone(),
            instance: "fix".into(),
            tracker: "root".into(),
        };
        let mine = Context {
            instance_owner: Some("ana".into()),
            instance_private: false,
        };
        assert!(decide(&fork, true, &member, Some(&root), &mine).is_ok());
        let theirs = Context {
            instance_owner: Some("jack".into()),
            instance_private: true,
        };
        assert!(decide(&fork, false, &member, Some(&root), &theirs).is_err());
    }

    #[test]
    fn definitions_default_their_maintainers_to_their_audience() {
        let facts = facts("audience = \"user:Ana\"\ngit = true\nmount = \"./apps/web/\"\n");
        assert_eq!(facts.maintainers, Maintainers::Named("ana".into()));
        assert_eq!(facts.mount, "apps/web");
        assert!(facts.git);
    }
}
