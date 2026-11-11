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
//!   which says nothing about anyone's content.
//! - **Composed objects never reach a store.** The composed trees cross
//!   every audience, so they are made in a scratch repository of the merge's
//!   own (`<home>/scratch/<random>.git`), which borrows every store's objects
//!   (alternates), is served to no one, and is removed when the merge is done.
//!   Only each tracker's own new commit, and the objects it adds over its
//!   parents, leave it: into the merger's landing for that tracker
//!   (`Repo::open_landing`), where the merger's own commits wait, and from
//!   there into a store only as `merge::finish` places any of a merge's
//!   commits.

use std::io::Write as _;
use std::process::{Command, Stdio};

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
    /// The commits the server wrote, as `(tracker, commit)`: each waits at
    /// its landing ref in the merger's landing for that tracker.
    pub made: Vec<(String, String)>,
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

fn output(dir: &Utf8Path, args: &[&str], input: Option<&[u8]>) -> ApiResult<std::process::Output> {
    let mut child = repos::git_command(dir)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input)?;
    }
    Ok(child.wait_with_output()?)
}

/// Merge `list` (the merger's index, made on `base`) onto `head` (the
/// branch now), for `login` landing into `branch`. Refused (409) only for a
/// real conflict, naming each file, or for what cannot be split.
pub fn merge(
    project: &ProjectContext<'_>,
    login: &str,
    branch: &str,
    base: &List,
    head: &List,
    list: &List,
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
    let stdout = String::from_utf8_lossy(&merged.stdout).into_owned();
    let mut fields = stdout.split('\0').filter(|field| !field.is_empty());
    let tree = fields.next().unwrap_or_default().trim().to_owned();
    match merged.status.code() {
        Some(0) => {}
        Some(1) => {
            let mut files: Vec<String> = fields.map(str::to_owned).collect();
            files.dedup();
            let named = files
                .iter()
                .map(|file| format!("`{file}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(HttpError::new(
                409,
                format!(
                    "this merge conflicts with what landed on `{branch}` since your base, in {named}: bring `{branch}` into your work, resolve, and merge again"
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
    };
    let split =
        compose::split(&scratch.dir, &merge_view, &options).map_err(|error| match error {
            SplitError::Refused(refusal) => HttpError::new(
                409,
                format!("the merge of this onto `{branch}` cannot be split by tracker: {refusal}"),
            ),
            SplitError::Git(error) => HttpError::internal(error),
        })?;
    // Each tracker's new commit, and what it adds over its parents (its own
    // tree), into the merger's landing for it: nothing composed leaves.
    for (tracker, commit) in &split.made {
        if let Some((home, _)) = project.links.get(tracker) {
            return Err(HttpError::new(
                409,
                format!(
                    "`{tracker}` is linked from `{home}`, and both this merge and what landed on `{branch}` since your base moved it: land it at its home first"
                ),
            ));
        }
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
        run(
            &landing,
            &["update-ref", &crate::merge::landing_ref(commit), commit],
            None,
        )?;
    }
    Ok(Merged {
        list: split.index,
        made: split.made,
    })
}
