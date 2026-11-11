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

use std::collections::BTreeMap;
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
    // A ref a push or a landing moved survives power loss once Git says it
    // moved (`docs/design/serve.md`, *Known limits*).
    ("core.fsync", DURABLE_FSYNC),
    ("core.fsyncMethod", "fsync"),
    ("uploadpack.allowFilter", "true"),
    ("core.logAllRefUpdates", "always"),
];

/// Where a fork's repo records the tracker it was made from:
/// `refs/newgit/from/<upstream>`, in its namespace, at
/// [`Repo::record_commit`]. Written by the server alone, by the landing that
/// makes the fork's repo (`api::Authorized::made`), never by a push: Git
/// requests hide it (`transfer.hideRefs`), so no push can move or delete it.
pub const FORK_RECORD: &str = "refs/newgit/from/";

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

    /// Whether everyone who reads `other` reads this store too, so anything
    /// `other`'s readers may see may be written here. Only `public` is known
    /// to hold every other audience; any other two (`user:a` and `user:b`,
    /// `user:a` and `project-devs`, whose login need not be a member, or a
    /// private instance's and the members') are not ordered by inclusion.
    pub fn includes(&self, other: &Store) -> bool {
        self == other || *self == Self::Public
    }

    /// The store's directory in the home at `home`.
    pub fn dir(&self, home: &Utf8Path) -> Utf8PathBuf {
        home.join("stores").join(format!("{}.git", self.name()))
    }
}

/// What every store and the landing log fsync: objects and refs, so what
/// Git says it wrote survives power loss.
pub const DURABLE_FSYNC: &str = "committed,reference";

/// Git run against the repository at `dir`, syncing what it writes
/// ([`DURABLE_FSYNC`]) whatever the repository's own config says.
pub fn git_command(dir: &Utf8Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", &format!("core.fsync={DURABLE_FSYNC}")])
        .args(["-c", "core.fsyncMethod=fsync"])
        .arg("--git-dir")
        .arg(dir.as_str());
    command
}

pub fn git(dir: &Utf8Path, args: &[&str]) -> std::io::Result<Output> {
    git_command(dir)
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
    let mut child = git_command(dir)
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

/// Whether `store` may be made, or is, at `dir`: nothing there yet, or a
/// store made for that same audience.
pub fn check_store(dir: &Utf8Path, store: &Store) -> ApiResult<()> {
    let name = store.name();
    if !store_exists(dir) {
        return Ok(());
    }
    match audience_of(dir) {
        Some(made_for) if made_for == name => Ok(()),
        made_for => Err(HttpError::internal(format!(
            "{dir} was made for {}, so it never holds {name}'s",
            made_for.as_deref().unwrap_or("no audience")
        ))),
    }
}

/// Make the store `store` at `dir`, refs in reftable. One that exists already
/// must have been made for the same audience: a store never takes a second.
pub fn create_store(dir: &Utf8Path, store: &Store) -> ApiResult<()> {
    let name = store.name();
    if store_exists(dir) {
        return check_store(dir, store);
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

/// Give every store in `data`'s homes [`STORE_CONFIG`], as a store made
/// now gets it: one made before a key was added (`core.fsync`, by this
/// server's first landings) gets it when the server starts, so a push into
/// it, which runs with the store's own config, is as durable as a landing.
/// Writes only what differs; returns how many stores it changed.
pub fn configure_stores(data: &Utf8Path) -> ApiResult<usize> {
    let mut changed = 0;
    let Ok(owners) = data.join("homes").read_dir_utf8() else {
        return Ok(0);
    };
    for owner in owners.flatten() {
        let Ok(projects) = owner.path().read_dir_utf8() else {
            continue;
        };
        for project in projects.flatten() {
            let mut stack = vec![project.path().join("stores")];
            while let Some(dir) = stack.pop() {
                if store_exists(&dir) {
                    if configure_store(&dir)? {
                        changed += 1;
                    }
                    continue;
                }
                let Ok(read) = dir.read_dir_utf8() else {
                    continue;
                };
                for entry in read.flatten() {
                    if entry.path().is_dir() {
                        stack.push(entry.path().to_owned());
                    }
                }
            }
        }
    }
    Ok(changed)
}

/// [`STORE_CONFIG`] for the store at `dir`, written where it differs.
fn configure_store(dir: &Utf8Path) -> ApiResult<bool> {
    let listed = checked(dir, &["config", "--local", "--list"])?;
    let have: std::collections::BTreeSet<&str> = listed.lines().collect();
    let mut changed = false;
    for (key, value) in STORE_CONFIG {
        if !have.contains(format!("{}={value}", key.to_lowercase()).as_str()) {
            checked(dir, &["config", key, value])?;
            changed = true;
        }
    }
    Ok(changed)
}

pub fn delete(dir: &Utf8Path) -> ApiResult<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
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

    /// `reference`, as this repo's readers name it, as the store names it.
    pub fn full(&self, reference: &str) -> String {
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
        self.branch_head_at(&format!("refs/heads/{branch}"))
    }

    /// The commit `reference`, as this repo's readers name it, names.
    pub fn branch_head_at(&self, reference: &str) -> Option<String> {
        if !store_exists(&self.dir()) {
            return None;
        }
        let reference = format!("{}^{{commit}}", self.full(reference));
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
        has_commit(&self.dir(), commit)
    }

    /// Whether `ancestor` is `descendant` or behind it.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        is_ancestor(&self.dir(), ancestor, descendant)
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

    /// A commit's pins: `.newgit/pins/<name>` gitlinks, by child name.
    pub fn pins_of(&self, commit: &str) -> Vec<(String, String)> {
        pins_of(&self.dir(), commit)
    }

    /// The first half of moving this repo to `to`, run once a landing that
    /// moves it is written down: every ref under its prefix (its instances'
    /// forks among them) copied into `to`'s store under the same namespace.
    /// A store of one tracker alone, moving to an audience no store holds yet,
    /// is that store, labelled for `to` and renamed. Nothing is removed here:
    /// readers are served from where its row says until the landing is in
    /// place, and [`Self::finish_move`] removes it here after that.
    ///
    /// Run again (a landing replayed at startup), it copies what is left: a
    /// repo already gone from here has nothing to copy.
    pub fn copy_to(&self, to: &Store) -> ApiResult<Copied> {
        let moved = Self::new(&self.home, to.clone(), self.namespace.clone());
        let alone = matches!(self.store, Store::Tracker(_));
        if !store_exists(&self.dir()) {
            return Ok(Copied {
                to: moved,
                renamed: true,
                refs: Vec::new(),
            });
        }
        if alone && !store_exists(&moved.dir()) {
            checked(&self.dir(), &["config", AUDIENCE_KEY, &to.name()])?;
            if let Some(parent) = moved.dir().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(self.dir(), moved.dir())?;
            self.remove_empty_parent();
            return Ok(Copied {
                to: moved,
                renamed: true,
                refs: Vec::new(),
            });
        }
        let refs = list_refs(&self.dir(), &self.full("refs/"))?;
        let had_head = self.exists();
        if had_head || !refs.is_empty() {
            moved.copy_from(&self.dir(), &self.prefix(), "refs/")?;
        } else {
            create_store(&moved.dir(), to)?;
        }
        Ok(Copied {
            to: moved,
            renamed: false,
            refs,
        })
    }

    /// The second half of a move, once readers are served from `copied.to`:
    /// this repo's refs removed here, each at exactly the object it was
    /// copied at. A ref that moved, appeared, or went here since the copy (a
    /// push that began before readers moved, which the caller waits out
    /// first) is carried to the new store first, by compare-and-swap against
    /// what was copied there, so a push made since to the new store wins and
    /// nothing pushed is dropped.
    pub fn finish_move(&self, copied: &Copied) -> ApiResult<()> {
        if copied.renamed || !store_exists(&self.dir()) {
            return Ok(());
        }
        let now = list_refs(&self.dir(), &self.full("refs/"))?;
        let before: std::collections::BTreeMap<&str, &str> = copied
            .refs
            .iter()
            .map(|(reference, object)| (reference.as_str(), object.as_str()))
            .collect();
        let after: std::collections::BTreeMap<&str, &str> = now
            .iter()
            .map(|(reference, object)| (reference.as_str(), object.as_str()))
            .collect();
        let zero = "0000000000000000000000000000000000000000";
        for (reference, object) in &after {
            if before.get(reference) == Some(object) {
                continue;
            }
            let staging = format!("refs/newgit/moving/{object}");
            let output = git(
                &copied.to.dir(),
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    self.dir().as_str(),
                    &format!("+{reference}:{staging}"),
                ],
            )?;
            let old = before.get(reference).copied().unwrap_or(zero);
            let carried = output.status.success()
                && run_stdin(
                    &copied.to.dir(),
                    &["update-ref", "--stdin"],
                    &format!("update {reference} {object} {old}\ndelete {staging}\n"),
                )?;
            if !carried {
                eprintln!(
                    "newgit serve: {reference} moved in {} while {} moved to {}; the newer one in {} is kept",
                    self.dir(),
                    self.namespace,
                    copied.to.store.name(),
                    copied.to.dir()
                );
            }
        }
        for (reference, object) in &before {
            if !after.contains_key(reference) {
                // Removed here since the copy (an instance removed): removed
                // there too, unless something newer is there now.
                run_stdin(
                    &copied.to.dir(),
                    &["update-ref", "--stdin"],
                    &format!("delete {reference} {object}\n"),
                )?;
            }
        }
        self.remove_exactly(&now)?;
        if matches!(self.store, Store::Tracker(_)) && list_refs(&self.dir(), "refs/")?.is_empty() {
            delete(&self.dir())?;
            self.remove_empty_parent();
        }
        Ok(())
    }

    /// The second half of a move whose first half and whose rows flipping
    /// happened before a restart, so what was copied is not known: every ref
    /// still here is carried to `to` only where that moves it forward (absent
    /// there, or there at an ancestor of it here), then removed here. A ref
    /// pushed to `to` since the rows flipped is never moved back; one that
    /// differs here and is not ahead is left there as it is, and said.
    pub fn finish_move_after_restart(&self, to: &Repo) -> ApiResult<()> {
        if !store_exists(&self.dir()) || self.dir() == to.dir() {
            return Ok(());
        }
        let here = list_refs(&self.dir(), &self.full("refs/"))?;
        let there: BTreeMap<String, String> = list_refs(&to.dir(), &to.full("refs/"))?
            .into_iter()
            .collect();
        let zero = "0000000000000000000000000000000000000000";
        for (reference, object) in &here {
            let theirs = there.get(reference);
            if theirs == Some(object) {
                continue;
            }
            let staging = format!("refs/newgit/moving/{object}");
            let fetched = git(
                &to.dir(),
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    self.dir().as_str(),
                    &format!("+{reference}:{staging}"),
                ],
            )?
            .status
            .success();
            let forward = match theirs {
                None => true,
                Some(theirs) => fetched && is_ancestor(&to.dir(), theirs, object),
            };
            let carried = fetched
                && forward
                && run_stdin(
                    &to.dir(),
                    &["update-ref", "--stdin"],
                    &format!(
                        "update {reference} {object} {}\n",
                        theirs.map_or(zero, String::as_str)
                    ),
                )?;
            let _ = run_stdin(
                &to.dir(),
                &["update-ref", "--stdin"],
                &format!("delete {staging}\n"),
            );
            if !carried {
                eprintln!(
                    "newgit serve: {reference} in {} is not ahead of {} in {}; the one in {} is kept",
                    self.dir(),
                    self.namespace,
                    to.dir(),
                    to.dir()
                );
            }
        }
        self.remove_exactly(&here)?;
        if matches!(self.store, Store::Tracker(_)) && list_refs(&self.dir(), "refs/")?.is_empty() {
            delete(&self.dir())?;
            self.remove_empty_parent();
        }
        Ok(())
    }

    /// Undo [`Self::copy_to`] for a move voided before readers moved: a store
    /// renamed into place is put back, labelled for this store again; refs
    /// copied into `to` are removed there. Objects copied stay in `to`'s
    /// store, unreachable, until it is garbage collected.
    pub fn uncopy(&self, to: &Store) -> ApiResult<()> {
        let moved = Self::new(&self.home, to.clone(), self.namespace.clone());
        if moved.dir() == self.dir() || !store_exists(&moved.dir()) {
            return Ok(());
        }
        if matches!(self.store, Store::Tracker(_)) && !store_exists(&self.dir()) {
            checked(&moved.dir(), &["config", AUDIENCE_KEY, &self.store.name()])?;
            if let Some(parent) = self.dir().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(moved.dir(), self.dir())?;
            return Ok(());
        }
        moved.delete_refs_under("refs/")?;
        git(
            &moved.dir(),
            &["symbolic-ref", "--delete", "--quiet", &moved.full("HEAD")],
        )?;
        Ok(())
    }

    /// `stores/tracker/`, once the last seed in it is gone.
    fn remove_empty_parent(&self) {
        if let Some(parent) = self.dir().parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    /// Delete exactly `refs` (full names, as listed) at the objects they were
    /// listed at, all or none, then this repo's `HEAD`.
    fn remove_exactly(&self, refs: &[(String, String)]) -> ApiResult<()> {
        let mut script = String::from("start\n");
        for (reference, old) in refs {
            script.push_str(&format!("delete {reference} {old}\n"));
        }
        script.push_str("prepare\ncommit\n");
        if !refs.is_empty() && !run_stdin(&self.dir(), &["update-ref", "--stdin"], &script)? {
            return Err(HttpError::internal(format!(
                "a ref of {} in {} moved while it was removed; it stays here",
                self.namespace,
                self.dir()
            )));
        }
        git(
            &self.dir(),
            &["symbolic-ref", "--delete", "--quiet", &self.full("HEAD")],
        )?;
        Ok(())
    }

    /// Make this repo, with every ref under `<from_prefix><under>` in the
    /// repository at `from` as its own: `from_prefix` a namespace's prefix, or
    /// empty for a repository's own refs; `under` `refs/` for all of them, or
    /// `refs/heads/` for branches alone.
    pub fn copy_from(&self, from: &Utf8Path, from_prefix: &str, under: &str) -> ApiResult<()> {
        self.create()?;
        let refspec = format!("+{from_prefix}{under}*:{}{under}*", self.prefix());
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

    /// The tracker this repo was made from, when it is a fork this server
    /// started ([`FORK_RECORD`]).
    pub fn recorded_from(&self) -> Option<String> {
        if !store_exists(&self.dir()) {
            return None;
        }
        let mut found = self.list_refs(FORK_RECORD).ok()?.into_iter();
        let (reference, _) = found.next()?;
        reference.strip_prefix(FORK_RECORD).map(str::to_owned)
    }

    /// The commit this repo's record of being made from `upstream`'s points
    /// at ([`FORK_RECORD`]), written into its store: an empty tree, its
    /// message the upstream's name, its author and date fixed, so it keeps
    /// nothing else alive and writing it again is the same commit. A ref
    /// moved by a landing names a commit its store holds, so the record is
    /// one.
    pub fn record_commit(&self, upstream: &str) -> ApiResult<String> {
        let tree = checked(&self.dir(), &["hash-object", "-w", "-t", "tree", "--stdin"])?;
        let output = git_command(&self.dir())
            .args(["commit-tree", &tree, "-m", &format!("from {upstream}")])
            .env("GIT_AUTHOR_NAME", "newgit")
            .env("GIT_AUTHOR_EMAIL", "newgit@localhost")
            .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z")
            .env("GIT_COMMITTER_NAME", "newgit")
            .env("GIT_COMMITTER_EMAIL", "newgit@localhost")
            .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z")
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "recording {}'s upstream in {}: {}",
                self.namespace,
                self.dir(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Bring `commits`, and everything they reach, into this repo's store
    /// from the store at `from`, by id, with no ref: only what they reach is
    /// copied. A ref is pointed at them afterwards, by whoever says it moves.
    pub fn fetch_commits(&self, from: &Utf8Path, commits: &[String]) -> ApiResult<()> {
        let missing: Vec<&str> = commits
            .iter()
            .filter(|commit| !self.has_commit(commit))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "-c",
            "protocol.version=2",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            from.as_str(),
        ];
        args.extend(missing);
        let output = git(&self.dir(), &args)?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "copying {from} into {} ({}): {}",
                self.dir(),
                self.namespace,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Point each of `refs` (as readers name them) that does not exist yet at
    /// its commit, all in one ref transaction or none: a ref that exists
    /// already is left as it is.
    pub fn start_refs(&self, refs: &[(String, String)]) -> ApiResult<()> {
        let names: Vec<String> = refs
            .iter()
            .map(|(reference, _)| self.full(reference))
            .collect();
        let now = resolve(&self.dir(), &names)?;
        let script: String = refs
            .iter()
            .filter(|(reference, _)| !now.contains_key(&self.full(reference)))
            .map(|(reference, commit)| format!("create {} {commit}\n", self.full(reference)))
            .collect();
        if script.is_empty() {
            return Ok(());
        }
        if run_stdin(&self.dir(), &["update-ref", "--stdin"], &script)? {
            Ok(())
        } else {
            Err(HttpError::new(
                409,
                format!(
                    "{} ({}) moved while it was being started: try again",
                    self.dir(),
                    self.namespace
                ),
            ))
        }
    }

    // ── A merge's landing ─────────────────────────────────────────────────

    /// Where `login`'s merge objects for this repo wait until the merge lands:
    /// a bare repository of its own, `landing/<login>/<namespace>.git` in the
    /// home, beside the stores and never one of them. Nothing serves it to
    /// anyone; only the merger writes to it, and the server reads it.
    ///
    /// Which store a merge's commits belong in is only known once it lands
    /// (it may change the definitions that say so), so they are never written
    /// into a store before then: a landing that narrows a tracker's audience
    /// would otherwise leave its new content in the wider store, where under
    /// protocol v2 anyone reaching any repo in it can fetch it by id.
    pub fn landing_dir(&self, login: &str) -> ApiResult<Utf8PathBuf> {
        if !is_part(login) {
            return Err(HttpError::new(
                403,
                format!("`{login}` cannot name a merge's landing"),
            ));
        }
        Ok(self
            .home
            .join("landing")
            .join(login)
            .join(format!("{}.git", self.namespace)))
    }

    /// [`Self::landing_dir`], made if it is not yet, borrowing the objects of
    /// `borrow`'s store (its alternates, rewritten each time so they follow
    /// the tracker when it moves) so a thin pack resolves against them. A
    /// push into it is told of `borrow`'s own branches as what the server
    /// has, and no other ref in the store.
    pub fn open_landing(&self, login: &str) -> ApiResult<Utf8PathBuf> {
        self.open_landing_from(login, self)
    }

    fn open_landing_from(&self, login: &str, borrow: &Repo) -> ApiResult<Utf8PathBuf> {
        let dir = self.landing_dir(login)?;
        if !store_exists(&dir) {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let output = Command::new("git")
                .args(["init", "-q", "--bare", "-b", "main"])
                .arg(dir.as_str())
                .output()?;
            if !output.status.success() {
                return Err(HttpError::internal(format!(
                    "git init {dir}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            checked(&dir, &["config", "receive.fsckObjects", "true"])?;
        }
        std::fs::write(
            dir.join("objects/info/alternates"),
            format!("{}\n", borrow.dir().join("objects")),
        )?;
        checked(
            &dir,
            &[
                "config",
                "core.alternateRefsPrefixes",
                &borrow.full("refs/heads/"),
            ],
        )?;
        Ok(dir)
    }

    /// What a merge by `login` reads this repo's objects from: its landing,
    /// which sees the store's objects too, or the store when it has none.
    pub fn view(&self, login: &str) -> ApiResult<Utf8PathBuf> {
        if store_exists(&self.landing_dir(login)?) {
            self.open_landing(login)
        } else {
            Ok(self.dir())
        }
    }

    /// Whether `commit` waits in `login`'s landing for this repo.
    pub fn waits_in_landing(&self, login: &str, commit: &str) -> bool {
        let Ok(landing) = self.landing_dir(login) else {
            return false;
        };
        store_exists(&landing)
            && git(
                &landing,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/newgit/landing/{commit}"),
                ],
            )
            .is_ok_and(|output| output.status.success())
    }

    /// `commit`, waiting in `login`'s landing, brought into this repo's store
    /// at its landing ref, resolving against `borrow`'s store: where the
    /// tracker was when the commit was sent, which a thin pack and the
    /// commit's own history were checked against. The store is made if it
    /// is not yet (it is checked to be for its own audience). One already in
    /// the store, or never sent, is left alone.
    pub fn take_landing(&self, login: &str, commit: &str, borrow: &Repo) -> ApiResult<()> {
        if !self.waits_in_landing(login, commit) {
            return Ok(());
        }
        let landing = self.landing_dir(login)?;
        let reference = format!("refs/newgit/landing/{commit}");
        if !self.has_commit(commit) {
            create_store(&self.dir(), &self.store)?;
            self.open_landing_from(login, borrow)?;
            let refspec = format!("+{reference}:{}", self.full(&reference));
            let output = git(
                &self.dir(),
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    landing.as_str(),
                    &refspec,
                ],
            )?;
            if !output.status.success() {
                return Err(HttpError::internal(format!(
                    "bring {commit} from {landing} into {} ({}): {}",
                    self.dir(),
                    self.namespace,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        git(&landing, &["update-ref", "-d", &reference])?;
        Ok(())
    }
}

/// What [`Repo::copy_to`] copied, for [`Repo::finish_move`].
#[derive(Debug, Clone)]
pub struct Copied {
    /// The repo where it is now.
    pub to: Repo,
    /// Its store was renamed into place (or was gone already): nothing is
    /// left where it was.
    pub renamed: bool,
    /// Every ref copied (full names, as the store names them), at the object
    /// it was copied at.
    pub refs: Vec<(String, String)>,
}

/// One store's ref transaction, prepared (`git update-ref --stdin`, `start`
/// … `prepare`): every ref in it locked and checked against its old value,
/// nothing moved yet. [`Prepared::commit`] moves them all; dropping it
/// without committing moves none. A landing prepares every
/// store's transaction before it commits any, so a store that refuses
/// leaves every store as it was.
pub struct Prepared {
    dir: Utf8PathBuf,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    /// What it answers: kept open until it is done, so it is never written
    /// into a closed pipe.
    answers: std::io::BufReader<std::process::ChildStdout>,
}

impl Prepared {
    /// Prepare `lines` (`update <full ref> <new> <old>`, `delete <full ref>
    /// <old>`) in the store at `dir`.
    pub fn new(dir: &Utf8Path, lines: &[String]) -> ApiResult<Self> {
        use std::io::BufRead as _;
        let mut child = git_command(dir)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take();
        let mut script = String::from("start\n");
        for line in lines {
            script.push_str(line);
            script.push('\n');
        }
        script.push_str("prepare\n");
        let written = stdin.as_mut().map(|stdin| {
            stdin
                .write_all(script.as_bytes())
                .and_then(|()| stdin.flush())
        });
        let mut answers =
            std::io::BufReader::new(child.stdout.take().ok_or_else(|| {
                HttpError::internal(format!("update-ref in {dir} has no stdout"))
            })?);
        let mut prepared = false;
        if matches!(written, Some(Ok(()))) {
            let mut line = String::new();
            while answers.read_line(&mut line).is_ok_and(|read| read > 0) {
                if line.trim() == "prepare: ok" {
                    prepared = true;
                    break;
                }
                line.clear();
            }
        }
        if !prepared {
            drop(stdin);
            let output = child.wait_with_output()?;
            return Err(HttpError::internal(format!(
                "moving refs in {dir}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Self {
            dir: dir.to_owned(),
            child,
            stdin,
            answers,
        })
    }

    /// Move every ref prepared.
    pub fn commit(mut self) -> ApiResult<()> {
        use std::io::Read as _;
        let mut stdin = self.stdin.take();
        let sent = stdin
            .as_mut()
            .map(|stdin| stdin.write_all(b"commit\n").and_then(|()| stdin.flush()));
        drop(stdin);
        let mut rest = String::new();
        let _ = self.answers.read_to_string(&mut rest);
        let status = self.child.wait()?;
        if !matches!(sent, Some(Ok(()))) || !status.success() || !rest.contains("commit: ok") {
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return Err(HttpError::internal(format!(
                "committing refs in {}: {}",
                self.dir,
                stderr.trim()
            )));
        }
        Ok(())
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Closing stdin before `commit` aborts: nothing moved.
        drop(self.stdin.take());
        let _ = std::io::copy(&mut self.answers, &mut std::io::sink());
        let _ = self.child.wait();
    }
}

/// What each of `names` is in the repository at `dir`, by one `git cat-file
/// --batch-check`: a full ref name answers the object it names, a commit id
/// itself when the repository holds that commit. Anything missing (or not a
/// commit, for an id) is left out.
pub fn resolve(dir: &Utf8Path, names: &[String]) -> ApiResult<BTreeMap<String, String>> {
    let mut found = BTreeMap::new();
    if names.is_empty() || !store_exists(dir) {
        return Ok(found);
    }
    let mut child = git_command(dir)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let mut input = String::new();
        for name in names {
            input.push_str(name);
            input.push('\n');
        }
        stdin.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "reading refs in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let answers = String::from_utf8_lossy(&output.stdout);
    for (name, line) in names.iter().zip(answers.lines()) {
        let mut parts = line.split_whitespace();
        let (Some(object), Some(kind)) = (parts.next(), parts.next()) else {
            continue;
        };
        if kind == "missing" || kind == "ambiguous" {
            continue;
        }
        if name.starts_with("refs/") || kind == "commit" {
            found.insert(name.clone(), object.to_owned());
        }
    }
    Ok(found)
}

/// Whether the repository at `dir` holds `commit`.
pub fn has_commit(dir: &Utf8Path, commit: &str) -> bool {
    store_exists(dir)
        && git(dir, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .is_ok_and(|output| output.status.success())
}

/// Whether, in the repository at `dir`, `ancestor` is `descendant` or
/// behind it.
pub fn is_ancestor(dir: &Utf8Path, ancestor: &str, descendant: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
        .is_ok_and(|output| output.status.success())
}

/// `path`'s text at `commit` in the repository at `dir`.
pub fn read_blob(dir: &Utf8Path, commit: &str, path: &str) -> Option<String> {
    let output = git(dir, &["cat-file", "blob", &format!("{commit}:{path}")]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A commit's pins in the repository at `dir`: `.newgit/pins/<name>`
/// gitlinks, by child name.
pub fn pins_of(dir: &Utf8Path, commit: &str) -> Vec<(String, String)> {
    let Ok(output) = git(dir, &["ls-tree", &format!("{commit}:.newgit/pins")]) else {
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

/// How many repos, as the API names them (trackers, forks, users' content),
/// the stores in the home at `home` hold: one per namespace with any ref.
pub fn count_repos(home: &Utf8Path) -> usize {
    let mut found = std::collections::BTreeSet::new();
    let mut stack = vec![home.join("stores")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = dir.read_dir_utf8() else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path().to_owned();
            if !path.is_dir() {
                continue;
            }
            if !store_exists(&path) {
                stack.push(path);
                continue;
            }
            for (reference, _) in list_refs(&path, "refs/namespaces/").unwrap_or_default() {
                // `refs/namespaces/<t|u>/refs/namespaces/<name>/`, then for a
                // fork `refs/namespaces/i/refs/namespaces/<instance>/`.
                let parts: Vec<&str> = reference.split('/').collect();
                let repo = match parts.as_slice() {
                    [
                        "refs",
                        "namespaces",
                        kind,
                        "refs",
                        "namespaces",
                        name,
                        "refs",
                        "namespaces",
                        "i",
                        "refs",
                        "namespaces",
                        instance,
                        ..,
                    ] => {
                        format!("{kind}/{name}/i/{instance}")
                    }
                    ["refs", "namespaces", kind, "refs", "namespaces", name, ..] => {
                        format!("{kind}/{name}")
                    }
                    _ => continue,
                };
                // Per-user content is one repo per user's store.
                found.insert(format!(
                    "{}:{repo}",
                    if repo.starts_with("u/") {
                        path.as_str()
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    found.len()
}

impl Repo {
    /// Tracker `tracker`'s repo in the home at `home`: in `store`, or alone in
    /// a store of its own when it has none yet (where a new one is made).
    pub fn tracker(home: &Utf8Path, store: Option<Store>, tracker: &str) -> Self {
        Self::new(
            home,
            store.unwrap_or_else(|| Store::Tracker(tracker.to_owned())),
            crate::names::tracker_namespace(tracker),
        )
    }
}

/// `project`'s own tracker `tracker`, in the store its row names
/// ([`Repo::tracker`]).
pub fn tracker(
    data: &Utf8Path,
    db: &rusqlite::Connection,
    project: &crate::names::ProjectId,
    tracker: &str,
) -> ApiResult<Repo> {
    let store =
        crate::db::tracker_store(db, &project.key(), tracker)?.and_then(|name| Store::parse(&name));
    Ok(Repo::tracker(&project.dir(data), store, tracker))
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
