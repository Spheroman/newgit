//! Stores on local disk, and the repos in them, worked with real Git.
//!
//! A **store** is one bare repository with refs in reftable, made for exactly
//! one audience ([`Store`]): everyone who can reach any of it may read all of
//! it. Under protocol v2 any object in a repository can be fetched by id, a
//! `have` line answers whether a hidden commit exists, and a crafted push can
//! point a ref at any object in it, so a store is never a boundary within
//! itself, and nothing here tries to make it one.
//!
//! Each **repo** the API names (a tracker, one user's content of a per-user
//! tracker, an instance's fork) is a Git namespace in a store ([`Repo`]),
//! served by `git --namespace`. Everything here is a short, local Git
//! command: what the Worker read from Artifacts over its binding (a file at a
//! branch, a branch's head, a commit's pins) is `git cat-file` here.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{ApiResult, HttpError};
use crate::names::{is_part, namespace_prefix};
use crate::policy::{Audience, Facts};

/// Config every store gets: check what is pushed, never prune what a fork's
/// namespace or a landing might still name, and let `newgit clone` filter.
const STORE_CONFIG: &[(&str, &str)] = &[
    ("receive.fsckObjects", "true"),
    ("uploadpack.allowFilter", "true"),
    ("core.logAllRefUpdates", "always"),
];

/// The config key a store's audience is written under when it is made.
const AUDIENCE_KEY: &str = "newgit.audience";

/// One store in a project's home: whose objects it holds. Its path under
/// `homes/<owner>/<project>/stores/` is its name, and its name is written in
/// its config when it is made.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Store {
    /// Trackers anyone may read.
    Public,
    /// Trackers the project's members may read: `root` and `access` by default.
    ProjectDevs,
    /// Exactly one user's: trackers for `user:<login>`, and that user's own
    /// content of every per-user tracker.
    User(String),
    /// One tracker alone, before its audience is settled: a new tracker's
    /// seed, until it joins a branch.
    Tracker(String),
    /// A private instance's forks: its shape is the secret, so nobody else's
    /// objects are beside it.
    Instance(String),
}

impl Store {
    /// Where a tracker's history belongs, by its definition. A tracker not
    /// defined yet, or defined as per-user (whose content is each user's own),
    /// stays alone.
    pub fn for_tracker(facts: Option<&Facts>, tracker: &str) -> Self {
        match facts.map(|facts| &facts.audience) {
            Some(Audience::Public) => Self::Public,
            Some(Audience::ProjectDevs) => Self::ProjectDevs,
            // A definition is text anyone governing it wrote: a login that
            // could not name a path gets no shared store.
            Some(Audience::Named(login)) if is_part(login) => Self::User(login.clone()),
            _ => Self::Tracker(tracker.to_owned()),
        }
    }

    /// `public`, `project-devs`, `user/<login>`, `tracker/<tracker>`,
    /// `instance/<instance>`.
    pub fn name(&self) -> String {
        match self {
            Self::Public => "public".to_owned(),
            Self::ProjectDevs => "project-devs".to_owned(),
            Self::User(login) => format!("user/{login}"),
            Self::Tracker(tracker) => format!("tracker/{tracker}"),
            Self::Instance(instance) => format!("instance/{instance}"),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let store = match name.split_once('/') {
            None if name == "public" => Self::Public,
            None if name == "project-devs" => Self::ProjectDevs,
            Some(("user", login)) => Self::User(login.to_owned()),
            Some(("tracker", tracker)) => Self::Tracker(tracker.to_owned()),
            Some(("instance", instance)) => Self::Instance(instance.to_owned()),
            _ => return None,
        };
        match &store {
            Self::User(part) | Self::Tracker(part) | Self::Instance(part) if !is_part(part) => None,
            _ => Some(store),
        }
    }

    /// The store's directory in the home at `home`.
    pub fn dir(&self, home: &Utf8Path) -> Utf8PathBuf {
        home.join("stores").join(format!("{}.git", self.name()))
    }
}

pub fn git(dir: &Utf8Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
}

fn checked(dir: &Utf8Path, args: &[&str]) -> ApiResult<String> {
    let output = git(dir, args)?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "git {args:?} in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_stdin(dir: &Utf8Path, args: &[&str], input: &str) -> ApiResult<bool> {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    Ok(output.status.success())
}

pub fn store_exists(dir: &Utf8Path) -> bool {
    dir.join("HEAD").is_file()
}

/// The audience store `dir` was made for.
pub fn audience_of(dir: &Utf8Path) -> Option<String> {
    let output = git(dir, &["config", "--get", AUDIENCE_KEY]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Make the store `store` at `dir`, refs in reftable. One that exists already
/// must have been made for the same audience: a store never takes a second.
pub fn create_store(dir: &Utf8Path, store: &Store) -> ApiResult<()> {
    let name = store.name();
    if store_exists(dir) {
        return match audience_of(dir) {
            Some(made_for) if made_for == name => Ok(()),
            made_for => Err(HttpError::internal(format!(
                "{dir} was made for {}, so it never holds {name}'s",
                made_for.as_deref().unwrap_or("no audience")
            ))),
        };
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = Command::new("git")
        .args([
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--ref-format=reftable",
        ])
        .arg(dir.as_str())
        .output()?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "git init {dir}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    for (key, value) in STORE_CONFIG {
        checked(dir, &["config", key, value])?;
    }
    checked(dir, &["config", AUDIENCE_KEY, &name])?;
    Ok(())
}

pub fn delete(dir: &Utf8Path) -> ApiResult<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

/// One ref update for [`Repo::update_refs`]: `old` none means "must not exist".
pub struct RefUpdate<'a> {
    pub reference: &'a str,
    pub new: &'a str,
    pub old: Option<&'a str>,
}

/// One repo: a Git namespace in a store. Every ref named to it is as its
/// readers see it (`refs/heads/main`), and lives in the store under the
/// namespace's prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The project's home, `homes/<owner>/<project>`.
    pub home: Utf8PathBuf,
    pub store: Store,
    /// As `git --namespace` takes it: `t/<tracker>`, `t/<tracker>/i/<instance>`.
    pub namespace: String,
}

impl Repo {
    pub fn new(home: &Utf8Path, store: Store, namespace: String) -> Self {
        Self {
            home: home.to_owned(),
            store,
            namespace,
        }
    }

    /// The store's directory.
    pub fn dir(&self) -> Utf8PathBuf {
        self.store.dir(&self.home)
    }

    /// Where this repo's refs are stored.
    pub fn prefix(&self) -> String {
        namespace_prefix(&self.namespace)
    }

    fn full(&self, reference: &str) -> String {
        format!("{}{reference}", self.prefix())
    }

    /// A tracker's repo exists once it has a `HEAD`, made with it.
    pub fn exists(&self) -> bool {
        store_exists(&self.dir())
            && git(
                &self.dir(),
                &["symbolic-ref", "--quiet", &self.full("HEAD")],
            )
            .is_ok_and(|output| output.status.success())
    }

    /// Make this repo, and its store if that is the first. Making one that
    /// exists is a no-op.
    pub fn create(&self) -> ApiResult<()> {
        create_store(&self.dir(), &self.store)?;
        checked(
            &self.dir(),
            &[
                "symbolic-ref",
                &self.full("HEAD"),
                &self.full("refs/heads/main"),
            ],
        )?;
        Ok(())
    }

    /// A file's text on `branch`, or `None` when there is no such branch or path.
    pub fn read_file(&self, branch: &str, path: &str) -> Option<String> {
        if !store_exists(&self.dir()) {
            return None;
        }
        let at = format!("{}:{path}", self.full(&format!("refs/heads/{branch}")));
        let output = git(&self.dir(), &["cat-file", "blob", &at]).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The commit `refs/heads/<branch>` names.
    pub fn branch_head(&self, branch: &str) -> Option<String> {
        if !store_exists(&self.dir()) {
            return None;
        }
        let reference = format!("{}^{{commit}}", self.full(&format!("refs/heads/{branch}")));
        let output = git(
            &self.dir(),
            &["rev-parse", "--verify", "--quiet", &reference],
        )
        .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Whether the store holds `commit`. The store, not the namespace: what
    /// its audience may read, which is all that matters here.
    pub fn has_commit(&self, commit: &str) -> bool {
        store_exists(&self.dir())
            && git(
                &self.dir(),
                &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            )
            .is_ok_and(|output| output.status.success())
    }

    /// Whether `ancestor` is `descendant` or behind it.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        git(
            &self.dir(),
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
        .is_ok_and(|output| output.status.success())
    }

    /// Move refs all together or not at all (`git update-ref --stdin`, in one
    /// transaction). Returns whether they moved; a ref not at its `old` moves
    /// none.
    pub fn update_refs(&self, updates: &[RefUpdate<'_>]) -> ApiResult<bool> {
        let mut script = String::from("start\n");
        for update in updates {
            let old = update
                .old
                .unwrap_or("0000000000000000000000000000000000000000");
            script.push_str(&format!(
                "update {} {} {old}\n",
                self.full(update.reference),
                update.new
            ));
        }
        script.push_str("prepare\ncommit\n");
        run_stdin(&self.dir(), &["update-ref", "--stdin"], &script)
    }

    /// Point every ref in `refs` at its commit, whatever was there.
    pub fn set_refs(&self, refs: &[(String, String)]) -> ApiResult<()> {
        if refs.is_empty() {
            return Ok(());
        }
        let script: String = refs
            .iter()
            .map(|(reference, commit)| format!("update {} {commit}\n", self.full(reference)))
            .collect();
        if run_stdin(&self.dir(), &["update-ref", "--stdin"], &script)? {
            Ok(())
        } else {
            Err(HttpError::internal(format!(
                "could not set refs in {} ({})",
                self.dir(),
                self.namespace
            )))
        }
    }

    pub fn delete_ref(&self, reference: &str) -> ApiResult<()> {
        git(&self.dir(), &["update-ref", "-d", &self.full(reference)])?;
        Ok(())
    }

    /// Delete every ref under `prefix` (`refs/` for all of them, forks'
    /// namespaces inside this one included).
    pub fn delete_refs_under(&self, prefix: &str) -> ApiResult<usize> {
        if !store_exists(&self.dir()) {
            return Ok(0);
        }
        let refs = list_refs(&self.dir(), &self.full(prefix))?;
        let script: String = refs
            .iter()
            .map(|(reference, _)| format!("delete {reference}\n"))
            .collect();
        if !refs.is_empty() && !run_stdin(&self.dir(), &["update-ref", "--stdin"], &script)? {
            return Err(HttpError::internal(format!(
                "could not delete {prefix} in {} ({})",
                self.dir(),
                self.namespace
            )));
        }
        Ok(refs.len())
    }

    /// Every ref under `prefix`, as this repo's readers name it.
    pub fn list_refs(&self, prefix: &str) -> ApiResult<Vec<(String, String)>> {
        let own = self.prefix();
        Ok(list_refs(&self.dir(), &self.full(prefix))?
            .into_iter()
            .filter_map(|(reference, object)| {
                Some((reference.strip_prefix(&own)?.to_owned(), object))
            })
            .collect())
    }

    /// The repo gone: its refs (and any fork's inside it), and its `HEAD`.
    pub fn remove(&self) -> ApiResult<()> {
        self.delete_refs_under("refs/")?;
        git(
            &self.dir(),
            &["symbolic-ref", "--delete", "--quiet", &self.full("HEAD")],
        )?;
        Ok(())
    }

    /// A commit's pins: `.newgit/pins/<name>` gitlinks, by child name.
    pub fn pins_of(&self, commit: &str) -> Vec<(String, String)> {
        let Ok(output) = git(&self.dir(), &["ls-tree", &format!("{commit}:.newgit/pins")]) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (meta, name) = line.split_once('\t')?;
                let mut fields = meta.split_whitespace();
                (fields.next()? == "160000").then_some(())?;
                fields.next()?;
                Some((name.to_owned(), fields.next()?.to_owned()))
            })
            .collect()
    }

    /// Bring this repo's refs into `to`, a store of `to`'s own, under the same
    /// namespace, then remove them here. A store of one tracker alone moving
    /// to an audience no store holds yet is that store, renamed.
    pub fn move_to(&self, to: &Store) -> ApiResult<Self> {
        let moved = Self::new(&self.home, to.clone(), self.namespace.clone());
        if matches!(self.store, Store::Tracker(_)) && !store_exists(&moved.dir()) {
            if let Some(parent) = moved.dir().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(self.dir(), moved.dir())?;
            checked(&moved.dir(), &["config", AUDIENCE_KEY, &to.name()])?;
        } else {
            moved.copy_from(&self.dir(), &self.prefix())?;
            if matches!(self.store, Store::Tracker(_)) {
                delete(&self.dir())?;
            } else {
                self.remove()?;
            }
        }
        // `stores/tracker/`, once the last seed in it is gone.
        if let Some(parent) = self.dir().parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(moved)
    }

    /// Make this repo, with every ref under `from_prefix` in the repository at
    /// `from` as its own: a namespace's prefix, or empty for a repository's
    /// own refs.
    pub fn copy_from(&self, from: &Utf8Path, from_prefix: &str) -> ApiResult<()> {
        self.create()?;
        let refspec = format!("+{from_prefix}refs/*:{}refs/*", self.prefix());
        let output = git(
            &self.dir(),
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                from.as_str(),
                &refspec,
            ],
        )?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "copy {from} into {} ({}): {}",
                self.dir(),
                self.namespace,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// `project`'s own tracker `tracker`: in the store its row names, or alone in
/// a store of its own when it has none yet (where a new one is made).
pub fn tracker(
    data: &Utf8Path,
    db: &rusqlite::Connection,
    project: &crate::names::ProjectId,
    tracker: &str,
) -> ApiResult<Repo> {
    let store = crate::db::tracker_store(db, &project.key(), tracker)?
        .and_then(|name| Store::parse(&name))
        .unwrap_or_else(|| Store::Tracker(tracker.to_owned()));
    Ok(Repo::new(
        &project.dir(data),
        store,
        crate::names::tracker_namespace(tracker),
    ))
}

/// Every ref under `prefix` in the store at `dir`, with its object.
fn list_refs(dir: &Utf8Path, prefix: &str) -> ApiResult<Vec<(String, String)>> {
    let listed = checked(
        dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    )?;
    Ok(listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(reference, object)| (reference.to_owned(), object.to_owned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_names_round_trip() {
        for store in [
            Store::Public,
            Store::ProjectDevs,
            Store::User("ana".into()),
            Store::Tracker("t3code__repos".into()),
            Store::Instance("fix-1".into()),
        ] {
            assert_eq!(Store::parse(&store.name()), Some(store));
        }
        assert_eq!(Store::parse("user/../etc"), None);
        assert_eq!(Store::parse("private"), None);
        assert_eq!(
            Store::Instance("fix".into()).dir(Utf8Path::new("/h")),
            "/h/stores/instance/fix.git"
        );
    }

    #[test]
    fn a_tracker_goes_to_its_audiences_store() {
        let with = |audience| Facts {
            audience,
            ..crate::policy::implicit_facts("root").expect("root")
        };
        assert_eq!(
            Store::for_tracker(Some(&with(Audience::Public)), "site"),
            Store::Public
        );
        assert_eq!(
            Store::for_tracker(Some(&with(Audience::Named("ana".into()))), "notes"),
            Store::User("ana".into())
        );
        assert_eq!(
            Store::for_tracker(Some(&with(Audience::Named("../x".into()))), "notes"),
            Store::Tracker("notes".into())
        );
        assert_eq!(
            Store::for_tracker(Some(&with(Audience::User)), "env"),
            Store::Tracker("env".into())
        );
        assert_eq!(
            Store::for_tracker(None, "lib"),
            Store::Tracker("lib".into())
        );
    }
}
