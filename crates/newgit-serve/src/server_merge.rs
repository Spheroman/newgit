//! The server's own merge, for a `newgit merge` whose branch moved since
//! its base (docs/design/landing.md, *What a user sees* 2 and 3, *Landing*
//! 1): Git's merge over the composed tree, split back into each tracker's
//! history.
//!
//! - **Composed, merged, split.** The base, the head the merge lands on, and
//!   the merger's own index are each composed into one view tree (every
//!   tracker expanded at its mount, `newgit_core::compose::view_tree`), Git
//!   merges the three (`git merge-tree`, so equal subtrees are skipped and the
//!   cost follows the change), and the result is split by tracker
//!   (`newgit_core::compose::split`) on the head's and the merger's root
//!   commits. A file moved between trackers is followed as in one repository,
//!   and so is a directory renamed (`merge.directoryRenames=true`).
//! - **The merger's commits keep their ids.** Split reuses every commit whose
//!   tree is the merge's: a tracker only the merger moved lands at the
//!   merger's own commit, one only the head moved stays at the head's. A
//!   tracker both moved gets one merge commit, the head's commit first and
//!   the merger's second, as GitHub's merge button writes it.
//! - **One commit, one audience.** Each merge commit holds its own tracker's
//!   tree and pins, and the stock message `Merge `<login>` into <branch>`,
//!   which says nothing about anyone's content. A merge that would follow a
//!   move into a tracker whose store does not include the one the content
//!   was in (so someone who could not read it could), carrying the other
//!   side's change along, is refused ([`carried`]); a conflict names only
//!   files, and trackers, the merger may read.
//! - **Only when the changes meet.** A merge whose changes are apart from
//!   what landed since its base never gets here: `crate::reparent` re-makes
//!   it on the head without Git's merge.
//! - **Composed objects never reach a store.** The composed trees cross
//!   every audience, so they are made in a scratch repository of the merge's
//!   own (`<home>/scratch/<random>.git`), which borrows every store's objects
//!   (alternates), is served to no one, and is removed when the merge is done.
//!   Only each tracker's own new commit, and the objects it adds over its
//!   parents, leave it: into the merger's landing for that tracker
//!   (`Repo::open_landing`), where the merger's own commits wait, and from
//!   there into a store only as `merge::finish` places any of a merge's
//!   commits.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, NOTES_REF, SplitError, SplitOptions};
use newgit_core::objects::Objects;

use crate::api::ProjectContext;
use crate::db::List;
use crate::error::{ApiResult, HttpError};
use crate::names::parse_project_key;
use crate::repos;

/// Who the view commits that exist only in the scratch repository are by.
const NOBODY: &[u8] = b"newgit <newgit@localhost> 0 +0000";

/// What the server's merge made.
#[derive(Debug)]
pub struct Merged {
    /// The index that lands: each tracker's commit.
    pub list: List,
    /// The commits the server wrote: each waits at its landing ref in the
    /// merger's landing for its tracker until it lands or is given up.
    pub made: Made,
}

/// A repository of one merge's own, removed when dropped.
struct Scratch {
    dir: Utf8PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where a home's scratch repositories are: never a store, never served.
pub fn scratch_dir(home: &Utf8Path) -> Utf8PathBuf {
    home.join("scratch")
}

/// Every scratch repository under `<data>/homes/<owner>/<project>/`: run at
/// startup, when no merge is running, for those a stopped server left.
pub fn remove_scratch(data: &Utf8Path) {
    let Ok(owners) = std::fs::read_dir(data.join("homes")) else {
        return;
    };
    for owner in owners.flatten() {
        let Ok(projects) = std::fs::read_dir(owner.path()) else {
            continue;
        };
        for project in projects.flatten() {
            let _ = std::fs::remove_dir_all(project.path().join("scratch"));
        }
    }
}

/// Every bare repository under `dir`, at any depth: its `objects`.
fn object_dirs(dir: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        if path.extension() == Some("git") && path.join("objects").is_dir() {
            out.push(path.join("objects"));
        } else {
            object_dirs(&path, out);
        }
    }
}

impl Scratch {
    /// A new scratch repository in `project`'s home, borrowing the objects
    /// of every store there and of `login`'s landings (where the merge's own
    /// commits wait), and of each linked tracker's home.
    fn open(project: &ProjectContext<'_>, login: &str) -> ApiResult<Self> {
        let home = project.home();
        let dir = scratch_dir(&home).join(format!("{}.git", crate::tokens::random_hex(12)));
        std::fs::create_dir_all(scratch_dir(&home))?;
        let scratch = Self { dir };
        let output = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(scratch.dir.as_str())
            .output()?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "git init {}: {}",
                scratch.dir,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        for (key, value) in [
            ("commit.gpgSign", "false"),
            ("gc.auto", "0"),
            ("user.name", "newgit"),
            ("user.email", "newgit@localhost"),
        ] {
            run(&scratch.dir, &["config", key, value], None)?;
        }
        let mut borrowed = Vec::new();
        object_dirs(&home.join("stores"), &mut borrowed);
        object_dirs(&home.join("landing").join(login), &mut borrowed);
        for (linked_home, _) in project.links.values() {
            if let Some(id) = parse_project_key(linked_home) {
                object_dirs(
                    &id.dir(&project.app.config.data).join("stores"),
                    &mut borrowed,
                );
            }
        }
        let mut alternates = String::new();
        for dir in borrowed {
            alternates.push_str(dir.as_str());
            alternates.push('\n');
        }
        std::fs::write(scratch.dir.join("objects/info/alternates"), alternates)?;
        Ok(scratch)
    }
}

/// Git in `dir`, `input` on its stdin; its stdout, or why it failed.
fn run(dir: &Utf8Path, args: &[&str], input: Option<&[u8]>) -> ApiResult<Vec<u8>> {
    let output = output(dir, args, input)?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "git {} in {dir}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// How long any one `git` the merge runs may take before it is stopped. A
/// hung `merge-tree` would otherwise hold its turn, or its roots, for ever.
pub const GIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

fn output(dir: &Utf8Path, args: &[&str], input: Option<&[u8]>) -> ApiResult<std::process::Output> {
    repos::git_timed(dir, args, input, GIT_LIMIT)
}

/// What the merger may see, and who may read each tracker: read under the
/// db lock before the merge runs, at the head it merges onto.
#[derive(Debug, Default, Clone)]
pub struct Seen {
    /// The trackers the merger may read.
    pub readable: BTreeSet<String>,
    /// The store each tracker is in, which says who may read it.
    pub stores: BTreeMap<String, repos::Store>,
}

impl Seen {
    /// Whether content of tracker `from` may be carried into `to` without
    /// anyone choosing to: every reader of `to` may read `from` already.
    fn may_carry(&self, from: &str, to: &str) -> bool {
        from == to
            || matches!(
                (self.stores.get(from), self.stores.get(to)),
                (Some(from), Some(to)) if from.includes(to)
            )
    }
}

/// Each tracker's directory in a composed view, by its name here.
struct Mounts(Vec<(String, String)>);

fn depth(at: &str) -> usize {
    if at == "." { 0 } else { at.split('/').count() }
}

/// Whether the directory `at` holds `path` (bytes, as Git stores names).
fn holds(at: &str, path: &[u8]) -> bool {
    at == "."
        || path == at.as_bytes()
        || path
            .strip_prefix(at.as_bytes())
            .is_some_and(|rest| rest.starts_with(b"/"))
}

impl Mounts {
    fn read(objects: &mut Objects, roots: &[&str]) -> ApiResult<Self> {
        let mut all = BTreeSet::new();
        for root in roots {
            for (chain, at) in compose::view_mounts(objects, root)? {
                let name = if chain.is_empty() {
                    "root".to_owned()
                } else {
                    chain.join("__")
                };
                all.insert((name, at));
            }
        }
        Ok(Self(all.into_iter().collect()))
    }

    /// Every tracker whose directory holds `path`, at any depth.
    fn holding(&self, path: &[u8]) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, at)| holds(at, path))
            .map(|(tracker, _)| tracker.as_str())
            .collect()
    }

    /// The trackers `path` is in: those holding it at the deepest directory
    /// (more than one where trackers share a directory).
    fn owners(&self, path: &[u8]) -> Vec<&str> {
        let deepest = self
            .0
            .iter()
            .filter(|(_, at)| holds(at, path))
            .map(|(_, at)| depth(at))
            .max()
            .unwrap_or(0);
        self.0
            .iter()
            .filter(|(_, at)| holds(at, path) && depth(at) == deepest)
            .map(|(tracker, _)| tracker.as_str())
            .collect()
    }

    /// `paths` as the merger may be told of them: each one in trackers it
    /// may read by name; of the rest, not even how many.
    fn name(&self, paths: &[Vec<u8>], seen: &Seen) -> String {
        let mut named = Vec::new();
        let mut hidden = false;
        for path in paths {
            if self
                .holding(path)
                .iter()
                .all(|tracker| seen.readable.contains(*tracker))
            {
                named.push(format!("`{}`", String::from_utf8_lossy(path)));
            } else {
                hidden = true;
            }
        }
        if hidden {
            named.push("one or more files in trackers you cannot read".to_owned());
        }
        named.join(", ")
    }
}

/// What one side changed from the base, as Git's rename detection (the
/// merge's own, 50% similar) sees it.
/// Paths are the bytes Git stores (`-z`: never quoted).
#[derive(Default)]
struct Side {
    renames: Vec<(Vec<u8>, Vec<u8>)>,
    modified: BTreeSet<Vec<u8>>,
    added: BTreeSet<Vec<u8>>,
}

impl Side {
    fn read(dir: &Utf8Path, base: &str, view: &str) -> ApiResult<Self> {
        let out = run(
            dir,
            &["diff-tree", "-r", "-M", "-z", "--name-status", base, view],
            None,
        )?;
        let mut fields = out
            .split(|&byte| byte == 0)
            .filter(|field| !field.is_empty());
        let mut side = Self::default();
        while let Some(status) = fields.next() {
            let Some(path) = fields.next() else { break };
            match status.first() {
                Some(b'R' | b'C') => {
                    let Some(to) = fields.next() else { break };
                    if status.starts_with(b"R") {
                        side.renames.push((path.to_vec(), to.to_vec()));
                    }
                }
                Some(b'M' | b'T') => {
                    side.modified.insert(path.to_vec());
                }
                Some(b'A') => {
                    side.added.insert(path.to_vec());
                }
                _ => {}
            }
        }
        Ok(side)
    }
}

/// What the merge would carry across trackers into one whose store does not
/// include the store of the tracker it was in (decision 1: only the same
/// store, or the public one, includes another), by following a move the
/// *other* side made: `(path, from, to)`.
/// A change one side made to a file the other side moved into a wider
/// tracker, or a file one side added under a directory the other side
/// moved there (Git's directory renames).
fn carried(
    objects: &mut Objects,
    merged: &str,
    sides: [&Side; 2],
    mounts: &Mounts,
    seen: &Seen,
) -> ApiResult<Vec<(Vec<u8>, String, String)>> {
    let mut out = Vec::new();
    for (moved, other) in [(sides[0], sides[1]), (sides[1], sides[0])] {
        for (from_path, to_path) in &moved.renames {
            let Some((from, to)) = mounts.owners(from_path).into_iter().find_map(|from| {
                mounts
                    .owners(to_path)
                    .into_iter()
                    .find(|to| !seen.may_carry(from, to))
                    .map(|to| (from.to_owned(), to.to_owned()))
            }) else {
                continue;
            };
            if other.modified.contains(from_path) {
                out.push((from_path.clone(), from.clone(), to.clone()));
            }
            if let Some(slash) = from_path.iter().rposition(|&byte| byte == b'/') {
                let dir = &from_path[..slash];
                for added in &other.added {
                    if added
                        .strip_prefix(dir)
                        .is_some_and(|rest| rest.starts_with(b"/"))
                        && objects.entry_at(merged, added)?.is_none()
                    {
                        out.push((added.clone(), from.clone(), to.clone()));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Why a merge that would carry `what` from tracker `from` into `to` is
/// refused, naming only the trackers the merger may read. The stores are
/// not ordered (a `user:` tracker's readers are not a subset of the
/// project's, nor the other way), so this says some readers of `to` may
/// not read `from`, never that more people may.
fn refused_carry(what: &str, from: &str, to: &str, seen: &Seen, branch: &str) -> String {
    let readable = |tracker: &str| seen.readable.contains(tracker);
    let from_named = if readable(from) {
        format!("`{from}`")
    } else {
        "a tracker you cannot read".to_owned()
    };
    let into = match (readable(to), readable(from)) {
        (true, true) => format!("`{to}`, which some who may not read `{from}` may read"),
        (true, false) => format!("`{to}`, which some who may not read that tracker may read"),
        (false, _) => {
            "a tracker you cannot read, which some who may not read where it was may read"
                .to_owned()
        }
    };
    format!(
        "this merge would carry {what} from {from_named} into {into}, by following a move made on the other side, and nobody chose to widen who may read it: bring `{branch}` into your work and merge again, so any such move is one you make"
    )
}

/// Merge `list` (the merger's index, made on `base`) onto `head` (the
/// branch now), for `login` landing into `branch`. Refused (409) for a real
/// conflict, naming each file the merger may read; for content it would
/// carry into a tracker some who may not read it may read (see
/// [`carried`]); and for
/// what cannot be split.
#[allow(clippy::too_many_arguments, reason = "each is one thing a merge is")]
pub fn merge(
    project: &ProjectContext<'_>,
    login: &str,
    branch: &str,
    base: &List,
    head: &List,
    list: &List,
    seen: &Seen,
) -> ApiResult<Merged> {
    let root_of = |index: &List| {
        index
            .get("root")
            .cloned()
            .ok_or_else(|| HttpError::new(400, "an index names root's commit"))
    };
    let (base_root, head_root, list_root) = (root_of(base)?, root_of(head)?, root_of(list)?);
    let scratch = Scratch::open(project, login)?;
    let mut objects = Objects::open(&scratch.dir)?;
    let view = |objects: &mut Objects, root: &str| -> ApiResult<String> {
        let tree = compose::view_tree(objects, root)?;
        Ok(objects.make_commit(&tree, &[], NOBODY, NOBODY, b"", b"view\n"))
    };
    let base_view = view(&mut objects, &base_root)?;
    let head_view = view(&mut objects, &head_root)?;
    let list_view = view(&mut objects, &list_root)?;
    objects.flush()?;
    let mounts = Mounts::read(&mut objects, &[&base_root, &head_root, &list_root])?;
    // Which root commit each side's view stands for, as split reads it.
    for (view, root) in [(&head_view, &head_root), (&list_view, &list_root)] {
        run(
            &scratch.dir,
            &[
                "notes",
                "--ref",
                NOTES_REF,
                "add",
                "-f",
                "-m",
                &format!("root {root}"),
                view,
            ],
            None,
        )?;
    }
    let merged = output(
        &scratch.dir,
        &[
            "-c",
            "merge.directoryRenames=true",
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            &format!("--merge-base={base_view}"),
            &head_view,
            &list_view,
        ],
        None,
    )?;
    let mut fields = merged
        .stdout
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let tree = String::from_utf8_lossy(fields.next().unwrap_or_default())
        .trim()
        .to_owned();
    match merged.status.code() {
        Some(0) => {}
        Some(1) => {
            let mut files: Vec<Vec<u8>> = fields.map(<[u8]>::to_vec).collect();
            files.sort();
            files.dedup();
            return Err(HttpError::new(
                409,
                format!(
                    "this merge conflicts with what landed on `{branch}` since your base, in {}: bring `{branch}` into your work, resolve, and merge again",
                    mounts.name(&files, seen)
                ),
            ));
        }
        _ => {
            return Err(HttpError::internal(format!(
                "git merge-tree in {}: {}",
                scratch.dir,
                String::from_utf8_lossy(&merged.stderr).trim()
            )));
        }
    }
    // One commit, one audience: nothing crosses into a tracker others
    // may read unless someone chose to move it there from what they read.
    let sides = [
        Side::read(&scratch.dir, &base_view, &head_view)?,
        Side::read(&scratch.dir, &base_view, &list_view)?,
    ];
    let crossing = carried(&mut objects, &tree, [&sides[0], &sides[1]], &mounts, seen)?;
    if let Some((_, from, to)) = crossing.first() {
        let paths: Vec<Vec<u8>> = crossing.iter().map(|(path, _, _)| path.clone()).collect();
        return Err(HttpError::new(
            409,
            refused_carry(&mounts.name(&paths, seen), from, to, seen, branch),
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let by = format!("{login} <{login}@users.noreply.github.com> {now} +0000");
    let message = format!("Merge `{login}` into {branch}\n");
    let merge_view = objects.make_commit(
        &tree,
        &[head_view, list_view],
        by.as_bytes(),
        by.as_bytes(),
        b"",
        message.as_bytes(),
    );
    objects.flush()?;
    let bound = |_: &str| true;
    let audience = |_: &str| None;
    let options = SplitOptions {
        root: "root",
        bound: &bound,
        audience: &audience,
        newgit_made: None,
        need_history: &|_| Ok(()),
        // Each side's forks are read from its own commits, the merge's
        // parents.
        forks_before: &[],
    };
    let split =
        compose::split(&scratch.dir, &merge_view, &options).map_err(|error| match error {
            SplitError::Refused(refusal) => HttpError::new(
                409,
                format!("the merge of this onto `{branch}` cannot be split by tracker: {refusal}"),
            ),
            SplitError::Git(error) => HttpError::internal(error),
        })?;
    if let Some((tracker, _)) = split
        .made
        .iter()
        .find(|(tracker, _)| project.links.contains_key(tracker))
    {
        let home = &project.links[tracker].0;
        return Err(HttpError::new(
            409,
            format!(
                "`{tracker}` is linked from `{home}`, and both this merge and what landed on `{branch}` since your base moved it: land it at its home first"
            ),
        ));
    }
    // Each tracker's new commit, and what it adds over its parents (its own
    // tree), into the merger's landing for it: nothing composed leaves.
    let mut made = Made::default();
    for (tracker, commit) in &split.made {
        let parents = objects.commit(commit)?.parents;
        let mut revs = format!("{commit}\n");
        for parent in &parents {
            revs.push_str(&format!("^{parent}\n"));
        }
        let pack = run(
            &scratch.dir,
            &["pack-objects", "--revs", "--stdout", "-q"],
            Some(revs.as_bytes()),
        )?;
        let landing = project.tracker(tracker).open_landing(login)?;
        run(&landing, &["index-pack", "--stdin"], Some(&pack))?;
        made.wait(&landing, tracker, commit)?;
    }
    Ok(Merged {
        list: split.index,
        made,
    })
}

/// The commits the server made for one merge, each waiting at its landing
/// ref in the merger's landing for its tracker. Dropped without
/// [`Made::landed`], they are given up: their refs go, so an attempt that
/// lost leaves nothing behind that names them.
#[derive(Default)]
pub struct Made {
    commits: Vec<(String, String, Utf8PathBuf)>,
}

impl std::fmt::Debug for Made {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.commits.iter().map(|(t, c, _)| (t, c)))
            .finish()
    }
}

impl Made {
    /// `commit`, in `landing` (tracker `tracker`'s for this merger), set at
    /// its landing ref.
    pub fn wait(&mut self, landing: &Utf8Path, tracker: &str, commit: &str) -> ApiResult<()> {
        run(
            landing,
            &["update-ref", &crate::merge::landing_ref(commit), commit],
            None,
        )?;
        self.commits
            .push((tracker.to_owned(), commit.to_owned(), landing.to_owned()));
        Ok(())
    }

    /// Whether the server made `commit` for `tracker` here.
    pub fn has(&self, tracker: &str, commit: &str) -> bool {
        self.commits
            .iter()
            .any(|(made, id, _)| made == tracker && id == commit)
    }

    /// Those made, and kept: `commit`s still named by what lands.
    pub fn keep(&mut self, landing: &List) {
        self.commits
            .retain(|(tracker, commit, _)| landing.get(tracker) != Some(commit));
    }

    /// Everything here landed (or is named by an entry written down): its
    /// refs are the landing's to remove.
    pub fn landed(mut self) {
        self.commits.clear();
    }

    pub fn absorb(&mut self, mut other: Made) {
        self.commits.append(&mut other.commits);
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        for (_, commit, landing) in self.commits.drain(..) {
            let _ = repos::git(
                &landing,
                &["update-ref", "-d", &crate::merge::landing_ref(&commit)],
            );
        }
    }
}
