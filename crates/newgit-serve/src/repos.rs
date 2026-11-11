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

    /// Whether a tracker moving from this store to `to` widens who reads it:
    /// everyone who reads this store reads `to`, and more. Only `public` is
    /// known to hold every other audience. Any other two (`user:a` and
    /// `user:b`, or `user:a` and `project-devs`, whose login need not be a
    /// member) are not ordered by inclusion, and a move between them is not a
    /// widening.
    pub fn widens_to(&self, to: &Store) -> bool {
        *to == Self::Public && *self != Self::Public
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
        has_commit(&self.dir(), commit)
    }

    /// Whether `ancestor` is `descendant` or behind it.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        is_ancestor(&self.dir(), ancestor, descendant)
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

    /// A commit's pins: `.newgit/pins/<name>` gitlinks, by child name.
    pub fn pins_of(&self, commit: &str) -> Vec<(String, String)> {
        pins_of(&self.dir(), commit)
    }

    /// Bring this repo's refs into `to`, a store of `to`'s own, under the same
    /// namespace: copied, then `place`d (the row saying where it is now
    /// follows it), then removed here. A store of one tracker alone moving to
    /// an audience no store holds yet is that store, labelled for `to` and
    /// renamed. So a move that stops part way leaves the repo whole where the
    /// row says, or whole where it was going with the row behind it, which
    /// `ProjectContext::settle` repairs; never a row naming a store without
    /// it, nor a store at `to`'s path labelled for another audience.
    ///
    /// A ref removed here must still be what was copied: a push that moved
    /// one meanwhile fails the move (the row goes back), rather than being
    /// dropped while its pusher was told it landed.
    pub fn move_to(&self, to: &Store, place: impl Fn(&Store) -> ApiResult<()>) -> ApiResult<Self> {
        let moved = Self::new(&self.home, to.clone(), self.namespace.clone());
        let alone = matches!(self.store, Store::Tracker(_));
        if alone && !store_exists(&moved.dir()) {
            checked(&self.dir(), &["config", AUDIENCE_KEY, &to.name()])?;
            if let Some(parent) = moved.dir().parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(self.dir(), moved.dir())?;
            place(to)?;
        } else {
            let copied = list_refs(&self.dir(), &self.full("refs/"))?;
            moved.copy_from(&self.dir(), &self.prefix(), "refs/")?;
            place(to)?;
            if let Err(error) = self.remove_exactly(&copied) {
                place(&self.store)?;
                return Err(error);
            }
            let left = list_refs(&self.dir(), &self.full("refs/"))?;
            if !left.is_empty() {
                // Made after the copy (a push in flight): kept, not lost, and
                // said. The store is the old audience's, which could read it.
                eprintln!(
                    "newgit serve: {} ({}) gained {} refs while it moved to {}; they stay in {}",
                    self.namespace,
                    self.store.name(),
                    left.len(),
                    to.name(),
                    self.dir()
                );
            } else if alone {
                delete(&self.dir())?;
            }
        }
        // `stores/tracker/`, once the last seed in it is gone.
        if let Some(parent) = self.dir().parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(moved)
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
                "a ref of {} in {} moved while it was copied; it stays here",
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

    /// [`Self::landing_dir`], made if it is not yet, borrowing the store's
    /// objects (its alternates, rewritten each time so they follow the store
    /// when it moves) so a thin pack resolves against them. A push into it
    /// is told of this repo's own branches as what the server has, and no
    /// other ref in the store.
    pub fn open_landing(&self, login: &str) -> ApiResult<Utf8PathBuf> {
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
            format!("{}\n", self.dir().join("objects")),
        )?;
        checked(
            &dir,
            &[
                "config",
                "core.alternateRefsPrefixes",
                &self.full("refs/heads/"),
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

    /// `commit`, waiting in `login`'s landing, brought into this repo at its
    /// landing ref, once the repo is in the store the landing left it in.
    /// One already in the store, or never sent, is left alone.
    pub fn take_landing(&self, login: &str, commit: &str) -> ApiResult<()> {
        let landing = self.landing_dir(login)?;
        let reference = format!("refs/newgit/landing/{commit}");
        if !store_exists(&landing)
            || !git(&landing, &["rev-parse", "--verify", "--quiet", &reference])
                .is_ok_and(|output| output.status.success())
        {
            return Ok(());
        }
        if !self.has_commit(commit) {
            self.open_landing(login)?;
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
