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

/// What `git ls-files --others` lists in `workspace`, `-z` split. A
/// directory holding a repository of its own is listed once, with a
/// trailing `/`, and not walked.
fn others(workspace: &Utf8Path, ignored: bool) -> Result<Vec<String>> {
    let mut args = vec!["ls-files", "-z", "--others", "--exclude-standard"];
    if ignored {
        args.push("--ignored");
    }
    Ok(git(workspace, &args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Every ignored file in `workspace` (a repository inside an ignored
/// directory named as that directory), workspace-relative.
pub fn ignored_files(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(others(workspace, true)?
        .into_iter()
        .map(|path| Utf8PathBuf::from(path.trim_end_matches('/')))
        .collect())
}

/// Untracked directories that are repositories of their own (a `git clone`
/// inside the workspace). A checkpoint's `git add -A` records one as a
/// commit id, not its files, so it would not come back.
pub fn untracked_repositories(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(others(workspace, false)?
        .into_iter()
        .filter(|path| path.ends_with('/'))
        .map(|path| Utf8PathBuf::from(path.trim_end_matches('/')))
        .collect())
}

/// The ignored entries of `workspace` as `git ls-files --directory` names
/// them: a directory wholly ignored is one entry.
pub fn ignored_entries(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    Ok(git(
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
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(|path| Utf8PathBuf::from(path.trim_end_matches('/')))
    .collect())
}

/// `files`, the paths an eviction would lose, named as briefly as is still
/// exact: a directory `git ls-files --directory` lists whole when every
/// file under it is lost, each file otherwise.
pub fn summarize(workspace: &Utf8Path, files: &[Utf8PathBuf]) -> Result<Vec<Utf8PathBuf>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let all = ignored_files(workspace)?;
    let mut out = Vec::new();
    let mut covered: Vec<&Utf8PathBuf> = Vec::new();
    for entry in ignored_entries(workspace)? {
        let under: Vec<&Utf8PathBuf> = all.iter().filter(|file| file.starts_with(&entry)).collect();
        if !under.is_empty() && under.iter().all(|file| files.contains(file)) {
            covered.extend(under);
            out.push(entry);
        }
    }
    for file in files {
        if !covered.contains(&file) {
            out.push(file.clone());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
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
}

/// Copy what of `workspace`'s Git no checkpoint carries into the store at
/// `store`, under [`saved_namespace`]: its local branches and tags, each
/// stash entry, and what is staged, as a commit of its index. Everything is
/// fetched by the store from temporary refs in the workspace, as a
/// checkpoint's source is.
pub fn save_git_state(workspace: &Utf8Path, store: &Utf8Path, slug: &str) -> Result<SavedGit> {
    let namespace = saved_namespace(slug);
    clear_saved(store, slug)?;
    let mut saved = SavedGit::default();
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

    let fetched = run_git_env(
        &[
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
            &format!("+refs/heads/*:{namespace}/heads/*"),
            &format!("+refs/tags/*:{namespace}/tags/*"),
            &format!("+refs/newgit/evict/*:{namespace}/*"),
        ],
        &[],
    );
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
) -> Result<()> {
    let namespace = saved_namespace(slug);
    let mut refspecs = vec![
        format!("+{namespace}/heads/*:refs/heads/*"),
        format!("+{namespace}/tags/*:refs/tags/*"),
        format!("+{namespace}/stash/*:refs/newgit/evict/stash/*"),
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
    Ok(())
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

/// The processes, other than this one and the ones that started it, whose
/// working directory is `dir` or under it: a shell or an agent still
/// sitting in the workspace, which would go on writing into a directory
/// that is gone. `(pid, command)`.
pub fn processes_in(dir: &Utf8Path) -> Result<Vec<(u32, String)>> {
    let canonical = dir
        .canonicalize_utf8()
        .unwrap_or_else(|_| dir.to_path_buf());
    let inside = |cwd: &Utf8Path| cwd.starts_with(&canonical) || cwd.starts_with(dir);
    let spared = ancestors();
    let mut found: Vec<(u32, String)> = cwds()?
        .into_iter()
        .filter(|(pid, _, cwd)| !spared.contains(pid) && inside(cwd))
        .map(|(pid, command, _)| (pid, command))
        .collect();
    found.sort();
    found.dedup();
    Ok(found)
}

/// This process and every one above it.
fn ancestors() -> Vec<u32> {
    let mut parents = std::collections::BTreeMap::new();
    if let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
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

#[cfg(not(target_os = "linux"))]
fn cwds() -> Result<Vec<(u32, String, Utf8PathBuf)>> {
    let output = std::process::Command::new("lsof")
        .args(["-nPw", "-d", "cwd", "-F", "pcn"])
        .output()
        .map_err(|error| {
            NewgitError::Unsupported(format!(
                "could not run `lsof` to find the processes still in the workspace: {error}"
            ))
        })?;
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
