//! The Git endpoint's work inside its container (docs/design/trackers.md, *The Git
//! endpoint*). The Worker decides who may read what and hands the container a
//! plan: each branch's index (root's commit and every pin), cut down to the
//! trackers this reader may read, and where to fetch every tracker. This
//! composes each branch from its root commit, expanding only those trackers,
//! into a bare repository holding exactly that — nothing a tracker the reader
//! may not read put there — for `git upload-pack` to serve. A pin is
//! expanded only if its commit is in that tracker's own repo: a parent
//! cannot pin another tracker's commit under a name its reader may read.
//!
//! Views are cached by plan: the same lists compose to the same repository,
//! so a second fetch of an unchanged branch serves at once.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::compose;
use crate::error::{NewgitError, Result};
use crate::source::{GitSource, run_git_env};
use crate::tracker::ROOT_TRACKER;

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    /// Where the container keeps its repositories.
    pub dir: Utf8PathBuf,
    /// Branch → tracker → commit: each branch's index, only the trackers
    /// this reader may read, root among them.
    pub branches: BTreeMap<String, BTreeMap<String, String>>,
    pub default_branch: String,
    /// Tracker → where to fetch it: every tracker listed above, and `access`
    /// always — where each pin is mounted is in its content, which the
    /// server reads even when the reader may not, and never serves unless
    /// listed above.
    pub repos: BTreeMap<String, Source>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub url: String,
    /// The `Authorization` header Git sends.
    pub auth: String,
}

/// Compose `plan` into a bare repository and return its path.
pub fn compose_view(plan: &Plan) -> Result<Utf8PathBuf> {
    let store = plan.dir.join("trackers.git");
    let views = plan.dir.join("views");
    std::fs::create_dir_all(&views).map_err(|source| NewgitError::io(&views, source))?;
    if !store.join("HEAD").exists() {
        run_git_env(&["init", "-q", "--bare", store.as_str()], &[])?;
    }

    // Fetch each tracker whose commits are not in its own history here yet.
    // A commit counts for a tracker only if that tracker's repo has it: the
    // store is shared by every reader of this project, so a commit being
    // *somewhere* in it proves nothing — a branch could name another
    // tracker's commit under this tracker's name.
    let mut wanted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for list in plan.branches.values() {
        for (tracker, commit) in list {
            wanted.entry(tracker).or_default().push(commit);
        }
    }
    for tracker in plan.repos.keys() {
        wanted.entry(tracker).or_default();
    }
    for (tracker, commits) in &wanted {
        if !commits.is_empty()
            && commits
                .iter()
                .all(|commit| in_tracker(&store, tracker, commit))
        {
            continue;
        }
        let source = plan.repos.get(*tracker).ok_or_else(|| {
            NewgitError::Unsupported(format!("the plan names `{tracker}` but not where it is"))
        })?;
        GitSource::fetch_refs(
            &[&format!("--git-dir={store}")],
            &source.url,
            &[format!("+refs/heads/*:refs/trackers/{tracker}/heads/*")],
            Some(&source.auth),
        )?;
    }
    // A branch naming a commit its tracker does not have is left out —
    // logged, not served, and not allowed to take the others down with it.
    let mut plan = plan.clone();
    // A child naming a commit its tracker does not have is left out of
    // that branch, as for a reader who may not read it; a root commit that is
    // not root's leaves the branch unserved. Logged either way.
    plan.branches.retain(|branch, list| {
        let bad: Vec<String> = list
            .iter()
            .filter(|(tracker, commit)| !in_tracker(&store, tracker, commit))
            .map(|(tracker, _)| tracker.clone())
            .collect();
        if !bad.is_empty() {
            eprintln!("newgit: branch `{branch}` names commits its trackers do not have: {bad:?}");
        }
        for tracker in &bad {
            if tracker != ROOT_TRACKER {
                list.remove(tracker);
            }
        }
        !bad.iter().any(|tracker| tracker == ROOT_TRACKER) && list.contains_key(ROOT_TRACKER)
    });
    if !plan.branches.contains_key(&plan.default_branch) {
        return Err(NewgitError::Unsupported(format!(
            "branch `{}` names commits its trackers do not have",
            plan.default_branch
        )));
    }
    let plan = &plan;

    let key = plan_key(plan);
    let view = views.join(format!("{key}.git"));
    if view.join("HEAD").exists() {
        return Ok(view);
    }

    // Each tracker's own history, once: what a pin may name.
    let mut histories: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
    for tracker in plan.repos.keys() {
        let commits = run_git_env(
            &[
                &format!("--git-dir={store}"),
                "rev-list",
                &format!("--glob=refs/trackers/{tracker}/*"),
            ],
            &[],
        )
        .unwrap_or_default();
        histories.insert(
            tracker.clone(),
            commits.lines().map(ToOwned::to_owned).collect(),
        );
    }
    for (branch, list) in &plan.branches {
        let readable = |tracker: &str| list.contains_key(tracker);
        let admit = |tracker: &str, commit: &str| {
            histories
                .get(tracker)
                .is_some_and(|history| history.contains(commit))
        };
        compose::compose(
            &store,
            ROOT_TRACKER,
            &[(
                format!("refs/views/{key}/heads/{branch}"),
                list[ROOT_TRACKER].clone(),
            )],
            &compose::ComposeOptions {
                readable: &readable,
                admit: Some(&admit),
            },
        )?;
    }

    let notes_ref = format!("refs/views/{key}/notes");
    let has_notes = view_notes(&store, &key, &notes_ref)?;

    // A repository of its own, so it holds only what these views reach: a
    // fetch copies nothing else, and upload-pack serves nothing else.
    let staging = views.join(format!("{key}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    run_git_env(&["init", "-q", "--bare", staging.as_str()], &[])?;
    run_git_env(
        &[
            &format!("--git-dir={staging}"),
            "fetch",
            "-q",
            store.as_str(),
            &format!("+refs/views/{key}/heads/*:refs/heads/*"),
        ],
        &[],
    )?;
    if has_notes {
        // Git's default notes ref: `git log` shows a rebuilt commit's
        // canonical id once a clone fetches `refs/notes/*`.
        run_git_env(
            &[
                &format!("--git-dir={staging}"),
                "fetch",
                "-q",
                store.as_str(),
                &format!("+{notes_ref}:refs/notes/commits"),
            ],
            &[],
        )?;
    }
    run_git_env(
        &[
            &format!("--git-dir={staging}"),
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{}", plan.default_branch),
        ],
        &[],
    )?;
    match std::fs::rename(&staging, &view) {
        Ok(()) => Ok(view),
        // Another request composed the same view first: it is the same.
        Err(_) if view.join("HEAD").exists() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(view)
        }
        Err(source) => Err(NewgitError::io(&view, source)),
    }
}

/// The canonical-id notes for just the commits these views hold, as one
/// notes commit at `notes_ref`: a reader learns nothing about commits outside
/// their view. False when no commit of theirs was rebuilt.
fn view_notes(store: &Utf8Path, key: &str, notes_ref: &str) -> Result<bool> {
    let git_dir = format!("--git-dir={store}");
    let reachable: std::collections::BTreeSet<String> = run_git_env(
        &[
            &git_dir,
            "rev-list",
            &format!("--glob=refs/views/{key}/heads/*"),
        ],
        &[],
    )?
    .lines()
    .map(ToOwned::to_owned)
    .collect();
    let listing = run_git_env(
        &[
            &git_dir,
            "notes",
            "--ref",
            compose::CANONICAL_NOTES_REF,
            "list",
        ],
        &[],
    )
    .unwrap_or_default();
    let mut stream = Vec::new();
    let mut any = false;
    for line in listing.lines() {
        let Some((blob, commit)) = line.split_once(' ') else {
            continue;
        };
        if !reachable.contains(commit) {
            continue;
        }
        if !any {
            stream.extend_from_slice(
                format!(
                    "commit {notes_ref}\ncommitter newgit <newgit@localhost> 946684800 +0000\ndata 9\ncanonical\n"
                )
                .as_bytes(),
            );
            any = true;
        }
        let note = run_git_env(&[&git_dir, "cat-file", "-p", blob], &[])?;
        let note = format!("{}\n", note.trim_end());
        stream.extend_from_slice(
            format!("N inline {commit}\ndata {}\n{note}\n", note.len()).as_bytes(),
        );
    }
    if !any {
        return Ok(false);
    }
    stream.push(b'\n');
    let mut child = std::process::Command::new("git")
        .args([git_dir.as_str(), "fast-import", "--quiet", "--force"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| NewgitError::io(store.to_path_buf(), source))?;
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(&stream)
            .map_err(|source| NewgitError::io(store.to_path_buf(), source))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|source| NewgitError::io(store.to_path_buf(), source))?;
    if !output.status.success() {
        return Err(NewgitError::SourceCommand {
            command: "git fast-import (view notes)".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(true)
}

/// Whether `commit` is in `tracker`'s own history here: reachable from what
/// was fetched from that tracker's repo.
fn in_tracker(store: &Utf8Path, tracker: &str, commit: &str) -> bool {
    run_git_env(
        &[
            &format!("--git-dir={store}"),
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            &format!("--contains={commit}"),
            &format!("refs/trackers/{tracker}/"),
        ],
        &[],
    )
    .is_ok_and(|found| !found.trim().is_empty())
}

/// The same lists compose to the same view.
fn plan_key(plan: &Plan) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plan.default_branch.as_bytes());
    for (branch, list) in &plan.branches {
        hasher.update(format!("\n{branch}").as_bytes());
        for (tracker, commit) in list {
            hasher.update(format!(" {tracker}={commit}").as_bytes());
        }
    }
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the Worker asks before a merge: for each pin the merge moves, does
/// the new commit descend from the old one, and is it on that tracker's own
/// `main`? The Worker cannot run Git; this container can.
#[derive(Debug, Clone, Deserialize)]
pub struct AncestryPlan {
    pub dir: Utf8PathBuf,
    pub repos: BTreeMap<String, Source>,
    pub checks: Vec<AncestryCheck>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AncestryCheck {
    pub tracker: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AncestryAnswer {
    pub tracker: String,
    pub forward: bool,
    #[serde(rename = "onMain")]
    pub on_main: bool,
}

/// Answer `plan` from each tracker's own repo, fetched fresh into the
/// container's store. A commit the repo does not have is neither forward nor
/// on `main`.
pub fn check_ancestry(plan: &AncestryPlan) -> Result<Vec<AncestryAnswer>> {
    let store = plan.dir.join("trackers.git");
    std::fs::create_dir_all(&plan.dir).map_err(|source| NewgitError::io(&plan.dir, source))?;
    if !store.join("HEAD").exists() {
        run_git_env(&["init", "-q", "--bare", store.as_str()], &[])?;
    }
    let git_dir = format!("--git-dir={store}");
    let mut answers = Vec::new();
    for check in &plan.checks {
        let source = plan.repos.get(&check.tracker).ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "the plan checks `{}` but not where it is",
                check.tracker
            ))
        })?;
        // Fresh and pruned: a head deleted since the last fetch is not `main`.
        GitSource::fetch_refs(
            &[&git_dir, "-c", "fetch.prune=true"],
            &source.url,
            &[format!(
                "+refs/heads/*:refs/trackers/{}/heads/*",
                check.tracker
            )],
            Some(&source.auth),
        )?;
        let in_repo = |commit: &str| in_tracker(&store, &check.tracker, commit);
        let ancestor = |old: &str, new: &str| {
            std::process::Command::new("git")
                .args([&git_dir, "merge-base", "--is-ancestor", old, new])
                .output()
                .is_ok_and(|output| output.status.success())
        };
        let main = format!("refs/trackers/{}/heads/main", check.tracker);
        let known = in_repo(&check.new);
        answers.push(AncestryAnswer {
            tracker: check.tracker.clone(),
            forward: known && in_repo(&check.old) && ancestor(&check.old, &check.new),
            on_main: known && ancestor(&check.new, &main),
        });
    }
    Ok(answers)
}

/// What the Worker asks before it writes a branch's index: is `list` exactly
/// the index of root commit `list["root"]` as root's repo holds it — every
/// pin, recursively, as the gitlinks in those commits say?
#[derive(Debug, Clone, Deserialize)]
pub struct IndexPlan {
    pub dir: Utf8PathBuf,
    pub repos: BTreeMap<String, Source>,
    pub list: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexAnswer {
    pub matches: bool,
    /// The index the commits give, as far as the repos here hold them.
    pub actual: BTreeMap<String, String>,
}

pub fn check_index(plan: &IndexPlan) -> Result<IndexAnswer> {
    let store = plan.dir.join("trackers.git");
    std::fs::create_dir_all(&plan.dir).map_err(|source| NewgitError::io(&plan.dir, source))?;
    if !store.join("HEAD").exists() {
        run_git_env(&["init", "-q", "--bare", store.as_str()], &[])?;
    }
    let git_dir = format!("--git-dir={store}");
    for (tracker, source) in &plan.repos {
        GitSource::fetch_refs(
            &[&git_dir, "-c", "fetch.prune=true"],
            &source.url,
            &[format!("+refs/heads/*:refs/trackers/{tracker}/heads/*")],
            Some(&source.auth),
        )?;
    }
    let Some(root) = plan.list.get(ROOT_TRACKER) else {
        return Ok(IndexAnswer {
            matches: false,
            actual: BTreeMap::new(),
        });
    };
    if !in_tracker(&store, ROOT_TRACKER, root) {
        return Ok(IndexAnswer {
            matches: false,
            actual: BTreeMap::new(),
        });
    }
    let actual = compose::index(&store, ROOT_TRACKER, root)?;
    Ok(IndexAnswer {
        matches: actual == plan.list,
        actual,
    })
}
