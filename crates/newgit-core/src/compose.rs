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
///
/// `all` is every tracker definition. Which tracker a change belongs to is
/// decided among the *code* trackers only: a secret tracker is never in a
/// view, so a path it owns now that history once committed (a `.env` later
/// gitignored) stays in the history of the code tracker that held it then.
/// Rewriting that away would change every commit after it.
pub fn compose(
    repo: &Utf8Path,
    view_ref: &str,
    tips: &[TrackerTip<'_>],
    all: &[TrackerDefinition],
) -> Result<View> {
    let mut code: Vec<&TrackerDefinition> = tips.iter().map(|tip| tip.definition).collect();
    for definition in all {
        if !definition.secret && !code.iter().any(|known| known.name == definition.name) {
            code.push(definition);
        }
    }
    let owner_of = |path: &Utf8Path| code_owner(&code, path).map(|owner| owner.name.as_str());
    // Each tracker's whole history: its first-parent spine, interleaved with
    // the other trackers' spines into the view's main line, and its side
    // branches (what its merges brought in), replayed as real commits so a
    // rebuilt merge keeps every parent.
    let mut events = Vec::new();
    let mut side: BTreeMap<String, SideCommit> = BTreeMap::new();
    for tip in tips {
        let dag = full_history(repo, &tip.commit)?;
        let spine: std::collections::BTreeSet<String> = first_parent_history(repo, &tip.commit)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let pairs: Vec<(String, Option<String>)> = dag
            .iter()
            .map(|(id, parents)| (id.clone(), parents.first().cloned()))
            .collect();
        let changes = diff_tree(repo, &pairs)?;
        let ids: Vec<String> = dag.iter().map(|(id, _)| id.clone()).collect();
        let raw = read_commits(repo, &ids)?;
        for (id, parents) in dag {
            let commit = raw.get(&id).cloned().ok_or_else(|| missing(&id))?;
            let all_changes = changes.get(&id).cloned().unwrap_or_default();
            let owned: Vec<Change> = all_changes
                .iter()
                .filter(|change| {
                    owner_of(Utf8Path::new(&change.path)) == Some(tip.definition.name.as_str())
                })
                .cloned()
                .collect();
            if spine.contains(&id) {
                events.push(Event {
                    tracker: tip.definition.name.clone(),
                    id,
                    first_parent: parents.first().cloned(),
                    extra_parents: parents.iter().skip(1).cloned().collect(),
                    commit,
                    disowned: !all_changes.is_empty() && owned.is_empty(),
                    changes: owned,
                    all_changes,
                });
            } else {
                // On a side branch only this tracker's history exists — the
                // carved trackers' histories follow their main line — so a
                // change to a path another *bound* tracker owns is the true
                // state of that path on this branch (it predates the
                // carve-out). Paths no bound tracker owns stay out.
                let applied: Vec<Change> = all_changes
                    .iter()
                    .filter(|change| {
                        let owner = owner_of(Utf8Path::new(&change.path));
                        tips.iter()
                            .any(|bound| Some(bound.definition.name.as_str()) == owner)
                    })
                    .cloned()
                    .collect();
                side.insert(
                    id,
                    SideCommit {
                        tracker: tip.definition.name.clone(),
                        parents,
                        commit,
                        changes: applied,
                        all_changes,
                    },
                );
            }
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
    // that tracker shows it as an empty commit. (A genuinely empty commit is
    // kept: none of its members is `disowned`.) Its members still map to the
    // view commit before it, so merges that name them find a parent.
    let mut dropped: Vec<Vec<usize>> = Vec::new();
    let groups: Vec<Vec<usize>> = groups
        .into_iter()
        .filter_map(|members| {
            if members.iter().all(|&member| events[member].disowned) {
                dropped.push(members);
                None
            } else {
                Some(members)
            }
        })
        .collect();
    let dropped_members: std::collections::BTreeSet<usize> =
        dropped.into_iter().flatten().collect();
    if groups.is_empty() {
        return Ok(View {
            tip: None,
            commits: 0,
        });
    }

    let source_of = |members: &[usize]| -> String {
        let first = &events[members[0]];
        first
            .commit
            .split_source
            .clone()
            .unwrap_or_else(|| first.id.clone())
    };
    let outside: Vec<String> = groups
        .iter()
        .map(|members| source_of(members))
        .filter(|source| {
            !events
                .iter()
                .any(|event| &event.id == source && event.commit.split_source.is_none())
        })
        .collect();
    let outside = describe_sources(repo, &outside)?;

    let mut out = Emitter {
        view_ref: view_ref.to_owned(),
        stream: Vec::new(),
        mark: 0,
        view_of: BTreeMap::new(),
        state_of: BTreeMap::new(),
        notes: Vec::new(),
        commits: 0,
    };
    let mut tip = Target::None;
    let mut state = State::new();
    let mut next_event = 0usize;
    for members in &groups {
        // Spine commits of dropped groups that precede this one map to the
        // view so far.
        while next_event < events.len() && members.iter().all(|&member| member != next_event) {
            if dropped_members.contains(&next_event) {
                out.view_of
                    .insert(events[next_event].id.clone(), tip.clone());
                out.state_of
                    .insert(events[next_event].id.clone(), state.clone());
            }
            next_event += 1;
        }

        // Side branches this group's merges bring in, replayed first.
        let mut merges: Vec<Target> = Vec::new();
        for &index in members {
            for parent in &events[index].extra_parents {
                let target = out.side(parent, &side, &tip, &state)?;
                if !merges.contains(&target) {
                    merges.push(target);
                }
            }
        }

        let source = source_of(members);
        let original = members
            .iter()
            .map(|&index| &events[index])
            .find(|event| event.id == source && event.commit.split_source.is_none());
        let (source_parent, source_changes, source_extra) = match original {
            Some(event) => (
                Some(event.first_parent.clone()),
                Some(event.all_changes.clone()),
                event.extra_parents.clone(),
            ),
            None => match outside.get(&source) {
                Some((parent, changes)) => {
                    (Some(parent.clone()), Some(changes.clone()), Vec::new())
                }
                None => (None, None, Vec::new()),
            },
        };
        let mut combined: Vec<&Change> = members
            .iter()
            .flat_map(|&index| events[index].changes.iter())
            .collect();
        combined.sort_by(|left, right| left.path.cmp(&right.path));

        // Reusable when the source sits exactly where the view puts it — same
        // first parent, and every merge parent came back as itself — and the
        // members' changes are exactly the source's own.
        let parent_matches = match (&tip, &source_parent) {
            (Target::None, Some(None)) => true,
            (Target::Commit(current), Some(Some(parent))) => parent == current,
            _ => false,
        };
        let merges_match = merges.len() == source_extra.len()
            && source_extra.iter().zip(&merges).all(
                |(parent, target)| matches!(target, Target::Commit(commit) if commit == parent),
            );
        let changes_match = source_changes.is_some_and(|mut own| {
            own.sort_by(|left, right| left.path.cmp(&right.path));
            own.iter().collect::<Vec<_>>() == combined
        });
        for &index in members {
            state.insert(events[index].tracker.clone(), events[index].id.clone());
        }
        tip = if parent_matches && merges_match && changes_match {
            out.reuse(&source, &state)
        } else {
            // Recreated: the source's metadata (the member that is an original
            // commit, or any member with its trailer stripped).
            let lead = original.unwrap_or(&events[members[0]]);
            out.write(&lead.commit, &tip, &merges, &combined, &state)
        };
        for &index in members {
            out.view_of.insert(events[index].id.clone(), tip.clone());
            out.state_of.insert(events[index].id.clone(), state.clone());
        }
        next_event = members.iter().max().map_or(next_event, |last| last + 1);
    }
    out.finish(repo, tip, groups.len())
}

/// A tracker commit off its tracker's first-parent spine: what a merge
/// brought in.
#[derive(Debug, Clone)]
struct SideCommit {
    tracker: String,
    parents: Vec<String>,
    commit: RawCommit,
    changes: Vec<Change>,
    all_changes: Vec<Change>,
}

/// Where a tracker commit landed in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    None,
    /// An existing commit, reused as-is.
    Commit(String),
    /// A commit written in this fast-import stream.
    Mark(usize),
}

impl Target {
    fn reference(&self) -> Option<String> {
        match self {
            Target::None => None,
            Target::Commit(commit) => Some(commit.clone()),
            Target::Mark(mark) => Some(format!(":{mark}")),
        }
    }
}

/// Builds the fast-import stream for a view.
struct Emitter {
    view_ref: String,
    stream: Vec<u8>,
    mark: usize,
    /// Tracker commit → where it landed in the view.
    view_of: BTreeMap<String, Target>,
    /// Tracker commit → the tracker state of the view commit holding it.
    state_of: BTreeMap<String, State>,
    notes: Vec<(String, String)>,
    commits: usize,
}

impl Emitter {
    fn reuse(&mut self, commit: &str, state: &State) -> Target {
        self.notes.push((commit.to_owned(), render_state(state)));
        self.commits += 1;
        Target::Commit(commit.to_owned())
    }

    fn write(
        &mut self,
        commit: &RawCommit,
        from: &Target,
        merges: &[Target],
        changes: &[&Change],
        state: &State,
    ) -> Target {
        let message = commit.message_without_trailer();
        self.mark += 1;
        let stream = &mut self.stream;
        writeln!(stream, "commit {}", self.view_ref).ok();
        writeln!(stream, "mark :{}", self.mark).ok();
        writeln!(stream, "author {}", commit.author).ok();
        writeln!(stream, "committer {}", commit.committer).ok();
        writeln!(stream, "data {}", message.len()).ok();
        stream.extend_from_slice(message.as_bytes());
        stream.push(b'\n');
        if let Some(reference) = from.reference() {
            writeln!(stream, "from {reference}").ok();
        }
        for merge in merges {
            if let Some(reference) = merge.reference() {
                writeln!(stream, "merge {reference}").ok();
            }
        }
        for change in changes {
            match &change.new {
                Some((mode, sha)) => {
                    writeln!(stream, "M {mode} {sha} {}", quote(&change.path)).ok()
                }
                None => writeln!(stream, "D {}", quote(&change.path)).ok(),
            };
        }
        stream.push(b'\n');
        self.notes
            .push((format!(":{}", self.mark), render_state(state)));
        self.commits += 1;
        Target::Mark(self.mark)
    }

    /// Where side commit `id` lands, replaying it (and whatever of its
    /// ancestry is not in the view yet) first. Its tracker's content is its
    /// own; the other trackers' is what they held where it forked off — the
    /// view commit holding its first parent.
    fn side(
        &mut self,
        id: &str,
        side: &BTreeMap<String, SideCommit>,
        spine_tip: &Target,
        spine_state: &State,
    ) -> Result<Target> {
        // Depth-first over unreplayed ancestry, iteratively so a long branch
        // cannot overflow the stack.
        let mut stack = vec![(id.to_owned(), false)];
        while let Some((current, ready)) = stack.pop() {
            if self.view_of.contains_key(&current) {
                continue;
            }
            let Some(commit) = side.get(&current) else {
                // Not a side commit and not in the view: a spine commit from a
                // later group (a merge of the future) — fall back to the tip.
                self.view_of.insert(current.clone(), spine_tip.clone());
                self.state_of.insert(current, spine_state.clone());
                continue;
            };
            if !ready {
                stack.push((current.clone(), true));
                for parent in commit.parents.iter().rev() {
                    if !self.view_of.contains_key(parent) {
                        stack.push((parent.clone(), false));
                    }
                }
                continue;
            }
            let parents: Vec<Target> = commit
                .parents
                .iter()
                .map(|parent| self.view_of.get(parent).cloned().unwrap_or(Target::None))
                .collect();
            let mut state = commit
                .parents
                .first()
                .and_then(|parent| self.state_of.get(parent).cloned())
                .unwrap_or_default();
            state.insert(commit.tracker.clone(), current.clone());
            let all_original = commit.parents.iter().zip(&parents).all(
                |(parent, target)| matches!(target, Target::Commit(reused) if reused == parent),
            );
            let mut own = commit.all_changes.clone();
            own.sort_by(|left, right| left.path.cmp(&right.path));
            let mut changes: Vec<&Change> = commit.changes.iter().collect();
            changes.sort_by(|left, right| left.path.cmp(&right.path));
            let target = if all_original && own.iter().collect::<Vec<_>>() == changes {
                self.reuse(&current, &state)
            } else {
                let from = parents.first().cloned().unwrap_or(Target::None);
                self.write(
                    &commit.commit,
                    &from,
                    &parents[1.min(parents.len())..],
                    &changes,
                    &state,
                )
            };
            self.view_of.insert(current.clone(), target);
            self.state_of.insert(current, state);
        }
        Ok(self.view_of.get(id).cloned().unwrap_or(Target::None))
    }

    fn finish(mut self, repo: &Utf8Path, tip: Target, groups: usize) -> Result<View> {
        // Ended on a reused commit (or wrote side commits after the last
        // written spine commit): point the view at the tip.
        if let Some(reference) = tip.reference() {
            writeln!(self.stream, "reset {}\nfrom {reference}\n", self.view_ref).ok();
        }
        // The notes: one commit on NOTES_REF holding every view commit's
        // state, with a fixed identity and date so it is deterministic too.
        let notes_parent = git_lines(repo, &["rev-parse", "-q", "--verify", NOTES_REF])
            .ok()
            .and_then(|lines| lines.into_iter().next());
        let stream = &mut self.stream;
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
        for (target, note) in &self.notes {
            writeln!(stream, "N inline {target}").ok();
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
            Some(&self.stream),
        )?;
        let exported = std::fs::read_to_string(&marks).unwrap_or_default();
        let _ = std::fs::remove_file(&marks);
        let tip = match tip {
            Target::None => None,
            Target::Commit(commit) => Some(commit),
            Target::Mark(last) => exported
                .lines()
                .filter_map(|line| line.strip_prefix(':')?.split_once(' '))
                .find(|(number, _)| number.parse::<usize>().ok() == Some(last))
                .map(|(_, sha)| sha.to_owned()),
        };
        let _ = groups;
        Ok(View {
            tip,
            commits: self.commits,
        })
    }
}

/// Every commit reachable from `tip`, oldest first in topological order,
/// with all its parents — one `rev-list` call.
fn full_history(repo: &Utf8Path, tip: &str) -> Result<Vec<(String, Vec<String>)>> {
    Ok(git_lines(
        repo,
        &["rev-list", "--topo-order", "--reverse", "--parents", tip],
    )?
    .into_iter()
    .map(|line| {
        let mut fields = line.split(' ').map(ToOwned::to_owned);
        let id = fields.next().unwrap_or_default();
        (id, fields.collect())
    })
    .collect())
}

/// A split source's first parent and full changes.
type Described = (Option<String>, Vec<Change>);

/// For split sources that are not themselves tracker commits (view commits
/// an agent made): whether each exists here, its first parent, and its full
/// changes — what reusing it as-is requires. Absent sources are left out.
fn describe_sources(repo: &Utf8Path, sources: &[String]) -> Result<BTreeMap<String, Described>> {
    let mut described = BTreeMap::new();
    if sources.is_empty() {
        return Ok(described);
    }
    let input: String = sources.iter().map(|source| format!("{source}\n")).collect();
    let present = run(
        &mut command(
            repo,
            &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        ),
        "git cat-file --batch-check",
        Some(input.as_bytes()),
    )?;
    let present: Vec<String> = String::from_utf8_lossy(&present)
        .lines()
        .filter(|line| line.ends_with(" commit"))
        .filter_map(|line| line.split(' ').next().map(ToOwned::to_owned))
        .collect();
    if present.is_empty() {
        return Ok(described);
    }
    let mut args = vec!["rev-list", "--no-walk=unsorted", "--parents"];
    args.extend(present.iter().map(String::as_str));
    let history: Vec<(String, Option<String>)> = git_lines(repo, &args)?
        .into_iter()
        .map(|line| {
            let mut fields = line.split(' ');
            let id = fields.next().unwrap_or_default().to_owned();
            (id, fields.next().map(ToOwned::to_owned))
        })
        .collect();
    let changes = diff_tree(repo, &history)?;
    for (id, parent) in history {
        let own = changes.get(&id).cloned().unwrap_or_default();
        described.insert(id, (parent, own));
    }
    Ok(described)
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

/// Carve a new code tracker's history out of `source` (the history of
/// whatever owned its paths until now, usually root): every commit that
/// touched `definition`'s paths becomes a commit of the new tracker — only
/// those paths, the original author, date, and message, marked
/// `Newgit-Split: <original>` — written to `target_ref` in one fast-import
/// pass. Because each is marked with its source, composing the source and
/// the new tracker folds them back into the original commits.
///
/// Returns the new tracker's tip, or `None` when its paths have no history.
pub fn carve(
    repo: &Utf8Path,
    source: &str,
    definition: &TrackerDefinition,
    target_ref: &str,
) -> Result<Option<String>> {
    let ids = first_parent_history(repo, source)?;
    let changes = diff_tree(repo, &ids)?;
    let ids: Vec<String> = ids.into_iter().map(|(id, _)| id).collect();
    let raw = read_commits(repo, &ids)?;
    let mut stream = Vec::new();
    let mut mark = 0usize;
    for id in &ids {
        let owned: Vec<&Change> = changes
            .get(id)
            .map(|all| {
                all.iter()
                    .filter(|change| claims(definition, Utf8Path::new(&change.path)))
                    .collect()
            })
            .unwrap_or_default();
        if owned.is_empty() {
            continue;
        }
        let commit = raw.get(id).ok_or_else(|| missing(id))?;
        // A commit that was itself split keeps the view commit it came from,
        // so all of that view commit's pieces still fold back together.
        let message = match &commit.split_source {
            Some(_) => commit.message.clone(),
            None => format!(
                "{}\n\n{SPLIT_TRAILER}{id}\n",
                commit.message.trim_end_matches('\n')
            ),
        };
        mark += 1;
        writeln!(stream, "commit {target_ref}").ok();
        writeln!(stream, "mark :{mark}").ok();
        writeln!(stream, "author {}", commit.author).ok();
        writeln!(stream, "committer {}", commit.committer).ok();
        writeln!(stream, "data {}", message.len()).ok();
        stream.extend_from_slice(message.as_bytes());
        stream.push(b'\n');
        if mark > 1 {
            writeln!(stream, "from :{}", mark - 1).ok();
        }
        for change in owned {
            match &change.new {
                Some((mode, sha)) => {
                    writeln!(stream, "M {mode} {sha} {}", quote(&change.path)).ok()
                }
                None => writeln!(stream, "D {}", quote(&change.path)).ok(),
            };
        }
        stream.push(b'\n');
    }
    if mark == 0 {
        return Ok(None);
    }
    run(
        Command::new("git").arg("-C").arg(repo.as_str()).args([
            "fast-import",
            "--quiet",
            "--force",
            "--date-format=raw",
        ]),
        "git fast-import",
        Some(&stream),
    )?;
    Ok(git_lines(repo, &["rev-parse", target_ref])?
        .into_iter()
        .next())
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
        let sign = signs_for(repo, &commit.author);
        for (tracker, tracker_changes) in by_tracker {
            let parent = state.get(&tracker).cloned();
            let definition = bound
                .iter()
                .find(|definition| definition.name == tracker)
                .expect("bound owner");
            let made_commit = commit_changes(
                repo,
                parent.as_deref(),
                &carved_out(definition, all),
                &tracker_changes,
                &message,
                &identity,
                sign,
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

/// The most specific of `code` claiming `path` (root claims everything).
fn code_owner<'a>(
    code: &[&'a TrackerDefinition],
    path: &Utf8Path,
) -> Option<&'a TrackerDefinition> {
    code.iter()
        .copied()
        .filter(|definition| definition.owns_everything() || claims(definition, path))
        .max_by_key(|definition| specificity(definition, path))
}

/// The paths more specific code trackers carved out of `definition`: what
/// its new commits must not carry forward from history before the carve.
fn carved_out(definition: &TrackerDefinition, all: &[TrackerDefinition]) -> Vec<Utf8PathBuf> {
    all.iter()
        .filter(|other| !other.secret && other.name != definition.name)
        .flat_map(|other| other.paths.iter())
        .filter(|path| {
            path.as_str() != "."
                && (definition.owns_everything() || claims(definition, path))
                && specificity(definition, path) < path.as_str().len()
        })
        .cloned()
        .collect()
}

/// The first commit among `rev_list` (arguments to `git rev-list`, so a
/// caller says what is new: `<tip> --not --all`) that changes a path a
/// secret tracker owns, with that path and tracker. Every parent of a merge
/// is diffed, so a secret cannot ride in on one. `env` lets the caller see
/// objects Git is still holding in quarantine.
pub fn find_secret(
    repo: &Utf8Path,
    rev_list: &[&str],
    env: &[(String, String)],
    all: &[TrackerDefinition],
) -> Result<Option<(String, String, String)>> {
    let mut args = vec!["rev-list"];
    args.extend_from_slice(rev_list);
    let commits = git_env(repo, &args, env)?;
    if commits.trim().is_empty() {
        return Ok(None);
    }
    let output = run(
        command(
            repo,
            &[
                "diff-tree",
                "--stdin",
                "-r",
                "-m",
                "--root",
                "--name-only",
                "-z",
            ],
        )
        .envs(env.iter().map(|(key, value)| (key, value))),
        "git diff-tree",
        Some(format!("{}\n", commits.trim()).as_bytes()),
    )?;
    let output = String::from_utf8_lossy(&output);
    let mut commit = String::new();
    for field in output.split('\0').filter(|field| !field.is_empty()) {
        let field = field.trim_start_matches('\n');
        if field.len() == 40 && field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            commit = field.to_owned();
            continue;
        }
        let path = Utf8Path::new(field);
        let owner = all
            .iter()
            .filter(|definition| definition.owns_everything() || claims(definition, path))
            .max_by_key(|definition| specificity(definition, path));
        if let Some(owner) = owner.filter(|owner| owner.secret) {
            return Ok(Some((commit, field.to_owned(), owner.name.clone())));
        }
    }
    Ok(None)
}

/// The nearest commit on `rev`'s first-parent line (itself included) that
/// carries a note — the last point whose tracker state is known.
pub fn find_base(repo: &Utf8Path, rev: &str) -> Result<Option<String>> {
    let noted: std::collections::BTreeSet<String> =
        git_lines(repo, &["notes", "--ref", NOTES_REF, "list"])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|line| line.split_once(' ').map(|(_, commit)| commit.to_owned()))
            .collect();
    if noted.is_empty() {
        return Ok(None);
    }
    Ok(git_lines(repo, &["rev-list", "--first-parent", rev])?
        .into_iter()
        .find(|commit| noted.contains(commit)))
}

/// Whether a commit by `author` (`Name <email> …`) is signed when newgit
/// makes it here: only when this machine signs its commits
/// (`commit.gpgSign`) and the author is this machine's user — newgit signs
/// with your key, never on someone else's behalf.
fn signs_for(repo: &Utf8Path, author: &str) -> bool {
    let config = |key: &str| {
        git_lines(repo, &["config", "--get", key])
            .ok()
            .and_then(|lines| lines.into_iter().next())
    };
    let signing = git_lines(repo, &["config", "--bool", "--get", "commit.gpgSign"])
        .ok()
        .and_then(|lines| lines.into_iter().next())
        .is_some_and(|value| value == "true");
    let email = author
        .split_once(" <")
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(email, _)| email);
    signing && email.is_some() && config("user.email").as_deref() == email
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
    sign: bool,
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
        if sign {
            args.push("-S");
        }
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
    first_parent: Option<String>,
    /// Its other parents, when it is a merge.
    extra_parents: Vec<String>,
    commit: RawCommit,
    /// Its changes, limited to what its tracker owns.
    changes: Vec<Change>,
    /// Every change it made, owned or not: what reusing it as-is requires.
    all_changes: Vec<Change>,
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
