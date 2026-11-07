//! Mounted trackers held as pinned snapshots: the commits root pins, one
//! commit deep, in the project repo.
//!
//! A view needs a child's files at each commit root pins, never the child's
//! history, so `newgit clone` fetches root whole and each pin `--depth=1`
//! (`docs/design/trackers.md`, *Snapshots*). The project repo is the shallow
//! store: it is where composing and splitting run, and a commit split there
//! needs its pin as a parent. `trackers.git` holds whole histories and is
//! never shallow, so a tracker whose history has not arrived has no refs
//! there.
//!
//! `refs/newgit/snapshots/<tracker>/<commit>` keeps each snapshot alive and
//! says whose it is. A tracker is held as snapshots while one of those
//! commits is shallow, so a fetch that deepens one is all it takes to stop
//! counting: there is no second record to fall out of step with Git's.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;

use camino::Utf8Path;

use crate::compose;
use crate::error::{NewgitError, Result};
use crate::objects::Objects;
use crate::source::{GitSource, run_git_env};

/// Where snapshots are kept: `<prefix><tracker>/<commit>`.
pub const SNAPSHOT_REFS: &str = "refs/newgit/snapshots/";

/// Where a tracker's repo can be read.
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    pub auth: String,
}

/// The commits in `location`'s `shallow` file. Empty for a repo that is not
/// shallow, which costs one `rev-parse`.
pub fn shallow_commits(location: &[&str]) -> Result<BTreeSet<String>> {
    let git = |extra: &[&str]| {
        let mut args = location.to_vec();
        args.extend_from_slice(extra);
        run_git_env(&args, &[])
    };
    if git(&["rev-parse", "--is-shallow-repository"])?.trim() != "true" {
        return Ok(BTreeSet::new());
    }
    let file = git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "shallow",
    ])?;
    let file = file.trim();
    let text = std::fs::read_to_string(file)
        .map_err(|source| NewgitError::io(Utf8Path::new(file), source))?;
    Ok(text.lines().map(ToOwned::to_owned).collect())
}

/// Whether `location` is shallow for a reason other than snapshots: a
/// stepped fetch that stopped part way, which carries on deepening.
pub fn left_shallow(location: &[&str]) -> Result<bool> {
    let shallow = shallow_commits(location)?;
    if shallow.is_empty() {
        return Ok(false);
    }
    let mut args = location.to_vec();
    args.extend([
        "for-each-ref",
        "--format=%(objectname)",
        "refs/newgit/snapshots/",
    ]);
    let kept = run_git_env(&args, &[])?;
    let kept: BTreeSet<&str> = kept.lines().collect();
    Ok(shallow.iter().any(|commit| !kept.contains(commit.as_str())))
}

/// The trackers `repo` holds as snapshots, each with its shallow commits.
pub fn held(repo: &Utf8Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let shallow = shallow_commits(&["-C", repo.as_str()])?;
    let mut held: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if shallow.is_empty() {
        return Ok(held);
    }
    let refs = run_git_env(
        &[
            "-C",
            repo.as_str(),
            "for-each-ref",
            "--format=%(refname)",
            "refs/newgit/snapshots/",
        ],
        &[],
    )?;
    for name in refs.lines() {
        let Some((tracker, commit)) = name
            .strip_prefix(SNAPSHOT_REFS)
            .and_then(|rest| rest.rsplit_once('/'))
        else {
            continue;
        };
        if shallow.contains(commit) {
            held.entry(tracker.to_owned())
                .or_default()
                .insert(commit.to_owned());
        }
    }
    Ok(held)
}

/// One snapshot fetch or history fetch at a time: two shallow fetches in one
/// repo fight over its `shallow` file. Released when dropped.
pub fn lock(repo: &Utf8Path) -> Result<File> {
    let path = repo.join(".git/newgit-snapshots.lock");
    let lock = File::create(&path).map_err(|source| NewgitError::io(&path, source))?;
    lock.lock()
        .map_err(|source| NewgitError::io(&path, source))?;
    Ok(lock)
}

/// The trackers in `held` a commit of root's history is a snapshot of.
///
/// A shallow commit ends every walk through it, so a commit root shares with
/// a child, once it is on root's history, would silently cut that history
/// short. Those trackers need their history before root can be walked.
pub fn cutting_root(
    repo: &Utf8Path,
    tips: &[String],
    held: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Vec<String>> {
    let walk = root_walk(repo, tips)?;
    Ok(held
        .iter()
        .filter(|(_, commits)| commits.iter().any(|commit| walk.contains(commit)))
        .map(|(tracker, _)| tracker.clone())
        .collect())
}

fn root_walk(repo: &Utf8Path, tips: &[String]) -> Result<HashSet<String>> {
    let mut args = vec!["-C", repo.as_str(), "rev-list"];
    args.extend(tips.iter().map(String::as_str));
    Ok(run_git_env(&args, &[])?
        .lines()
        .map(ToOwned::to_owned)
        .collect())
}

/// Fetch whichever of the commits `root`'s history from `tips` pins, and those
/// commits pin in turn, `repo` does not have, each one commit deep, from
/// the repo `source` names for its tracker (`None`: not one to fetch, as a
/// tracker this person may not read). `extra` (tracker → commit) are fetched
/// the same way, for a commit a list names.
///
/// Returns the commits a repo said it does not have, `(tracker, commit)`.
/// One the store has already is left alone: `--depth` would mark it shallow,
/// and if it is whole, that cuts whatever history it is on.
pub fn fetch_pins(
    repo: &Utf8Path,
    root: &str,
    tips: &[String],
    extra: &BTreeMap<String, String>,
    source: &mut dyn FnMut(&str) -> Result<Option<Source>>,
) -> Result<Vec<(String, String)>> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut level: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut objects = Objects::open(repo)?;
    for commit in root_walk(repo, tips)? {
        let found = objects.commit(&commit)?;
        for (child, pin) in compose::pins_in(&mut objects, &found.tree)? {
            let child = crate::tracker::child_id(root, &child);
            if seen.insert((child.clone(), pin.clone())) {
                level.entry(child).or_default().insert(pin);
            }
        }
    }
    for (tracker, commit) in extra {
        if seen.insert((tracker.clone(), commit.clone())) {
            level
                .entry(tracker.clone())
                .or_default()
                .insert(commit.clone());
        }
    }
    let mut absent = Vec::new();
    // A pin must be here before its own pins can be read, so this goes one
    // level of mounts at a time.
    while !level.is_empty() {
        for (tracker, commits) in &level {
            let mut lacking = lacking(&mut objects, commits)?;
            if lacking.is_empty() {
                continue;
            }
            let Some(from) = source(tracker)? else {
                continue;
            };
            let _lock = lock(repo)?;
            // Whoever held the lock may have brought these in.
            objects = Objects::open(repo)?;
            lacking = self::lacking(&mut objects, commits)?;
            if lacking.is_empty() {
                continue;
            }
            let missing = GitSource::fetch_snapshots(
                &["-C", repo.as_str()],
                &from.url,
                &format!("{SNAPSHOT_REFS}{tracker}/"),
                &lacking,
                Some(&from.auth),
            )?;
            absent.extend(missing.into_iter().map(|id| (tracker.clone(), id)));
            objects = Objects::open(repo)?;
        }
        let mut next: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (tracker, commits) in &level {
            for commit in commits {
                let Some(found) = objects.try_commit(commit)? else {
                    continue;
                };
                for (child, pin) in compose::pins_in(&mut objects, &found.tree)? {
                    let child = crate::tracker::child_id(tracker, &child);
                    if seen.insert((child.clone(), pin.clone())) {
                        next.entry(child).or_default().insert(pin);
                    }
                }
            }
        }
        level = next;
    }
    Ok(absent)
}

fn lacking(objects: &mut Objects, commits: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut lacking = Vec::new();
    for commit in commits {
        if !objects.has_commit(commit)? {
            lacking.push(commit.clone());
        }
    }
    Ok(lacking)
}
