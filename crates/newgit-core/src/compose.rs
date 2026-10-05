//! A workspace's Git is a view composed from tracker repos
//! (newgit-trackers.md, step 3). Two operations, both deterministic:
//!
//! - [`compose`] replays every bound code tracker's history, interleaved by
//!   committer date, into one ordinary Git history — real authors, dates, and
//!   messages, so `git log` and `git blame` work everywhere.
//! - [`split`] takes view commits made on top of that (an agent's work) and
//!   splits each into one commit per tracker it touched, marked
//!   `Newgit-Split: <view commit>`. Composing again folds those back into the
//!   identical view commit, so commit ids never change under anyone.
//!
//! Everything happens in one repository (the project repo): callers bring
//! the tracker commits' objects into it first. Which tracker commits a view
//! commit stands for is kept in a note on it (`refs/notes/newgit-composed`):
//! one `<tracker> <commit>` line per tracker, as of that view commit.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::tracker::TrackerDefinition;

/// Where the per-view-commit tracker state lives.
pub const NOTES_REF: &str = "refs/notes/newgit-composed";

const SPLIT_TRAILER: &str = "Newgit-Split: ";

/// One tracker to compose, at the commit it is bound to.
#[derive(Debug, Clone)]
pub struct TrackerTip<'a> {
    pub definition: &'a TrackerDefinition,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The view's tip; `None` when no tracker had any commit.
    pub tip: Option<String>,
    /// How many view commits it has.
    pub commits: usize,
}

/// Tracker state as of a view commit: tracker name → its commit.
pub type State = BTreeMap<String, String>;

/// Replay every tracker's history into one view history at `view_ref`.
pub fn compose(repo: &Utf8Path, view_ref: &str, tips: &[TrackerTip<'_>]) -> Result<View> {
    // Every tracker commit, with its changes limited to what that tracker
    // owns: a root commit from before a carve-out still carries the carved
    // paths, but they are the carved tracker's now.
    let mut events = Vec::new();
    for tip in tips {
        let ids = first_parent_history(repo, &tip.commit)?;
        let changes = diff_tree(repo, &ids)?;
        let ids: Vec<String> = ids.into_iter().map(|(id, _)| id).collect();
        let raw = read_commits(repo, &ids)?;
        for id in ids {
            let commit = raw.get(&id).cloned().ok_or_else(|| missing(&id))?;
            let all_changes = changes.get(&id).cloned().unwrap_or_default();
            let touched = !all_changes.is_empty();
            let changes: Vec<Change> = all_changes
                .into_iter()
                .filter(|change| tip.definition.owns(Utf8Path::new(&change.path)))
                .collect();
            events.push(Event {
                tracker: tip.definition.name.clone(),
                id,
                commit,
                disowned: touched && changes.is_empty(),
                changes,
            });
        }
    }
    if events.is_empty() {
        return Ok(View {
            tip: None,
            commits: 0,
        });
    }

    // Interleave by committer date, then tracker name, then commit id — a
    // total order, so the same tracker histories always give the same view.
    events.sort_by(|left, right| {
        (left.commit.committer_time, &left.tracker, &left.id).cmp(&(
            right.commit.committer_time,
            &right.tracker,
            &right.id,
        ))
    });

    // Commits split from one source fold back into one view commit.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut disowned_only: Vec<usize> = Vec::new();
    let mut group_of: BTreeMap<String, usize> = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        let key = event
            .commit
            .split_source
            .clone()
            .unwrap_or_else(|| event.id.clone());
        match group_of.get(&key) {
            Some(&group) => groups[group].push(index),
            None => {
                group_of.insert(key, groups.len());
                groups.push(vec![index]);
            }
        }
    }

    // A group whose every member changed only paths it no longer owns is
    // another tracker's commit seen from here: drop it, or a view without
    // that tracker shows it as an empty commit. (A genuinely empty commit
    // is kept: none of its members is `disowned`.)
    for (index, members) in groups.iter().enumerate() {
        if members.iter().all(|&member| events[member].disowned) {
            disowned_only.push(index);
        }
    }
    let groups: Vec<Vec<usize>> = groups
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !disowned_only.contains(index))
        .map(|(_, members)| members)
        .collect();
    if groups.is_empty() {
        return Ok(View {
            tip: None,
            commits: 0,
        });
    }

    let mut stream = Vec::new();
    let mut state = State::new();
    let mut notes = Vec::new();
    for (number, members) in groups.iter().enumerate() {
        let mark = number + 1;
        // The source's metadata: the member that is not itself a split (an
        // original commit), or any member with its trailer stripped.
        let lead = members
            .iter()
            .map(|&index| &events[index])
            .find(|event| event.commit.split_source.is_none())
            .unwrap_or(&events[members[0]]);
        let message = lead.commit.message_without_trailer();
        writeln!(stream, "commit {view_ref}").ok();
        writeln!(stream, "mark :{mark}").ok();
        writeln!(stream, "author {}", lead.commit.author).ok();
        writeln!(stream, "committer {}", lead.commit.committer).ok();
        writeln!(stream, "data {}", message.len()).ok();
        stream.extend_from_slice(message.as_bytes());
        stream.push(b'\n');
        if mark > 1 {
            writeln!(stream, "from :{}", mark - 1).ok();
        }
        for &index in members {
            let event = &events[index];
            for change in &event.changes {
                match &change.new {
                    Some((mode, sha)) => {
                        writeln!(stream, "M {mode} {sha} {}", quote(&change.path)).ok();
                    }
                    None => {
                        writeln!(stream, "D {}", quote(&change.path)).ok();
                    }
                }
            }
            state.insert(event.tracker.clone(), event.id.clone());
        }
        notes.push((mark, render_state(&state)));
        stream.push(b'\n');
    }

    // The notes: one commit on NOTES_REF holding every view commit's state.
    // A fixed identity and date keep it deterministic too.
    let notes_parent = git_lines(repo, &["rev-parse", "-q", "--verify", NOTES_REF])
        .ok()
        .and_then(|lines| lines.into_iter().next());
    writeln!(stream, "commit {NOTES_REF}").ok();
    writeln!(
        stream,
        "committer newgit <newgit@localhost> 946684800 +0000"
    )
    .ok();
    writeln!(stream, "data 8\ncompose\n").ok();
    if let Some(parent) = notes_parent {
        writeln!(stream, "from {parent}").ok();
    }
    for (mark, note) in &notes {
        writeln!(stream, "N inline :{mark}").ok();
        writeln!(stream, "data {}", note.len()).ok();
        stream.extend_from_slice(note.as_bytes());
        stream.push(b'\n');
    }
    stream.push(b'\n');

    let marks = repo_tmp_file(repo, "compose-marks");
    run(
        Command::new("git").arg("-C").arg(repo.as_str()).args([
            "fast-import",
            "--quiet",
            "--force",
            "--date-format=raw",
            &format!("--export-marks={marks}"),
        ]),
        "git fast-import",
        Some(&stream),
    )?;
    let exported =
        std::fs::read_to_string(&marks).map_err(|source| NewgitError::io(&marks, source))?;
    let _ = std::fs::remove_file(&marks);
    let tip = exported
        .lines()
        .filter_map(|line| line.strip_prefix(':')?.split_once(' '))
        .find(|(mark, _)| mark.parse::<usize>().ok() == Some(groups.len()))
        .map(|(_, sha)| sha.to_owned());
    Ok(View {
        tip,
        commits: groups.len(),
    })
}

/// Why a change cannot be split into a tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A secret tracker's file was committed — secrets never enter Git.
    Secret { path: String, tracker: String },
    /// The path's owner is not bound in this view.
    Unbound { path: String, tracker: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Secret { path, tracker } => write!(
                f,
                "`{path}` belongs to the secret tracker `{tracker}`, and secrets never enter Git: \
                 `git rm --cached {path}` and commit again"
            ),
            Refusal::Unbound { path, tracker } => write!(
                f,
                "`{path}` belongs to `{tracker}`, which this instance has not bound: \
                 `newgit tracker use {tracker}`"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitOutcome {
    /// Each tracker's commit after the split (unchanged trackers included).
    pub state: State,
    /// Tracker commits made, as `(tracker, commit)` in order.
    pub made: Vec<(String, String)>,
}

/// Split the view commits after `base` up to `tip` (first parent) into
/// tracker commits. `bound` are the code trackers this view composes; `all`
/// is every definition, to name the owner of a path nothing bound owns.
///
/// Each view commit is noted with the state it leaves, so a later split, or
/// a recompose, picks up where this one stopped.
pub fn split(
    repo: &Utf8Path,
    base: &str,
    tip: &str,
    bound: &[&TrackerDefinition],
    all: &[TrackerDefinition],
) -> std::result::Result<SplitOutcome, SplitError> {
    let mut state = read_state(repo, base)?;
    let ids = first_parent_history(repo, &format!("{base}..{tip}"))?;
    let changes = diff_tree(repo, &ids)?;
    let ids: Vec<String> = ids.into_iter().map(|(id, _)| id).collect();
    let raw = read_commits(repo, &ids)?;
    let mut made = Vec::new();
    for id in &ids {
        let commit = raw.get(id).cloned().ok_or_else(|| missing(id))?;
        let mut by_tracker: BTreeMap<String, Vec<Change>> = BTreeMap::new();
        for change in changes.get(id).cloned().unwrap_or_default() {
            let path = Utf8Path::new(&change.path);
            if let Some(owner) = bound.iter().find(|definition| definition.owns(path)) {
                by_tracker
                    .entry(owner.name.clone())
                    .or_default()
                    .push(change);
                continue;
            }
            let owner = all
                .iter()
                .filter(|definition| definition.owns(path) || claims(definition, path))
                .max_by_key(|definition| specificity(definition, path))
                .map(|definition| definition.name.clone())
                .unwrap_or_default();
            let secret = all
                .iter()
                .any(|definition| definition.name == owner && definition.secret);
            return Err(SplitError::Refused(if secret {
                Refusal::Secret {
                    path: change.path,
                    tracker: owner,
                }
            } else {
                Refusal::Unbound {
                    path: change.path,
                    tracker: owner,
                }
            }));
        }
        let message = format!(
            "{}\n\n{SPLIT_TRAILER}{id}\n",
            commit.message.trim_end_matches('\n')
        );
        let identity = commit.identity_env();
        for (tracker, tracker_changes) in by_tracker {
            let parent = state.get(&tracker).cloned();
            let definition = bound
                .iter()
                .find(|definition| definition.name == tracker)
                .expect("bound owner");
            let made_commit = commit_changes(
                repo,
                parent.as_deref(),
                &definition.excluded,
                &tracker_changes,
                &message,
                &identity,
            )?;
            state.insert(tracker.clone(), made_commit.clone());
            made.push((tracker, made_commit));
        }
        write_note(repo, id, &render_state(&state))?;
    }
    Ok(SplitOutcome { state, made })
}

#[derive(Debug)]
pub enum SplitError {
    Refused(Refusal),
    Git(NewgitError),
}

impl From<NewgitError> for SplitError {
    fn from(error: NewgitError) -> Self {
        SplitError::Git(error)
    }
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitError::Refused(refusal) => refusal.fmt(f),
            SplitError::Git(error) => error.fmt(f),
        }
    }
}

/// The tracker state noted on a view commit.
pub fn read_state(repo: &Utf8Path, view_commit: &str) -> Result<State> {
    let note = git(repo, &["notes", "--ref", NOTES_REF, "show", view_commit]).unwrap_or_default();
    Ok(note
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(tracker, commit)| (tracker.to_owned(), commit.to_owned()))
        .collect())
}

fn claims(definition: &TrackerDefinition, path: &Utf8Path) -> bool {
    definition
        .paths
        .iter()
        .any(|owned| owned.as_str() != "." && path.starts_with(owned))
}

/// How specifically `definition` claims `path`: the length of its longest
/// claim on it, so the most specific claimant wins.
fn specificity(definition: &TrackerDefinition, path: &Utf8Path) -> usize {
    definition
        .paths
        .iter()
        .filter(|owned| owned.as_str() == "." || path.starts_with(owned))
        .map(|owned| {
            if owned.as_str() == "." {
                0
            } else {
                owned.as_str().len()
            }
        })
        .max()
        .unwrap_or(0)
}

fn render_state(state: &State) -> String {
    state
        .iter()
        .map(|(tracker, commit)| format!("{tracker} {commit}\n"))
        .collect()
}

fn write_note(repo: &Utf8Path, commit: &str, note: &str) -> Result<()> {
    git(
        repo,
        &[
            "notes",
            "--ref",
            NOTES_REF,
            "add",
            "-f",
            "-m",
            note.trim_end(),
            commit,
        ],
    )
    .map(drop)
}

/// A commit on top of `parent` with `changes` applied (and paths a more
/// specific tracker owns dropped), through a throwaway index — nothing
/// touches a working tree.
fn commit_changes(
    repo: &Utf8Path,
    parent: Option<&str>,
    disowned: &[Utf8PathBuf],
    changes: &[Change],
    message: &str,
    identity: &[(String, String)],
) -> Result<String> {
    let index = repo_tmp_file(repo, "split-index");
    let env = vec![("GIT_INDEX_FILE".to_owned(), index.to_string())];
    let result = (|| {
        match parent {
            Some(parent) => git_env(repo, &["read-tree", parent], &env)?,
            None => git_env(repo, &["read-tree", "--empty"], &env)?,
        };
        // History from before a carve-out still holds the carved paths;
        // this tracker's new commits do not carry them forward.
        if !disowned.is_empty() {
            let mut args = vec!["rm", "--cached", "-r", "-q", "-f", "--ignore-unmatch", "--"];
            args.extend(disowned.iter().map(|path| path.as_str()));
            git_env(repo, &args, &env)?;
        }
        let mut info = Vec::new();
        for change in changes {
            match &change.new {
                Some((mode, sha)) => {
                    info.extend_from_slice(format!("{mode} {sha}\t{}\0", change.path).as_bytes())
                }
                None => info.extend_from_slice(
                    format!(
                        "0 0000000000000000000000000000000000000000\t{}\0",
                        change.path
                    )
                    .as_bytes(),
                ),
            }
        }
        run(
            command(repo, &["update-index", "-z", "--index-info"])
                .envs(env.iter().map(|(k, v)| (k, v))),
            "git update-index",
            Some(&info),
        )?;
        let tree = git_env(repo, &["write-tree"], &env)?;
        let mut args = vec![
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            tree.as_str(),
            "-F",
            "-",
        ];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        let mut full_env = identity.to_vec();
        full_env.extend(env.iter().cloned());
        let out = run(
            command(repo, &args).envs(full_env.iter().map(|(k, v)| (k, v))),
            "git commit-tree",
            Some(message.as_bytes()),
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

#[derive(Debug, Clone)]
struct Event {
    tracker: String,
    id: String,
    commit: RawCommit,
    changes: Vec<Change>,
    /// It changed something, but nothing this tracker owns now (a root
    /// commit from before a carve-out that touched only the carved paths).
    disowned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    path: String,
    /// `(mode, sha)` after the change; `None` for a deletion.
    new: Option<(String, String)>,
}

#[derive(Debug, Clone)]
struct RawCommit {
    /// `Name <email> <unix> <tz>`, exactly as stored.
    author: String,
    committer: String,
    committer_time: i64,
    message: String,
    /// The view commit this was split from, when it was.
    split_source: Option<String>,
}

impl RawCommit {
    fn message_without_trailer(&self) -> String {
        match &self.split_source {
            None => self.message.clone(),
            Some(source) => {
                let trailer = format!("\n\n{SPLIT_TRAILER}{source}");
                let trimmed = self.message.trim_end_matches('\n');
                let base = trimmed.strip_suffix(&trailer).unwrap_or(trimmed);
                format!("{base}\n")
            }
        }
    }

    /// Environment that makes `git commit-tree` record this commit's own
    /// author and committer, dates included.
    fn identity_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        for (role, line) in [("AUTHOR", &self.author), ("COMMITTER", &self.committer)] {
            if let Some((name, rest)) = line.split_once(" <")
                && let Some((email, date)) = rest.split_once("> ")
            {
                env.push((format!("GIT_{role}_NAME"), name.to_owned()));
                env.push((format!("GIT_{role}_EMAIL"), email.to_owned()));
                env.push((format!("GIT_{role}_DATE"), format!("@{date}")));
            }
        }
        env
    }
}

fn parse_commit(raw: &str) -> RawCommit {
    let (headers, message) = raw.split_once("\n\n").unwrap_or((raw, ""));
    let mut author = String::new();
    let mut committer = String::new();
    for line in headers.lines() {
        if let Some(value) = line.strip_prefix("author ") {
            author = value.to_owned();
        } else if let Some(value) = line.strip_prefix("committer ") {
            committer = value.to_owned();
        }
    }
    let committer_time = committer
        .rsplit(' ')
        .nth(1)
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or_default();
    let split_source = message
        .trim_end_matches('\n')
        .rsplit('\n')
        .next()
        .and_then(|last| last.strip_prefix(SPLIT_TRAILER))
        .map(ToOwned::to_owned);
    RawCommit {
        author,
        committer,
        committer_time,
        message: message.to_owned(),
        split_source,
    }
}

/// Every commit's raw object, read in one `cat-file --batch` pass.
fn read_commits(repo: &Utf8Path, ids: &[String]) -> Result<BTreeMap<String, RawCommit>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
    let out = run(
        &mut command(repo, &["cat-file", "--batch"]),
        "git cat-file --batch",
        Some(input.as_bytes()),
    )?;
    let mut commits = BTreeMap::new();
    let mut rest: &[u8] = &out;
    while let Some(newline) = rest.iter().position(|&byte| byte == b'\n') {
        let header = String::from_utf8_lossy(&rest[..newline]).to_string();
        let mut parts = header.split(' ');
        let (Some(id), Some(_kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            break;
        };
        let size: usize = size.parse().unwrap_or(0);
        let start = newline + 1;
        let body = String::from_utf8_lossy(&rest[start..start + size]).to_string();
        commits.insert(id.to_owned(), parse_commit(&body));
        rest = &rest[(start + size + 1).min(rest.len())..];
    }
    Ok(commits)
}

/// `(commit, first parent)`, oldest first, along the first-parent chain of
/// `range` — one `rev-list` call.
fn first_parent_history(repo: &Utf8Path, range: &str) -> Result<Vec<(String, Option<String>)>> {
    Ok(git_lines(
        repo,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            "--parents",
            range,
        ],
    )?
    .into_iter()
    .map(|line| {
        let mut fields = line.split(' ');
        let id = fields.next().unwrap_or_default().to_owned();
        (id, fields.next().map(ToOwned::to_owned))
    })
    .collect())
}

/// Every commit's changes against its first parent, in one `diff-tree
/// --stdin` pass.
fn diff_tree(
    repo: &Utf8Path,
    ids: &[(String, Option<String>)],
) -> Result<BTreeMap<String, Vec<Change>>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut input = String::new();
    for (id, parent) in ids {
        match parent {
            Some(parent) => input.push_str(&format!("{id} {parent}\n")),
            None => input.push_str(&format!("{id}\n")),
        }
    }
    let out = run(
        &mut command(
            repo,
            &[
                "diff-tree",
                "--stdin",
                "-r",
                "-z",
                "--raw",
                "--no-renames",
                "--root",
            ],
        ),
        "git diff-tree --stdin",
        Some(input.as_bytes()),
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut tokens = text.split('\0').filter(|token| !token.is_empty());
    let mut changes: BTreeMap<String, Vec<Change>> = BTreeMap::new();
    let mut current = String::new();
    while let Some(token) = tokens.next() {
        if let Some(meta) = token.strip_prefix(':') {
            let Some(path) = tokens.next() else { break };
            let fields: Vec<&str> = meta.split(' ').collect();
            let (new_mode, new_sha, status) = (fields[1], fields[3], fields[4]);
            let new = (!status.starts_with('D')).then(|| (new_mode.to_owned(), new_sha.to_owned()));
            changes.entry(current.clone()).or_default().push(Change {
                path: path.to_owned(),
                new,
            });
        } else {
            current = token.trim().to_owned();
            changes.entry(current.clone()).or_default();
        }
    }
    Ok(changes)
}

/// fast-import's C-style quoting, always applied so no path is special.
fn quote(path: &str) -> String {
    let mut out = String::from("\"");
    for character in path.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn missing(id: &str) -> NewgitError {
    NewgitError::SourceCommand {
        command: "git cat-file --batch".to_owned(),
        stderr: format!("commit {id} is not in the repository"),
    }
}

fn repo_tmp_file(repo: &Utf8Path, label: &str) -> Utf8PathBuf {
    let git_dir = git(repo, &["rev-parse", "--absolute-git-dir"])
        .unwrap_or_else(|_| repo.join(".git").to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    Utf8PathBuf::from(git_dir).join(format!("newgit-{label}-{}-{nanos}", std::process::id()))
}

fn command(repo: &Utf8Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo.as_str()).args(args);
    command
}

fn git(repo: &Utf8Path, args: &[&str]) -> Result<String> {
    git_env(repo, args, &[])
}

fn git_env(repo: &Utf8Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    let out = run(
        command(repo, args).envs(env.iter().map(|(key, value)| (key, value))),
        &format!("git {}", args.join(" ")),
        None,
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn git_lines(repo: &Utf8Path, args: &[&str]) -> Result<Vec<String>> {
    Ok(git(repo, args)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
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
    // Written from another thread while this one drains stdout, so a large
    // stream cannot deadlock against Git's output.
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
