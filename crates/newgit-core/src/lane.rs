use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::tracker::{TrackerDefinition, collect_all_files, collect_owned_files};

/// Length of the rev string newgit shows and records: a prefix of the
/// lane commit's id, the way Git abbreviates.
const REV_LEN: usize = 12;

/// Fixed identity and date for lane commits. A lane commit is a name for a
/// tree, not an event, so nothing about who or when may enter its id —
/// identical content has to yield the identical rev, on any machine.
const LANE_IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "newgit"),
    ("GIT_AUTHOR_EMAIL", "newgit@localhost"),
    ("GIT_AUTHOR_DATE", "@946684800 +0000"),
    ("GIT_COMMITTER_NAME", "newgit"),
    ("GIT_COMMITTER_EMAIL", "newgit@localhost"),
    ("GIT_COMMITTER_DATE", "@946684800 +0000"),
];

/// A tracker's content lane: Git commits in the store's lanes repo
/// (`.newgit/local/lanes.git`), one per distinct captured content.
///
/// - `refs/lanes/<tracker>/revs/<rev>` keeps each captured rev alive.
/// - `refs/lanes/<tracker>/head` marks the lane head — what new instances
///   project and `pull` brings in.
///
/// A rev is a parentless commit of the captured files with a fixed identity
/// and date, so it is content-addressed: capturing identical content again
/// yields the same rev. Lane history is not a parent chain; checkpoint
/// records already say which rev was bound when.
///
/// The lanes repo is separate from the store repo on purpose. Lane content
/// includes secrets, and the store repo is the one a `git push --mirror`
/// publishes and a local `git clone` hardlinks wholesale into every
/// workspace.
///
/// Checkpoints reference lane revs by name, so cleanup must never delete a
/// rev ref any checkpoint record still points at.
#[derive(Debug, Clone)]
pub struct TrackerLane {
    git_dir: Utf8PathBuf,
    checkouts: Utf8PathBuf,
    tracker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub rev: String,
    pub files: usize,
}

/// A lane rev present in the lanes repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRev {
    pub tracker: String,
    pub rev: String,
}

impl TrackerLane {
    /// `git_dir` is the lanes repo; `checkouts` is the cache directory revs
    /// are materialized into when a command needs a path.
    pub fn new(git_dir: &Utf8Path, checkouts: &Utf8Path, tracker: &str) -> Self {
        Self {
            git_dir: git_dir.to_path_buf(),
            checkouts: checkouts.join(tracker),
            tracker: tracker.to_owned(),
        }
    }

    pub fn latest(&self) -> Option<String> {
        self.resolve(&self.head_ref()).map(|commit| short(&commit))
    }

    pub fn set_latest(&self, rev: &str) -> Result<()> {
        let commit = self.commit_for(rev)?;
        git(&self.git_dir, &["update-ref", &self.head_ref(), &commit]).map(drop)
    }

    pub fn has_rev(&self, rev: &str) -> bool {
        self.resolve(&self.rev_ref(rev)).is_some()
    }

    /// Snapshot the tracker's owned paths from `workspace`. Identical
    /// content dedupes to an existing rev.
    pub fn capture(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
    ) -> Result<CaptureOutcome> {
        if definition.paths.is_empty() {
            return Err(NewgitError::TrackerHasNoPaths(self.tracker.clone()));
        }
        let files = collect_owned_files(workspace, definition)?;
        self.store_snapshot(&files)
    }

    /// Snapshot an arbitrary directory's contents into the lane — how a
    /// resource checkpoint deposits into a deposit-only tracker (one with no
    /// owned workspace paths).
    pub fn deposit(&self, dir: &Utf8Path) -> Result<CaptureOutcome> {
        let files = collect_all_files(dir)?;
        self.store_snapshot(&files)
    }

    /// Write files as a content-addressed rev. Identical content dedupes to
    /// an existing rev.
    fn store_snapshot(&self, files: &[(Utf8PathBuf, Utf8PathBuf)]) -> Result<CaptureOutcome> {
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

        let commit = git_env(
            &self.git_dir,
            &[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "i18n.commitEncoding=UTF-8",
                "commit-tree",
                &tree,
                "-m",
                &self.tracker,
            ],
            &LANE_IDENT.map(|(key, value)| (key.to_owned(), value.to_owned())),
        )?;
        let rev = short(&commit);
        git(&self.git_dir, &["update-ref", &self.rev_ref(&rev), &commit])?;

        Ok(CaptureOutcome {
            rev,
            files: files.len(),
        })
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

        clear_owned_paths(workspace, definition)?;

        // Only what the definition owns *now*: a rev captured before a path
        // was untracked must not write that path back.
        let files = self.tree_files(&commit, &definition.paths)?;
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
        self.resolve(&self.rev_ref(rev))
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

    fn head_ref(&self) -> String {
        format!("refs/lanes/{}/head", self.tracker)
    }

    fn rev_ref(&self, rev: &str) -> String {
        format!("refs/lanes/{}/revs/{rev}", self.tracker)
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

    /// Forget a rev: drop the ref keeping it alive and its materialized
    /// copy. The objects go at the next [`prune_unreferenced`].
    pub fn forget(&self, rev: &str) -> Result<()> {
        git(&self.git_dir, &["update-ref", "-d", &self.rev_ref(rev)])?;
        let dir = self.checkouts.join(rev);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| NewgitError::io(&dir, source))?;
        }
        Ok(())
    }
}

/// Every tracker with anything in the lanes repo, sorted.
pub fn lane_names(git_dir: &Utf8Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = lane_refs(git_dir)?
        .into_iter()
        .map(|(tracker, _)| tracker)
        .collect();
    names.dedup();
    Ok(names)
}

/// Every captured rev in every lane, sorted by tracker then rev.
pub fn lane_revs(git_dir: &Utf8Path) -> Result<Vec<LaneRev>> {
    Ok(lane_refs(git_dir)?
        .into_iter()
        .filter_map(|(tracker, rest)| {
            rest.strip_prefix("revs/").map(|rev| LaneRev {
                tracker,
                rev: rev.to_owned(),
            })
        })
        .collect())
}

/// Delete objects no ref reaches any more. Lane content can be secrets, so
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

/// `(tracker, rest)` for every ref under `refs/lanes/`, sorted.
fn lane_refs(git_dir: &Utf8Path) -> Result<Vec<(String, String)>> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let listing = git(
        git_dir,
        &["for-each-ref", "--format=%(refname)", "refs/lanes/"],
    )?;
    let mut refs: Vec<(String, String)> = listing
        .lines()
        .filter_map(|name| {
            let (tracker, rest) = name.strip_prefix("refs/lanes/")?.split_once('/')?;
            Some((tracker.to_owned(), rest.to_owned()))
        })
        .collect();
    refs.sort();
    Ok(refs)
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
