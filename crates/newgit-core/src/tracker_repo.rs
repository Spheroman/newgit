use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::tracker::{TrackerDefinition, collect_all_files, collect_owned_files};

/// Length of the rev string newgit shows and records: a prefix of the
/// commit id, the way Git abbreviates.
const REV_LEN: usize = 12;

/// Who a commit says made it, when Git has no identity configured. With one
/// configured, captures carry the developer's own name, like any commit.
const FALLBACK_IDENT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "newgit"),
    ("GIT_AUTHOR_EMAIL", "newgit@localhost"),
    ("GIT_COMMITTER_NAME", "newgit"),
    ("GIT_COMMITTER_EMAIL", "newgit@localhost"),
];

/// A tracker's repository: its history, kept in the store's tracker repo
/// (`.newgit/local/trackers.git`) under `refs/trackers/<tracker>/`.
///
/// - `main` — what new instances get, and where merges land.
/// - `instances/<instance>` — one branch per instance that changed it.
/// - `pins/<rev>` — keeps a commit alive while a binding or checkpoint names
///   it, even after a branch has moved past it.
///
/// A rev is a commit (shown as its 12-hex prefix) with a parent, a message,
/// and an author, like any commit. Capturing content identical to the
/// parent's makes no commit: the parent is the answer.
///
/// The store is separate from the project's own repository on purpose:
/// tracker content includes secrets, and a project repository is what a
/// `git push --mirror` publishes and a local `git clone` hardlinks wholesale
/// into every workspace.
///
/// Checkpoints reference revs by name, so cleanup never removes the pin of a
/// rev any checkpoint record still points at.
#[derive(Debug, Clone)]
pub struct TrackerRepo {
    git_dir: Utf8PathBuf,
    checkouts: Utf8PathBuf,
    tracker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub rev: String,
    pub files: usize,
    /// False when the content equalled the parent's, so no commit was made.
    pub committed: bool,
}

/// Where a capture lands: on top of `parent` (none for a first capture), on
/// `branch`, with `message`.
#[derive(Debug, Clone, Copy)]
pub struct CaptureAt<'a> {
    pub parent: Option<&'a str>,
    pub branch: Branch<'a>,
    pub message: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch<'a> {
    /// The tracker's `main`: what seeding from the store writes.
    Main,
    /// One instance's branch, by slug.
    Instance(&'a str),
}

/// A pinned rev present in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRev {
    pub tracker: String,
    pub rev: String,
}

impl TrackerRepo {
    /// `git_dir` is the store; `checkouts` is the cache directory revs are
    /// materialized into when a command needs a path.
    pub fn new(git_dir: &Utf8Path, checkouts: &Utf8Path, tracker: &str) -> Self {
        Self {
            git_dir: git_dir.to_path_buf(),
            checkouts: checkouts.join(tracker),
            tracker: tracker.to_owned(),
        }
    }

    /// The tip of `main`, or `None` before anything was merged or seeded.
    pub fn main(&self) -> Option<String> {
        self.resolve(&self.main_ref()).map(|commit| short(&commit))
    }

    /// Move `main` to `rev` — what merging a tracker does for now.
    pub fn set_main(&self, rev: &str) -> Result<()> {
        let commit = self.commit_for(rev)?;
        git(&self.git_dir, &["update-ref", &self.main_ref(), &commit]).map(drop)
    }

    pub fn has_rev(&self, rev: &str) -> bool {
        self.resolve(&self.pin_ref(rev)).is_some()
    }

    /// The tree a rev holds: what "same content" compares, now that equal
    /// content on two branches is two commits.
    pub fn tree_of(&self, rev: &str) -> Result<String> {
        let commit = self.commit_for(rev)?;
        git(&self.git_dir, &["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// Commit the tracker's owned paths from `workspace`.
    pub fn capture(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
        at: CaptureAt<'_>,
    ) -> Result<CaptureOutcome> {
        if definition.paths.is_empty() {
            return Err(NewgitError::TrackerHasNoPaths(self.tracker.clone()));
        }
        let files = collect_owned_files(workspace, definition)?;
        self.commit_files(&files, at)
    }

    /// Commit an arbitrary directory's contents — how a resource checkpoint
    /// deposits into a deposit-only tracker (one with no owned paths).
    pub fn deposit(&self, dir: &Utf8Path, at: CaptureAt<'_>) -> Result<CaptureOutcome> {
        let files = collect_all_files(dir)?;
        self.commit_files(&files, at)
    }

    fn commit_files(
        &self,
        files: &[(Utf8PathBuf, Utf8PathBuf)],
        at: CaptureAt<'_>,
    ) -> Result<CaptureOutcome> {
        ensure_repo(&self.git_dir)?;

        // Blobs: hashed straight from the files, bytes exactly as they are
        // (`--no-filters`), following symlinks as the file walk does.
        let mut paths = String::new();
        for (_, absolute) in files {
            paths.push_str(absolute.as_str());
            paths.push('\n');
        }
        let blobs = if files.is_empty() {
            String::new()
        } else {
            git_stdin(
                &self.git_dir,
                &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                &[],
                paths.as_bytes(),
            )?
        };

        // Tree: through a throwaway index, so nothing here touches a
        // working tree or an index anyone else is using.
        let mut entries = Vec::new();
        for ((relative, absolute), blob) in files.iter().zip(blobs.lines()) {
            entries.extend_from_slice(
                format!("{} {blob}\t{relative}\0", file_mode(absolute)?).as_bytes(),
            );
        }
        let index = TempIndex::new(&self.git_dir);
        git_stdin(
            &self.git_dir,
            &["update-index", "-z", "--index-info"],
            &index.env(),
            &entries,
        )?;
        let tree = git_env(&self.git_dir, &["write-tree"], &index.env())?;

        let parent = at.parent.map(|rev| self.commit_for(rev)).transpose()?;
        let unchanged = match &parent {
            Some(parent) => {
                git(&self.git_dir, &["rev-parse", &format!("{parent}^{{tree}}")])? == tree
            }
            None => false,
        };
        let commit = match (unchanged, parent) {
            (true, Some(parent)) => parent,
            (_, parent) => {
                let mut args = vec![
                    "-c",
                    "commit.gpgSign=false",
                    "commit-tree",
                    tree.as_str(),
                    "-m",
                    at.message,
                ];
                if let Some(parent) = &parent {
                    args.extend(["-p", parent.as_str()]);
                }
                git_env(&self.git_dir, &args, &identity(&self.git_dir))?
            }
        };
        let rev = short(&commit);
        git(&self.git_dir, &["update-ref", &self.pin_ref(&rev), &commit])?;
        let branch = match at.branch {
            Branch::Main => self.main_ref(),
            Branch::Instance(slug) => self.instance_ref(slug),
        };
        git(&self.git_dir, &["update-ref", &branch, &commit])?;

        Ok(CaptureOutcome {
            rev,
            files: files.len(),
            committed: !unchanged,
        })
    }

    /// Drop an instance's branch — when the instance is removed. Pins keep
    /// whatever its checkpoints still name.
    pub fn drop_instance(&self, slug: &str) -> Result<()> {
        if self.resolve(&self.instance_ref(slug)).is_none() {
            return Ok(());
        }
        git(
            &self.git_dir,
            &["update-ref", "-d", &self.instance_ref(slug)],
        )
        .map(drop)
    }

    /// A directory holding a captured rev's files, for commands that take a
    /// path (a restore handed `{{state_ref}}`). Materialized on first use
    /// into a cache that is safe to delete at any time.
    pub fn checkout_dir(&self, rev: &str) -> Result<Utf8PathBuf> {
        let dir = self.checkouts.join(rev);
        if dir.is_dir() {
            return Ok(dir);
        }
        let commit = self.commit_for(rev)?;
        let staging = self.checkouts.join(format!("{rev}.tmp"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)
                .map_err(|source| NewgitError::io(&staging, source))?;
        }
        create_dir_all(&staging)?;
        let files = self.tree_files(&commit, &[])?;
        self.check_out(&commit, &staging, &files)?;
        std::fs::rename(&staging, &dir).map_err(|source| NewgitError::io(&dir, source))?;
        Ok(dir)
    }

    /// Put a captured rev back into the workspace: owned paths are cleared
    /// first, so restore reproduces the captured state exactly (including
    /// file absence).
    pub fn restore(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
        rev: &str,
    ) -> Result<usize> {
        let commit = self.commit_for(rev)?;
        // A deposit-only tracker owns no paths: its content reaches resources
        // through `{{state_ref}}`, never the workspace. (An empty path list
        // means "the whole tree" to Git, so this has to be said explicitly.)
        if definition.paths.is_empty() {
            return Ok(0);
        }

        clear_owned_paths(workspace, definition)?;

        // Only what the definition owns *now*: a rev captured before a path
        // was untracked — or before a more specific tracker took part of it —
        // must not write that path back.
        let mut files = self.tree_files(&commit, &definition.paths)?;
        files.retain(|file| definition.owns(Utf8Path::new(file)));
        self.check_out(&commit, workspace, &files)?;
        Ok(files.len())
    }

    /// Paths in a rev's tree, limited to `paths` when it is non-empty.
    fn tree_files(&self, commit: &str, paths: &[Utf8PathBuf]) -> Result<Vec<String>> {
        let mut args = vec!["ls-tree", "-r", "-z", "--name-only", commit, "--"];
        args.extend(paths.iter().map(|path| path.as_str()));
        let listing = git(&self.git_dir, &args)?;
        Ok(listing
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    fn check_out(&self, commit: &str, target: &Utf8Path, files: &[String]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let index = TempIndex::new(&self.git_dir);
        git_env(&self.git_dir, &["read-tree", commit], &index.env())?;
        let mut stdin = Vec::new();
        for file in files {
            stdin.extend_from_slice(file.as_bytes());
            stdin.push(0);
        }
        let work_tree = format!("--work-tree={target}");
        git_stdin(
            &self.git_dir,
            &[&work_tree, "checkout-index", "-f", "-z", "--stdin"],
            &index.env(),
            &stdin,
        )
        .map(drop)
    }

    fn commit_for(&self, rev: &str) -> Result<String> {
        self.resolve(&self.pin_ref(rev))
            .ok_or_else(|| NewgitError::NoSnapshot {
                tracker: self.tracker.clone(),
                rev: rev.to_owned(),
            })
    }

    fn resolve(&self, name: &str) -> Option<String> {
        if !self.git_dir.join("HEAD").is_file() {
            return None;
        }
        git(
            &self.git_dir,
            &["rev-parse", "-q", "--verify", &format!("{name}^{{commit}}")],
        )
        .ok()
    }

    fn main_ref(&self) -> String {
        format!("refs/trackers/{}/main", self.tracker)
    }

    fn instance_ref(&self, slug: &str) -> String {
        format!("refs/trackers/{}/instances/{slug}", self.tracker)
    }

    fn pin_ref(&self, rev: &str) -> String {
        format!("refs/trackers/{}/pins/{rev}", self.tracker)
    }

    /// One file's content at a captured rev, or `None` when the rev does not
    /// have it. Exact bytes: this is what a render reverses against.
    pub fn read_file(&self, rev: &str, path: &Utf8Path) -> Result<Option<String>> {
        let commit = self.commit_for(rev)?;
        let object = format!("{commit}:{path}");
        if self.resolve_object(&object).is_none() {
            return Ok(None);
        }
        let args = ["cat-file", "blob", object.as_str()];
        let bytes = run(
            &mut command(&self.git_dir, &args, &[]),
            &describe(&args),
            None,
        )?;
        String::from_utf8(bytes).map(Some).map_err(|error| {
            NewgitError::io(
                format!("{}@{rev}:{path}", self.tracker),
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
    }

    fn resolve_object(&self, spec: &str) -> Option<String> {
        git(&self.git_dir, &["rev-parse", "-q", "--verify", spec]).ok()
    }

    /// Forget a rev: drop its pin and its materialized copy. Its objects go
    /// at the next [`prune_unreferenced`] unless a branch still reaches them.
    pub fn forget(&self, rev: &str) -> Result<()> {
        git(&self.git_dir, &["update-ref", "-d", &self.pin_ref(rev)])?;
        let dir = self.checkouts.join(rev);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| NewgitError::io(&dir, source))?;
        }
        Ok(())
    }
}

/// Every tracker with anything in the store, sorted.
pub fn tracker_names(git_dir: &Utf8Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = tracker_refs(git_dir)?
        .into_iter()
        .map(|(tracker, _)| tracker)
        .collect();
    names.dedup();
    Ok(names)
}

/// Every pinned rev of every tracker, sorted by tracker then rev.
pub fn pinned_revs(git_dir: &Utf8Path) -> Result<Vec<PinnedRev>> {
    Ok(tracker_refs(git_dir)?
        .into_iter()
        .filter_map(|(tracker, rest)| {
            rest.strip_prefix("pins/").map(|rev| PinnedRev {
                tracker,
                rev: rev.to_owned(),
            })
        })
        .collect())
}

/// Delete objects no ref reaches any more. Tracker content can be secrets, so
/// a forgotten rev is removed now rather than whenever Git's own gc would
/// have got to it.
pub fn prune_unreferenced(git_dir: &Utf8Path) -> Result<()> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    git(git_dir, &["reflog", "expire", "--expire=now", "--all"])?;
    git(git_dir, &["gc", "--quiet", "--prune=now"]).map(drop)
}

/// `tracker:<name>@<rev>` — the state ref an `into_tracker` deposit records.
pub fn parse_tracker_state_ref(state_ref: &str) -> Option<(&str, &str)> {
    state_ref.strip_prefix("tracker:")?.split_once('@')
}

/// `(tracker, rest)` for every ref under `refs/trackers/`, sorted.
fn tracker_refs(git_dir: &Utf8Path) -> Result<Vec<(String, String)>> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let listing = git(
        git_dir,
        &["for-each-ref", "--format=%(refname)", "refs/trackers/"],
    )?;
    let mut refs: Vec<(String, String)> = listing
        .lines()
        .filter_map(|name| {
            let (tracker, rest) = name.strip_prefix("refs/trackers/")?.split_once('/')?;
            Some((tracker.to_owned(), rest.to_owned()))
        })
        .collect();
    refs.sort();
    Ok(refs)
}

/// Give a tracker a history from another repository: fetch `source_ref`
/// from `source` into the tracker's `main`. A real copy, never borrowed
/// objects: Git's own cleanup in either repository deletes what *that*
/// repository does not reference, so each must hold what it needs. How
/// `newgit init` hands root the project's history.
pub fn import_history(
    git_dir: &Utf8Path,
    source: &Utf8Path,
    source_ref: &str,
    tracker: &str,
) -> Result<()> {
    ensure_repo(git_dir)?;
    git(
        git_dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            source.as_str(),
            &format!("+{source_ref}:refs/trackers/{tracker}/main"),
        ],
    )
    .map(drop)
}

/// Create the store if this machine has none yet.
pub fn ensure_store(git_dir: &Utf8Path) -> Result<()> {
    ensure_repo(git_dir)
}

fn ensure_repo(git_dir: &Utf8Path) -> Result<()> {
    if git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    create_dir_all(git_dir)?;
    run(
        Command::new("git").args(["init", "-q", "--bare", git_dir.as_str()]),
        &format!("git init --bare {git_dir}"),
        None,
    )
    .map(drop)
}

/// The developer's own Git identity when they have one; newgit's otherwise,
/// where Git would refuse to commit at all.
fn identity(git_dir: &Utf8Path) -> Vec<(String, String)> {
    if git(git_dir, &["var", "GIT_COMMITTER_IDENT"]).is_ok() {
        return Vec::new();
    }
    FALLBACK_IDENT
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn short(commit: &str) -> String {
    commit[..REV_LEN.min(commit.len())].to_owned()
}

#[cfg(unix)]
fn file_mode(path: &Utf8Path) -> Result<&'static str> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(path).map_err(|source| NewgitError::io(path, source))?;
    Ok(if metadata.permissions().mode() & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    })
}

#[cfg(not(unix))]
fn file_mode(_path: &Utf8Path) -> Result<&'static str> {
    Ok("100644")
}

/// An index file of our own, removed on drop.
struct TempIndex {
    path: Utf8PathBuf,
}

impl TempIndex {
    fn new(git_dir: &Utf8Path) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = git_dir.join(format!("newgit-index-{}-{nanos}", std::process::id()));
        Self { path }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("GIT_INDEX_FILE".to_owned(), self.path.to_string())]
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn git(git_dir: &Utf8Path, args: &[&str]) -> Result<String> {
    git_env(git_dir, args, &[])
}

fn git_env(git_dir: &Utf8Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    run(&mut command(git_dir, args, env), &describe(args), None).map(|out| trimmed(&out))
}

fn git_stdin(
    git_dir: &Utf8Path,
    args: &[&str],
    env: &[(String, String)],
    stdin: &[u8],
) -> Result<String> {
    run(
        &mut command(git_dir, args, env),
        &describe(args),
        Some(stdin),
    )
    .map(|out| trimmed(&out))
}

fn trimmed(output: &[u8]) -> String {
    String::from_utf8_lossy(output).trim().to_owned()
}

fn command(git_dir: &Utf8Path, args: &[&str], env: &[(String, String)]) -> Command {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={git_dir}"))
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)));
    command
}

fn describe(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

fn run(command: &mut Command, description: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let spawn_error = |source: std::io::Error| NewgitError::SourceCommand {
        command: description.to_owned(),
        stderr: source.to_string(),
    };
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    // Written from another thread while this one drains stdout: Git answers
    // line by line, so writing everything first deadlocks once its output
    // fills the pipe. The pipe drops at the end of the thread, so Git sees
    // EOF.
    let writer = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        let bytes = bytes.to_vec();
        std::thread::spawn(move || pipe.write_all(&bytes))
    });
    let output = child.wait_with_output().map_err(spawn_error)?;
    if let Some(writer) = writer {
        writer
            .join()
            .expect("stdin writer does not panic")
            .map_err(spawn_error)?;
    }
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(NewgitError::SourceCommand {
            command: description.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Remove a tracker's owned paths from a workspace — restoring "no captured
/// content" means file absence, not leftovers.
pub fn clear_owned_paths(workspace: &Utf8Path, definition: &TrackerDefinition) -> Result<()> {
    for owned in &definition.paths {
        // A more specific tracker owns something inside: remove this
        // tracker's files one by one and leave that tracker's alone.
        if definition
            .excluded
            .iter()
            .any(|inner| inner.starts_with(owned))
        {
            for (_, absolute) in collect_owned_files(workspace, definition)? {
                std::fs::remove_file(&absolute)
                    .map_err(|source| NewgitError::io(&absolute, source))?;
            }
            continue;
        }
        let target = workspace.join(owned);
        if target.is_dir() {
            std::fs::remove_dir_all(&target).map_err(|source| NewgitError::io(&target, source))?;
        } else if target.is_file() {
            std::fs::remove_file(&target).map_err(|source| NewgitError::io(&target, source))?;
        }
    }
    Ok(())
}

pub fn copy_file(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        create_dir_all(parent)?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|source| NewgitError::io(to.to_path_buf(), source))
}
