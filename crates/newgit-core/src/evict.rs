//! What `newgit stop --evict` has to know about a workspace beyond what a
//! checkpoint holds: the parts of its Git that live only in its `.git/`
//! (other branches, tags, stashes, what is staged), the Git operation it may
//! be in the middle of, the files Git lists that no checkpoint carries, and
//! the processes still sitting in it.
//!
//! A checkpoint is the workspace's working tree and the commit it is on. An
//! eviction deletes the whole directory, `.git/` included, so everything
//! else Git keeps there either goes into the store first ([`save_git_state`],
//! brought back by [`restore_git_state`]) or stops the eviction.

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::source::run_git_env;

fn git(workspace: &Utf8Path, args: &[&str]) -> Result<String> {
    let mut full = vec!["-C", workspace.as_str()];
    full.extend(args);
    run_git_env(&full, &[])
}

/// The Git operation `workspace` is in the middle of, if any: a merge, a
/// rebase, a cherry-pick, a revert, a bisect, or conflicts not yet
/// resolved. Their state is in `.git/`, which no checkpoint carries.
pub fn operation_in_progress(workspace: &Utf8Path) -> Result<Option<&'static str>> {
    for (name, what) in [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("BISECT_LOG", "a bisect"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase or `git am`"),
    ] {
        let path = Utf8PathBuf::from(git(workspace, &["rev-parse", "--git-path", name])?);
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        if path.exists() {
            return Ok(Some(what));
        }
    }
    if !git(workspace, &["ls-files", "--unmerged"])?.is_empty() {
        return Ok(Some("conflicts not yet resolved"));
    }
    Ok(None)
}

/// Every `-z` entry of `git <args>` in `workspace`.
fn listed(workspace: &Utf8Path, args: &[&str]) -> Result<Vec<String>> {
    Ok(git(workspace, args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Untracked directories that are repositories of their own (a `git clone`
/// inside the workspace). A checkpoint's `git add -A` records one as a
/// commit id, not its files, so it would not come back.
pub fn untracked_repositories(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(listed(
        workspace,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?
    .into_iter()
    .filter(|path| path.ends_with('/'))
    .map(|path| Utf8PathBuf::from(path.trim_end_matches('/')))
    .collect())
}

/// Tracked submodules that are initialized: a checkpoint records each as
/// the commit it is on, never its files or its own uncommitted work, and
/// its repository lives in this workspace's `.git/modules`.
pub fn initialized_submodules(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(listed(workspace, &["ls-files", "-z", "--stage"])?
        .into_iter()
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            meta.starts_with("160000 ").then(|| Utf8PathBuf::from(path))
        })
        .filter(|path| {
            let dir = workspace.join(path);
            dir.join(".git").exists()
                || std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some())
        })
        .collect())
}

/// Worktrees linked to the workspace's repository (`git worktree add`):
/// their Git lives in this workspace's `.git/worktrees`, so deleting it
/// leaves each one "not a git repository".
pub fn linked_worktrees(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(git(workspace, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .skip(1)
        .map(Utf8PathBuf::from)
        .collect())
}

/// The ignored entries of `workspace` as `git ls-files --directory` names
/// them, without walking a wholly ignored directory: `(path, is_dir)`.
pub fn ignored_entries(workspace: &Utf8Path) -> Result<Vec<(Utf8PathBuf, bool)>> {
    Ok(listed(
        workspace,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?
    .into_iter()
    .map(|path| {
        let dir = path.ends_with('/');
        (Utf8PathBuf::from(path.trim_end_matches('/')), dir)
    })
    .collect())
}

/// Every ignored file under the directories `dirs`, workspace-relative. A
/// repository inside one is named as its directory.
pub fn ignored_files_under(workspace: &Utf8Path, dirs: &[&Utf8Path]) -> Result<Vec<Utf8PathBuf>> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "--literal-pathspecs",
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--",
    ];
    args.extend(dirs.iter().map(|dir| dir.as_str()));
    Ok(listed(workspace, &args)?
        .into_iter()
        .map(|path| Utf8PathBuf::from(path.trim_end_matches('/')))
        .collect())
}

/// An ignored path an eviction loses: a whole ignored directory, with how
/// many files are under it, or one file (`files == 1`, `dir == false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discarded {
    pub path: Utf8PathBuf,
    pub dir: bool,
    pub files: usize,
}

impl std::fmt::Display for Discarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.dir {
            write!(
                f,
                "{}/ ({} file{})",
                self.path,
                self.files,
                if self.files == 1 { "" } else { "s" }
            )
        } else {
            write!(f, "{}", self.path)
        }
    }
}

/// `lost`, the ignored files an eviction would lose, named by the top-level
/// ignored directory (`entries`, from [`ignored_entries`]) when every file
/// `all` lists under it is lost, each file otherwise. Linear in the files
/// apart from the sort.
pub fn summarize(
    entries: &[(Utf8PathBuf, bool)],
    all: &[Utf8PathBuf],
    lost: &[Utf8PathBuf],
) -> Vec<Discarded> {
    if lost.is_empty() {
        return Vec::new();
    }
    let lost_set: std::collections::HashSet<&str> = lost.iter().map(|p| p.as_str()).collect();
    let sorted: std::collections::BTreeSet<&str> = all.iter().map(|p| p.as_str()).collect();
    let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (entry, dir) in entries {
        if !dir {
            continue;
        }
        let prefix = format!("{entry}/");
        let under: Vec<&str> = sorted
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|file| file.starts_with(&prefix))
            .copied()
            .collect();
        if !under.is_empty() && under.iter().all(|file| lost_set.contains(file)) {
            out.push(Discarded {
                path: entry.clone(),
                dir: true,
                files: under.len(),
            });
            covered.extend(under);
        }
    }
    for file in lost {
        if !covered.contains(file.as_str()) {
            out.push(Discarded {
                path: file.clone(),
                dir: false,
                files: 1,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup();
    out
}

/// What neither Git nor a tracker carries of the files an eviction keeps:
/// a mode other than the one a fresh checkout gives (`0644`, or `0755`
/// when executable; `0755` for a directory), and directories with nothing
/// in them. Saved at eviction, put back by [`reapply_files`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FileModes {
    /// Workspace-relative path → permission bits.
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, u32>,
    #[serde(default)]
    pub empty_dirs: Vec<String>,
}

/// [`FileModes`] for `workspace`, leaving out `.git` and every path
/// `skip` says the eviction does not keep (a tree a resource rebuilds,
/// newgit's marker, a rendered file, a discarded ignored file). Only the
/// permission bits are kept: setuid, setgid, and sticky bits are not put
/// back by a restore, and the second value names the paths that had one,
/// for the not-kept list.
#[cfg(unix)]
pub fn record_files(
    workspace: &Utf8Path,
    skip: &dyn Fn(&Utf8Path) -> bool,
) -> Result<(FileModes, Vec<String>)> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut out = FileModes::default();
    let mut special = Vec::new();
    let mut pending = vec![Utf8PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let dir = if relative.as_str().is_empty() {
            workspace.to_path_buf()
        } else {
            workspace.join(&relative)
        };
        let entries = std::fs::read_dir(&dir).map_err(|source| NewgitError::io(&dir, source))?;
        let mut any = false;
        for entry in entries {
            let entry = entry.map_err(|source| NewgitError::io(&dir, source))?;
            any = true;
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let path = if relative.as_str().is_empty() {
                Utf8PathBuf::from(&name)
            } else {
                relative.join(&name)
            };
            if path == ".git" || skip(&path) {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|source| NewgitError::io(workspace.join(&path), source))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let all = metadata.permissions().mode() & 0o7777;
            if all & 0o7000 != 0 {
                special.push(path.to_string());
            }
            let mode = all & 0o777;
            let default = if metadata.is_dir() || mode & 0o100 != 0 {
                0o755
            } else {
                0o644
            };
            if mode != default {
                out.modes.insert(path.to_string(), mode);
            }
            if metadata.is_dir() {
                pending.push(path);
            }
        }
        if !any && !relative.as_str().is_empty() {
            out.empty_dirs.push(relative.to_string());
        }
    }
    out.empty_dirs.sort();
    special.sort();
    Ok((out, special))
}

#[cfg(not(unix))]
pub fn record_files(
    _: &Utf8Path,
    _: &dyn Fn(&Utf8Path) -> bool,
) -> Result<(FileModes, Vec<String>)> {
    Ok((FileModes::default(), Vec::new()))
}

/// The not-kept entry for the paths [`record_files`] found with a setuid,
/// setgid, or sticky bit, if any.
pub fn special_bits_not_kept(paths: &[String]) -> Option<String> {
    (!paths.is_empty()).then(|| {
        format!(
            "setuid, setgid, and sticky bits on {} (the files and their other mode bits are \
             kept)",
            list_strings(paths)
        )
    })
}

/// `` `a`, `b`, `c`, … `` (at most eight named).
fn list_strings(items: &[String]) -> String {
    const SHOWN: usize = 8;
    let mut out = items
        .iter()
        .take(SHOWN)
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN {
        out.push_str(&format!(", and {} more", items.len() - SHOWN));
    }
    out
}

/// Whether `relative` names something inside `workspace` reached without
/// following a symlink: no component on the way is one. What is at the
/// end may be anything, or nothing.
fn no_symlink_on_the_way(workspace: &Utf8Path, relative: &Utf8Path) -> bool {
    let mut at = workspace.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if !matches!(component, camino::Utf8Component::Normal(_)) {
            return false;
        }
        at.push(component);
        if components.peek().is_some()
            && std::fs::symlink_metadata(&at).is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return false;
        }
    }
    true
}

/// Delete the directory `path` and everything in it, as `rm -rf` would
/// after `chmod -R u+w`: a directory without its owner's write (or search)
/// permission is given it first, so a read-only tree in the workspace
/// (`chmod 555`) does not stop the delete half way. Never follows a
/// symlink.
pub fn delete_tree(path: &Utf8Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut pending = vec![path.as_std_path().to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(meta) = std::fs::symlink_metadata(&dir) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let mode = meta.permissions().mode();
            if mode & 0o700 != 0o700 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode | 0o700))?;
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        pending.push(entry.path());
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(path)
}

/// Put back what [`record_files`] saved: the empty directories, then each
/// mode (files before directories, deepest directory first, so a directory
/// made unwritable is made so last). What could not be applied, as
/// warnings.
pub fn reapply_files(workspace: &Utf8Path, saved: &FileModes) -> Vec<String> {
    let mut warnings = Vec::new();
    for dir in &saved.empty_dirs {
        let path = workspace.join(dir);
        if !no_symlink_on_the_way(workspace, Utf8Path::new(dir)) {
            warnings.push(format!(
                "empty directory `{dir}` was not made again: a symlink is on the way to it"
            ));
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&path) {
            warnings.push(format!(
                "empty directory `{dir}` could not be made again: {error}"
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Looked at with `lstat`, never through a symlink: a mode is put on
        // the file or directory the eviction saw, not on what a link points
        // at. Only the permission bits, whatever an older record says.
        let mut ordered: Vec<(&String, u32, bool)> = saved
            .modes
            .iter()
            .filter_map(|(path, mode)| {
                let full = workspace.join(path);
                let meta = std::fs::symlink_metadata(&full).ok();
                let usable = no_symlink_on_the_way(workspace, Utf8Path::new(path))
                    && meta
                        .as_ref()
                        .is_some_and(|meta| !meta.file_type().is_symlink());
                if !usable {
                    warnings.push(format!(
                        "`{path}` is not there as a file or directory to get its mode {:o} back",
                        mode & 0o777
                    ));
                    return None;
                }
                Some((path, mode & 0o777, meta.is_some_and(|meta| meta.is_dir())))
            })
            .collect();
        ordered.sort_by(|a, b| {
            a.2.cmp(&b.2)
                .then(b.0.matches('/').count().cmp(&a.0.matches('/').count()))
        });
        for (path, mode, _) in ordered {
            let full = workspace.join(path);
            if let Err(error) =
                std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode))
            {
                warnings.push(format!(
                    "`{path}` could not get its mode {mode:o} back: {error}"
                ));
            }
        }
    }
    warnings
}

/// What of the workspace's `.git/` an eviction does not keep, said before
/// it deletes anything: reflogs other than the stash's, intent-to-add
/// entries, and assume-unchanged or skip-worktree flags set by hand (their
/// files' content is kept; only the flag is lost). `rendered` are newgit's
/// own skip-worktree paths, which a restore sets again.
pub fn not_kept(workspace: &Utf8Path, rendered: &[Utf8PathBuf]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let logs = Utf8PathBuf::from(git(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-path", "logs"],
    )?);
    let mut reflogs = Vec::new();
    let mut pending = vec![logs.clone()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(name) = path.strip_prefix(&logs)
                && name != "refs/stash"
            {
                reflogs.push(name.to_string());
            }
        }
    }
    if !reflogs.is_empty() {
        reflogs.sort();
        out.push(format!(
            "the reflogs of {} ({}; the stash is kept)",
            if reflogs.len() == 1 {
                "one ref".to_owned()
            } else {
                format!("{} refs", reflogs.len())
            },
            reflogs
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
                + if reflogs.len() > 3 { ", …" } else { "" }
        ));
    }
    let status = git(
        workspace,
        &["status", "--porcelain=v2", "-z", "--untracked-files=no"],
    )?;
    let intent: Vec<String> = status
        .split('\0')
        .filter_map(|line| line.strip_prefix("1 .A "))
        .filter_map(|rest| rest.splitn(7, ' ').nth(6).map(ToOwned::to_owned))
        .collect();
    if !intent.is_empty() {
        out.push(format!(
            "intent-to-add (`git add -N`) on {} (the files are kept, untracked)",
            intent.join(", ")
        ));
    }
    let mut flagged = Vec::new();
    for entry in git(workspace, &["ls-files", "-z", "-v"])?.split('\0') {
        let Some((tag, path)) = entry.split_once(' ') else {
            continue;
        };
        let assumed = tag.chars().next().is_some_and(char::is_lowercase);
        let skipped = tag.eq_ignore_ascii_case("s");
        if (assumed || skipped)
            && !rendered.iter().any(|rendered| rendered == path)
            && std::fs::symlink_metadata(workspace.join(path)).is_ok()
        {
            flagged.push(format!(
                "`{path}` ({})",
                if skipped {
                    "skip-worktree"
                } else {
                    "assume-unchanged"
                }
            ));
        }
    }
    if !flagged.is_empty() {
        out.push(format!(
            "index flags set by hand on {} (the content is kept)",
            flagged.join(", ")
        ));
    }
    Ok(out)
}

/// The file in a workspace's `.git/` an eviction writes, with its
/// checkpoint id, just before it records the instance as evicted: while
/// the workspace is at its path with this in it, it is the original, never
/// moved.
pub fn evicting_flag(workspace: &Utf8Path) -> Utf8PathBuf {
    workspace.join(".git/newgit-evicting")
}

/// Exit here, as a crash would, when a debug build's
/// `NEWGIT_TEST_KILL_AT` names `point`: how the tests cut an eviction or a
/// restore short. Nothing in a release build.
pub fn kill_point(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("NEWGIT_TEST_KILL_AT").is_ok_and(|at| at == point) {
        std::process::exit(137);
    }
    let _ = point;
}

/// Wait here when a debug build's `NEWGIT_TEST_PAUSE_AT` names `point`:
/// write `<NEWGIT_TEST_PAUSE_DIR>/<point>.paused`, then go on once
/// `<point>.go` is there (or after a minute). How the tests write into a
/// workspace at an exact moment of a restore. Nothing in a release build.
pub fn pause_point(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("NEWGIT_TEST_PAUSE_AT").is_ok_and(|at| at == point)
        && let Ok(dir) = std::env::var("NEWGIT_TEST_PAUSE_DIR")
    {
        let dir = Utf8PathBuf::from(dir);
        let _ = std::fs::write(dir.join(format!("{point}.paused")), "");
        let go = dir.join(format!("{point}.go"));
        let started = std::time::Instant::now();
        while !go.exists() && started.elapsed().as_secs() < 60 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let _ = point;
}

/// Fail here when a debug build's `NEWGIT_TEST_FAIL_AT` names `point`: how
/// the tests make a late restore step fail. Nothing in a release build.
pub fn fail_point(point: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    if std::env::var("NEWGIT_TEST_FAIL_AT").is_ok_and(|at| at == point) {
        return Err(NewgitError::Unsupported(format!(
            "failing at `{point}`, as NEWGIT_TEST_FAIL_AT asks"
        )));
    }
    let _ = point;
    Ok(())
}

/// Where an evicted workspace's Git-only state waits in the store.
pub fn saved_namespace(slug: &str) -> String {
    format!("refs/newgit/evicted/{slug}")
}

/// What [`save_git_state`] put in the store, by name under
/// [`saved_namespace`]: `heads/*`, `tags/*`, `stash/<n>` (oldest first, with
/// its message), and `index` when anything was staged.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SavedGit {
    /// The stash, oldest entry first: `(message, commit)`.
    #[serde(default)]
    pub stash: Vec<(String, String)>,
    /// The paths whose staged content differs from `HEAD`, restored from
    /// the saved index commit.
    #[serde(default)]
    pub staged: Vec<String>,
    /// Remotes other than `origin`, whose remote-tracking refs were saved.
    #[serde(default)]
    pub remotes: Vec<String>,
}

/// Copy what of `workspace`'s Git no checkpoint carries into the store at
/// `store`, under [`saved_namespace`]: its local branches and tags, each
/// stash entry, and what is staged, as a commit of its index. Everything is
/// fetched by the store from temporary refs in the workspace, as a
/// checkpoint's source is.
pub fn save_git_state(
    workspace: &Utf8Path,
    store: &Utf8Path,
    slug: &str,
    state: &Utf8Path,
) -> Result<SavedGit> {
    let namespace = saved_namespace(slug);
    clear_saved(store, slug)?;
    let mut saved = SavedGit::default();
    save_config_and_hooks(workspace, state)?;
    saved.remotes = git(workspace, &["remote"])?
        .lines()
        .filter(|name| !name.is_empty() && *name != "origin")
        .map(ToOwned::to_owned)
        .collect();
    let mut temporary: Vec<String> = Vec::new();

    let stash = git(workspace, &["stash", "list", "--format=%H%x00%gs"])?;
    let entries: Vec<(String, String)> = stash
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .map(|(commit, message)| (message.to_owned(), commit.to_owned()))
        .rev()
        .collect();
    for (n, (_, commit)) in entries.iter().enumerate() {
        let name = format!("refs/newgit/evict/stash/{n}");
        git(workspace, &["update-ref", &name, commit])?;
        temporary.push(name);
    }
    saved.stash = entries;

    let staged = git(
        workspace,
        &["diff", "--cached", "--name-only", "-z", "--no-renames"],
    )?;
    saved.staged = staged
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if !saved.staged.is_empty() {
        let tree = git(workspace, &["write-tree"])?;
        let commit = run_git_env(
            &[
                "-C",
                workspace.as_str(),
                "commit-tree",
                &tree,
                "-m",
                "newgit: what was staged when this workspace was evicted",
            ],
            &[
                ("GIT_AUTHOR_NAME".to_owned(), "newgit".to_owned()),
                ("GIT_AUTHOR_EMAIL".to_owned(), "newgit@localhost".to_owned()),
                ("GIT_COMMITTER_NAME".to_owned(), "newgit".to_owned()),
                (
                    "GIT_COMMITTER_EMAIL".to_owned(),
                    "newgit@localhost".to_owned(),
                ),
            ],
        )?;
        git(
            workspace,
            &["update-ref", "refs/newgit/evict/index", &commit],
        )?;
        temporary.push("refs/newgit/evict/index".to_owned());
    }

    let mut refspecs = vec![
        format!("+refs/heads/*:{namespace}/heads/*"),
        format!("+refs/tags/*:{namespace}/tags/*"),
        format!("+refs/newgit/evict/*:{namespace}/*"),
        format!("+refs/notes/*:{namespace}/notes/*"),
    ];
    refspecs.extend(
        saved
            .remotes
            .iter()
            .map(|remote| format!("+refs/remotes/{remote}/*:{namespace}/remotes/{remote}/*")),
    );
    let mut args = vec![
        "-C",
        store.as_str(),
        "fetch",
        "--quiet",
        "--no-write-fetch-head",
        "--no-tags",
        "--no-prune",
        "--no-recurse-submodules",
        "--",
        workspace.as_str(),
    ];
    args.extend(refspecs.iter().map(String::as_str));
    let fetched = run_git_env(&args, &[]);
    for name in &temporary {
        let _ = git(workspace, &["update-ref", "-d", name]);
    }
    fetched?;
    Ok(saved)
}

/// Put back what [`save_git_state`] saved, into the workspace the restore
/// just made: the branches (the one checked out included, which the undo
/// has already put where it was), tags, stash, and what was staged.
pub fn restore_git_state(
    workspace: &Utf8Path,
    store: &Utf8Path,
    slug: &str,
    saved: &SavedGit,
    state: &Utf8Path,
) -> Result<Vec<String>> {
    let namespace = saved_namespace(slug);
    let mut refspecs = vec![
        format!("+{namespace}/heads/*:refs/heads/*"),
        format!("+{namespace}/tags/*:refs/tags/*"),
        format!("+{namespace}/stash/*:refs/newgit/evict/stash/*"),
        format!("+{namespace}/notes/*:refs/notes/*"),
        format!("+{namespace}/remotes/*:refs/remotes/*"),
    ];
    if !saved.staged.is_empty() {
        refspecs.push(format!("+{namespace}/index:refs/newgit/evict/index"));
    }
    let mut args = vec![
        "-C",
        workspace.as_str(),
        "fetch",
        "--quiet",
        "--no-write-fetch-head",
        "--no-tags",
        "--no-prune",
        "--no-recurse-submodules",
        // The branch checked out is among them, already where the undo put
        // it.
        "--update-head-ok",
        "--",
        store.as_str(),
    ];
    args.extend(refspecs.iter().map(String::as_str));
    run_git_env(&args, &[])?;
    // `stash store` pushes onto the top, so oldest first.
    for (message, commit) in &saved.stash {
        git(workspace, &["stash", "store", "-m", message, commit])?;
    }
    if !saved.staged.is_empty() {
        let mut args = vec![
            "restore",
            "--staged",
            "--source=refs/newgit/evict/index",
            "--",
        ];
        args.extend(saved.staged.iter().map(String::as_str));
        git(workspace, &args)?;
    }
    let temporary = git(
        workspace,
        &["for-each-ref", "--format=%(refname)", "refs/newgit/evict/"],
    )?;
    for name in temporary.lines().filter(|name| !name.is_empty()) {
        git(workspace, &["update-ref", "-d", name])?;
    }
    replay_config_and_hooks(workspace, state)
}

/// Keep the workspace's `.git/config` and its hooks (all but Git's
/// `*.sample` files) in `state`, for [`replay_config_and_hooks`].
fn save_config_and_hooks(workspace: &Utf8Path, state: &Utf8Path) -> Result<()> {
    let config = git_path(workspace, "config")?;
    if let Ok(text) = std::fs::read_to_string(&config) {
        crate::remote::write_private(&state.join("config"), &text)?;
    }
    let hooks = git_path(workspace, "hooks")?;
    let saved = state.join("hooks");
    if saved.exists() {
        std::fs::remove_dir_all(&saved).map_err(|source| NewgitError::io(&saved, source))?;
    }
    let Ok(entries) = std::fs::read_dir(&hooks) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if name.ends_with(".sample") || !entry.path().is_file() {
            continue;
        }
        std::fs::create_dir_all(&saved).map_err(|source| NewgitError::io(&saved, source))?;
        std::fs::copy(entry.path(), saved.join(&name))
            .map_err(|source| NewgitError::io(saved.join(&name), source))?;
    }
    Ok(())
}

fn git_path(workspace: &Utf8Path, name: &str) -> Result<Utf8PathBuf> {
    Ok(Utf8PathBuf::from(git(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )?))
}

/// `key → values` of a config listing, in order.
fn config_entries(listing: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for entry in listing.split('\0').filter(|entry| !entry.is_empty()) {
        let (key, value) = entry
            .split_once('\n')
            .map_or((entry, "true"), |(key, value)| (key, value));
        match out.iter_mut().find(|(have, _)| have == key) {
            Some((_, values)) => values.push(value.to_owned()),
            None => out.push((key.to_owned(), vec![value.to_owned()])),
        }
    }
    out
}

/// Whether newgit owns config `key` in a workspace, setting it again on
/// every restore: `origin`'s URLs and receive-pack, the credential helper,
/// and the remotes newgit makes for export targets (their `receivepack`
/// runs `newgit push-receive-pack`). `new` is the config newgit wrote.
fn newgit_owns(key: &str, new: &[(String, Vec<String>)]) -> bool {
    if key.starts_with("credential.") {
        return true;
    }
    let Some(rest) = key.strip_prefix("remote.") else {
        return false;
    };
    let Some((remote, name)) = rest.rsplit_once('.') else {
        return false;
    };
    if !["url", "pushurl", "receivepack", "gh-resolved"].contains(&name) {
        return false;
    }
    remote == "origin"
        || new.iter().any(|(have, values)| {
            *have == format!("remote.{remote}.receivepack")
                && values
                    .iter()
                    .any(|value| value.contains("push-receive-pack"))
        })
}

/// How a saved config goes back over the one a restore's new clone has.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfigPlan {
    /// Keys set to exactly the saved values, replacing the new clone's.
    pub replace: Vec<(String, Vec<String>)>,
    /// Values added to a key, beside the new clone's.
    pub add: Vec<(String, Vec<String>)>,
    /// What does not come back, for the not-kept list.
    pub dropped: Vec<String>,
}

/// [`ConfigPlan`] for the saved config `old` over the new clone's `new`.
///
/// A key the new clone lacks comes back as it was. One newgit owns
/// ([`newgit_owns`]) keeps newgit's values, and a value set by hand on it
/// is dropped and named. `branch.*` is the user's (upstreams,
/// descriptions) and comes back as it was. Any other key the new clone
/// has with one value, saved with one value, takes the saved one
/// (`core.autocrlf`); otherwise it is multi-valued (`remote.origin.fetch`),
/// and the saved values the new clone lacks are added, and a value the user
/// had removed, which comes back, is named.
pub fn plan_config(old: &[(String, Vec<String>)], new: &[(String, Vec<String>)]) -> ConfigPlan {
    let mut plan = ConfigPlan::default();
    for (key, values) in old {
        let Some(have) = new.iter().find(|(name, _)| name == key).map(|(_, v)| v) else {
            plan.add.push((key.clone(), values.clone()));
            continue;
        };
        if have == values {
            continue;
        }
        if key.starts_with("branch.") {
            plan.replace.push((key.clone(), values.clone()));
        } else if newgit_owns(key, new) {
            if values.iter().any(|value| !have.contains(value)) {
                plan.dropped.push(format!(
                    "a value set by hand on config `{key}`, which newgit sets again"
                ));
            }
        } else if values.len() == 1 && have.len() == 1 {
            plan.replace.push((key.clone(), values.clone()));
        } else {
            let extra: Vec<String> = values
                .iter()
                .filter(|value| !have.contains(value))
                .cloned()
                .collect();
            if !extra.is_empty() {
                plan.add.push((key.clone(), extra));
            }
            if have.iter().any(|value| !values.contains(value)) {
                plan.dropped.push(format!(
                    "the removal of a value of config `{key}`: newgit's default value comes back \
                     beside yours"
                ));
            }
        }
    }
    plan
}

/// `key → values` of a config listing (`git config -z --list`), in order.
pub fn parse_config(listing: &str) -> Vec<(String, Vec<String>)> {
    config_entries(listing)
}

/// The workspace's own config (`.git/config`), listed.
pub fn workspace_config(workspace: &Utf8Path) -> Result<Vec<(String, Vec<String>)>> {
    Ok(config_entries(&git(
        workspace,
        &["config", "-z", "--local", "--list"],
    )?))
}

/// Put back what [`save_config_and_hooks`] kept, into the workspace the
/// restore just made, as [`plan_config`] says. A saved hook replaces what
/// the new workspace has under its name. What could not be replayed, and
/// what did not come back, as warnings.
fn replay_config_and_hooks(workspace: &Utf8Path, state: &Utf8Path) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let saved = state.join("config");
    if saved.exists() {
        let old = config_entries(&git(
            workspace,
            &["config", "-z", "--file", saved.as_str(), "--list"],
        )?);
        let new = workspace_config(workspace)?;
        let plan = plan_config(&old, &new);
        for (key, values) in &plan.replace {
            let _ = git(workspace, &["config", "--local", "--unset-all", key]);
            for value in values {
                if let Err(error) = git(workspace, &["config", "--local", "--add", key, value]) {
                    warnings.push(format!("config `{key}` could not be set again: {error}"));
                }
            }
        }
        for (key, values) in &plan.add {
            for value in values {
                if let Err(error) = git(workspace, &["config", "--local", "--add", key, value]) {
                    warnings.push(format!("config `{key}` could not be set again: {error}"));
                }
            }
        }
        warnings.extend(
            plan.dropped
                .into_iter()
                .map(|what| format!("not kept by the eviction: {what}")),
        );
    }
    let hooks = state.join("hooks");
    if let Ok(entries) = std::fs::read_dir(&hooks) {
        let target = git_path(workspace, "hooks")?;
        std::fs::create_dir_all(&target).map_err(|source| NewgitError::io(&target, source))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let to = target.join(name.to_string_lossy().as_ref());
            if std::fs::read(&to).ok() == std::fs::read(entry.path()).ok() {
                continue;
            }
            if let Err(error) = std::fs::copy(entry.path(), &to) {
                warnings.push(format!("hook {to} could not be put back: {error}"));
            }
        }
    }
    Ok(warnings)
}

/// Delete what [`save_git_state`] kept in the store for `slug`.
pub fn clear_saved(store: &Utf8Path, slug: &str) -> Result<()> {
    let listed = run_git_env(
        &[
            "-C",
            store.as_str(),
            "for-each-ref",
            "--format=%(refname)",
            &format!("{}/", saved_namespace(slug)),
        ],
        &[],
    )?;
    for name in listed.lines().filter(|name| !name.is_empty()) {
        run_git_env(&["-C", store.as_str(), "update-ref", "-d", name], &[])?;
    }
    Ok(())
}

/// A process, as `(pid, command, working directory)`.
pub type Process = (u32, String, Utf8PathBuf);

/// The processes whose working directory is `dir` or under it, apart from
/// this one: `(others, ancestors)`, each `(pid, command)`. `others` are a
/// shell or an agent still sitting in the workspace; `ancestors` are the
/// ones that started this command (the shell it was typed in), whose
/// directory would be deleted under them just the same.
pub fn processes_in(dir: &Utf8Path) -> Result<(Vec<Process>, Vec<Process>)> {
    let canonical = dir
        .canonicalize_utf8()
        .unwrap_or_else(|_| dir.to_path_buf());
    let inside = |cwd: &Utf8Path| cwd.starts_with(&canonical) || cwd.starts_with(dir);
    let chain = ancestors();
    let me = std::process::id();
    let (mut others, mut above) = (Vec::new(), Vec::new());
    for (pid, command, cwd) in cwds()? {
        if pid == me || !inside(&cwd) {
            continue;
        }
        if chain.contains(&pid) {
            above.push((pid, command, cwd));
        } else {
            others.push((pid, command, cwd));
        }
    }
    for list in [&mut others, &mut above] {
        list.sort();
        list.dedup();
    }
    Ok((others, above))
}

/// The processes above this one: its parent, its parent's parent, ….
fn ancestors() -> Vec<u32> {
    let mut parents = std::collections::BTreeMap::new();
    if let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .current_dir("/")
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            if let (Some(Ok(pid)), Some(Ok(ppid))) = (
                fields.next().map(str::parse::<u32>),
                fields.next().map(str::parse::<u32>),
            ) {
                parents.insert(pid, ppid);
            }
        }
    }
    let mut chain = vec![std::process::id()];
    while let Some(parent) = chain.last().and_then(|pid| parents.get(pid)).copied() {
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
    }
    chain.remove(0);
    chain
}

/// Every process this user may look at, with its command and working
/// directory.
#[cfg(target_os = "linux")]
fn cwds() -> Result<Vec<(u32, String, Utf8PathBuf)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir("/proc").map_err(|source| NewgitError::io("/proc", source))?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
            continue;
        };
        let Ok(cwd) = Utf8PathBuf::from_path_buf(cwd) else {
            continue;
        };
        let command = std::fs::read_to_string(entry.path().join("comm"))
            .map(|text| text.trim().to_owned())
            .unwrap_or_default();
        out.push((pid, command, cwd));
    }
    Ok(out)
}

/// How long `lsof` may take before the eviction gives up on it.
#[cfg(not(target_os = "linux"))]
fn lsof_timeout() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("NEWGIT_TEST_LSOF_TIMEOUT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::from_secs(20)
}

#[cfg(not(target_os = "linux"))]
fn cwds() -> Result<Vec<(u32, String, Utf8PathBuf)>> {
    use std::io::Read as _;
    // Run from `/`: started in the workspace, `lsof` would list itself.
    // `-b` avoids the kernel calls that can block (`stat` and friends on a
    // dead network mount); the timeout covers whatever still does, since
    // this runs under the instance's lock.
    let mut child = std::process::Command::new("lsof")
        .args(["-bnPw", "-d", "cwd", "-F", "pcn"])
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            NewgitError::Unsupported(format!(
                "could not run `lsof` to find the processes still in the workspace: {error}"
            ))
        })?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let reading = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let reading_err = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stderr.read_to_end(&mut out);
        out
    });
    let limit = lsof_timeout();
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NewgitError::Unsupported(format!(
                    "not evicting: `lsof`, which finds the processes still in the workspace, \
                     did not answer within {} s (a network mount that does not respond can \
                     do that), so newgit cannot tell whether anything is still there. \
                     Nothing was deleted; unmount what hangs, or try again",
                    limit.as_secs_f32()
                )));
            }
            Err(error) => {
                return Err(NewgitError::Unsupported(format!(
                    "could not wait for `lsof`: {error}"
                )));
            }
        }
    };
    let output = std::process::Output {
        status,
        stdout: reading.join().unwrap_or_default(),
        stderr: reading_err.join().unwrap_or_default(),
    };
    // lsof exits 1 when some process could not be read, which is not this
    // user's; what it did list is still right.
    if !output.status.success() && output.stdout.is_empty() {
        return Err(NewgitError::Unsupported(format!(
            "`lsof -d cwd` exited with {} and listed nothing, so newgit cannot tell which \
             processes are still in the workspace: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut out = Vec::new();
    let (mut pid, mut command) = (None, String::new());
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = value.parse().ok();
                command.clear();
            }
            "c" => command = value.to_owned(),
            "n" => {
                if let Some(pid) = pid {
                    out.push((pid, command.clone(), Utf8PathBuf::from(value)));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}
