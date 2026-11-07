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
//!
//! A view needs root's history and, of every child, only the commits that
//! history pins, so that is all composing fetches ([`Stores`]). What needs a
//! child's history (a merge's ancestry check, a fork's copy, a mirror's
//! pull) fetches it whole, into a store of its own, in the background.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::compose;
use crate::error::{NewgitError, Result};
use crate::export_target::ExportTarget;
use crate::objects::Objects;
use crate::source::{GitSource, run_git_env};
use crate::tracker::{ROOT_TRACKER, TrackerDefinition};

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

/// The container's repositories. Three, because a shallow commit must never
/// meet a question about history (a fork's copy has a fourth, `copies.git`,
/// for the same reason: see [`copy_repo`]):
///
/// - `trackers.git` holds whole histories: root's, and any tracker a check
///   or a pull needed. Every question about ancestry is asked here.
/// - `snapshots.git` is shallow on purpose: pinned commits, one commit deep,
///   each under `refs/snapshots/<tracker>/<commit>` as proof that tracker's
///   repo has it. It holds nothing else. Its `shallow` file would cut any
///   walk that met one of its commits, so root is never here: a commit root
///   shares with a child would end root's history.
/// - `compose.git` reads both through alternates and holds what composing
///   writes. It never fetches: Git takes an alternate's refs for complete
///   history, which is why this is the only repository that has one.
struct Stores {
    whole: Utf8PathBuf,
    snapshots: Utf8PathBuf,
    composing: Utf8PathBuf,
}

/// Tracker → the commits proved to be in its own repo.
type Held = BTreeMap<String, HashSet<String>>;

impl Stores {
    fn open(dir: &Utf8Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| NewgitError::io(dir, source))?;
        let stores = Self {
            whole: dir.join("trackers.git"),
            snapshots: dir.join("snapshots.git"),
            composing: dir.join("compose.git"),
        };
        for store in [&stores.whole, &stores.snapshots, &stores.composing] {
            if !store.join("HEAD").exists() {
                run_git_env(&["init", "-q", "--bare", store.as_str()], &[])?;
            }
        }
        // Renamed in, so a compose running beside this never reads it empty
        // and takes every pin for missing.
        let info = stores.composing.join("objects/info");
        let alternates = info.join("alternates");
        if !alternates.exists() {
            let staging = info.join(format!("alternates.tmp-{}", std::process::id()));
            let text = format!("{}/objects\n{}/objects\n", stores.whole, stores.snapshots);
            std::fs::create_dir_all(&info).map_err(|source| NewgitError::io(&info, source))?;
            std::fs::write(&staging, text).map_err(|source| NewgitError::io(&staging, source))?;
            std::fs::rename(&staging, &alternates)
                .map_err(|source| NewgitError::io(&alternates, source))?;
        }
        Ok(stores)
    }

    /// What is proved for `trackers`: each one's history in the whole store,
    /// and its snapshots.
    fn held<'a>(&self, trackers: impl Iterator<Item = &'a String>) -> Held {
        let mut held = tracker_histories(&self.whole, trackers);
        for (tracker, commits) in &mut held {
            commits.extend(self.snapshots_of(tracker));
        }
        held
    }

    fn snapshots_of(&self, tracker: &str) -> Vec<String> {
        run_git_env(
            &[
                &format!("--git-dir={}", self.snapshots),
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/snapshots/{tracker}/"),
            ],
            &[],
        )
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
    }

    /// Fetch whichever of `wanted` (tracker → commits) is not held yet, and
    /// add it to `held`. A commit its tracker's repo does not have stays out
    /// of `held`. One request at a time: two shallow fetches in one store
    /// fight over its `shallow` file.
    ///
    /// A commit the remote refuses by id may still be in the repo, since a
    /// remote need not serve what no branch names, and a pin wrongly left
    /// out stays out for as long as the view is cached. So a refusal is
    /// settled by that tracker's whole history, once the lock is let go: that
    /// can take minutes, and nobody else should wait on it.
    fn snapshot(
        &self,
        repos: &BTreeMap<String, Source>,
        wanted: &BTreeMap<String, BTreeSet<String>>,
        held: &mut Held,
    ) -> Result<()> {
        let lacking = |held: &Held, tracker: &str, commits: &BTreeSet<String>| -> Vec<String> {
            let known = held.get(tracker);
            commits
                .iter()
                .filter(|commit| !known.is_some_and(|known| known.contains(*commit)))
                .cloned()
                .collect()
        };
        if wanted
            .iter()
            .all(|(tracker, commits)| lacking(held, tracker, commits).is_empty())
        {
            return Ok(());
        }
        let path = self.snapshots.join("newgit-fetch.lock");
        let lock = std::fs::File::create(&path).map_err(|source| NewgitError::io(&path, source))?;
        lock.lock()
            .map_err(|source| NewgitError::io(&path, source))?;
        let git_dir = format!("--git-dir={}", self.snapshots);
        let mut refused: BTreeMap<&String, (&Source, Vec<String>)> = BTreeMap::new();
        for (tracker, commits) in wanted {
            // Whoever held the lock may have brought these in.
            let known = held.entry(tracker.clone()).or_default();
            known.extend(self.snapshots_of(tracker));
            let missing = lacking(held, tracker, commits);
            let Some(source) = repos.get(tracker).filter(|_| !missing.is_empty()) else {
                continue;
            };
            let absent = GitSource::fetch_snapshots(
                &[&git_dir],
                &source.url,
                &format!("refs/snapshots/{tracker}/"),
                &missing,
                Some(&source.auth),
            )?;
            let known = held.entry(tracker.clone()).or_default();
            known.extend(
                missing
                    .into_iter()
                    .filter(|commit| !absent.contains(commit)),
            );
            if !absent.is_empty() {
                refused.insert(tracker, (source, absent));
            }
        }
        drop(lock);
        for (tracker, (source, commits)) in refused {
            eprintln!(
                "newgit: `{tracker}`'s repo refused pinned commits {commits:?}: fetching its \
                 whole history to settle it"
            );
            GitSource::fetch_refs(
                &[&format!("--git-dir={}", self.whole)],
                &source.url,
                &[format!("+refs/heads/*:refs/trackers/{tracker}/heads/*")],
                Some(&source.auth),
            )?;
            let (reachable, absent): (Vec<String>, Vec<String>) = commits
                .into_iter()
                .partition(|commit| in_tracker(&self.whole, tracker, commit));
            if !absent.is_empty() {
                eprintln!("newgit: `{tracker}`'s repo does not have pinned commits {absent:?}");
            }
            held.entry(tracker.clone()).or_default().extend(reachable);
        }
        Ok(())
    }

    /// Fetch every commit `commits` (root's) pin, and every commit those
    /// pin in turn, for the trackers in `repos`: a reader's others are not
    /// theirs to expand. A pin must be here before its own pins can be read,
    /// so this goes one level of mounts at a time.
    fn snapshot_pins(
        &self,
        repos: &BTreeMap<String, Source>,
        commits: &[String],
        held: &mut Held,
    ) -> Result<()> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut level: BTreeMap<String, BTreeSet<String>> =
            BTreeMap::from([(ROOT_TRACKER.to_owned(), commits.iter().cloned().collect())]);
        loop {
            let mut next: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut objects = Objects::open(&self.composing)?;
            for (tracker, commits) in &level {
                for commit in commits {
                    if tracker != ROOT_TRACKER
                        && !held.get(tracker).is_some_and(|set| set.contains(commit))
                    {
                        continue;
                    }
                    let Some(found) = objects.try_commit(commit)? else {
                        continue;
                    };
                    for (child, pin) in compose::pins_in(&mut objects, &found.tree)? {
                        let child = crate::tracker::child_id(tracker, &child);
                        if repos.contains_key(&child) && seen.insert((child.clone(), pin.clone())) {
                            next.entry(child).or_default().insert(pin);
                        }
                    }
                }
            }
            if next.is_empty() {
                return Ok(());
            }
            self.snapshot(repos, &next, held)?;
            level = next;
        }
    }
}

/// Compose `plan` into a bare repository and return its path.
pub fn compose_view(plan: &Plan) -> Result<Utf8PathBuf> {
    let stores = Stores::open(&plan.dir)?;
    let views = plan.dir.join("views");
    std::fs::create_dir_all(&views).map_err(|source| NewgitError::io(&views, source))?;

    let mut wanted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for list in plan.branches.values() {
        for (tracker, commit) in list {
            wanted
                .entry(tracker.clone())
                .or_default()
                .insert(commit.clone());
        }
    }
    if let Some(tracker) = wanted.keys().find(|name| !plan.repos.contains_key(*name)) {
        return Err(NewgitError::Unsupported(format!(
            "the plan names `{tracker}` but not where it is"
        )));
    }
    let mut held = stores.held(plan.repos.keys());

    // Root's history is the view's shape, so all of it, unless every branch's
    // commit is in it already. A commit counts for a tracker only if that
    // tracker's repo has it: a commit being *somewhere* here proves nothing
    // — a branch could name another tracker's commit under this tracker's
    // name.
    let roots = wanted.remove(ROOT_TRACKER).unwrap_or_default();
    let known = |held: &Held, commit: &String| {
        held.get(ROOT_TRACKER)
            .is_some_and(|history| history.contains(commit))
    };
    if !roots.iter().all(|commit| known(&held, commit)) {
        let source = plan.repos.get(ROOT_TRACKER).ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "the plan names `{ROOT_TRACKER}` but not where it is"
            ))
        })?;
        GitSource::fetch_refs(
            &[&format!("--git-dir={}", stores.whole)],
            &source.url,
            &[format!(
                "+refs/heads/*:refs/trackers/{ROOT_TRACKER}/heads/*"
            )],
            Some(&source.auth),
        )?;
        held.extend(tracker_histories(
            &stores.whole,
            [ROOT_TRACKER.to_owned()].iter(),
        ));
    }
    // What each branch names now, so a view composed already serves at once.
    stores.snapshot(&plan.repos, &wanted, &mut held)?;

    // A branch naming a commit its tracker does not have is left out —
    // logged, not served, and not allowed to take the others down with it.
    let mut plan = plan.clone();
    // A child naming a commit its tracker does not have is left out of
    // that branch, as for a reader who may not read it; a root commit that is
    // not root's leaves the branch unserved. Logged either way.
    plan.branches.retain(|branch, list| {
        let bad: Vec<String> = list
            .iter()
            .filter(|(tracker, commit)| {
                !held
                    .get(*tracker)
                    .is_some_and(|commits| commits.contains(*commit))
            })
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

    // Composing is about to happen: every commit root's history pins, not
    // only what the branches name now.
    let store = stores.composing.clone();
    let tips: Vec<String> = plan
        .branches
        .values()
        .map(|list| list[ROOT_TRACKER].clone())
        .collect();
    let mut walk = vec![
        format!("--git-dir={store}"),
        "rev-list".to_owned(),
        "--topo-order".to_owned(),
    ];
    walk.extend(tips);
    let walk: Vec<&str> = walk.iter().map(String::as_str).collect();
    let history: Vec<String> = run_git_env(&walk, &[])?
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    stores.snapshot_pins(&plan.repos, &history, &mut held)?;

    for (branch, list) in &plan.branches {
        let readable = |tracker: &str| list.contains_key(tracker);
        let admit = |tracker: &str, commit: &str| {
            held.get(tracker)
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

/// Each tracker's own history: what a pin may name.
fn tracker_histories<'a>(
    store: &Utf8Path,
    trackers: impl Iterator<Item = &'a String>,
) -> BTreeMap<String, HashSet<String>> {
    let mut histories = BTreeMap::new();
    for tracker in trackers {
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
    histories
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
    let stores = Stores::open(&plan.dir)?;
    if let Some(source) = plan.repos.get(ROOT_TRACKER) {
        GitSource::fetch_refs(
            &[
                &format!("--git-dir={}", stores.whole),
                "-c",
                "fetch.prune=true",
            ],
            &source.url,
            // A merge's root commit waits at its landing ref until it lands.
            &[
                format!("+refs/heads/*:refs/trackers/{ROOT_TRACKER}/heads/*"),
                format!("+refs/newgit/landing/*:refs/trackers/{ROOT_TRACKER}/landing/*"),
            ],
            Some(&source.auth),
        )?;
    }
    let Some(root) = plan.list.get(ROOT_TRACKER) else {
        return Ok(IndexAnswer {
            matches: false,
            actual: BTreeMap::new(),
        });
    };
    if !in_tracker(&stores.whole, ROOT_TRACKER, root) {
        return Ok(IndexAnswer {
            matches: false,
            actual: BTreeMap::new(),
        });
    }
    // An index is this one commit's pins, all the way down: no history.
    let mut held = stores.held(plan.repos.keys());
    stores.snapshot_pins(&plan.repos, std::slice::from_ref(root), &mut held)?;
    let actual = compose::index(&stores.composing, ROOT_TRACKER, root)?;
    Ok(IndexAnswer {
        matches: actual == plan.list,
        actual,
    })
}

/// What the Worker asks when an instance first changes a tracker in Git:
/// copy every branch of the tracker's repo (`from`) into the instance's
/// fork (`to`), an empty repo the Worker made. Artifacts' own fork never
/// finished on a large history (Spheroman/t3code, 365 MiB), so the copy is
/// Git's: a fetch that steps and retries, and a push that steps past
/// Artifacts' per-push limit.
#[derive(Debug, Clone, Deserialize)]
pub struct CopyPlan {
    pub dir: Utf8PathBuf,
    pub from: Source,
    pub to: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CopyAnswer {
    /// Every branch copied, and the commit it is at.
    pub branches: BTreeMap<String, String>,
}

/// Copy `plan.from`'s branches into `plan.to`. Idempotent: run again after a
/// copy that stopped part way, the fetch takes only what the store lacks and
/// the push only what the fork lacks, so it finishes the job.
///
/// It has a store of its own. A copy Artifacts drops comes in bands, shallow
/// until the last one, for as long as half an hour. In `trackers.git` that
/// would hand a merge's check a shallow history, and every other fetch there
/// would take the store for one left part way and deepen it too.
pub fn copy_repo(plan: &CopyPlan) -> Result<CopyAnswer> {
    let store = plan.dir.join("copies.git");
    std::fs::create_dir_all(&plan.dir).map_err(|source| NewgitError::io(&plan.dir, source))?;
    if !store.join("HEAD").exists() {
        run_git_env(&["init", "-q", "--bare", store.as_str()], &[])?;
    }
    let git_dir = format!("--git-dir={store}");
    // Each source under its own refs: forks of two trackers share this store.
    let mut hasher = Sha256::new();
    hasher.update(plan.from.url.as_bytes());
    let key: String = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let prefix = format!("refs/copies/{key}/heads/");
    GitSource::fetch_refs(
        &[&git_dir, "-c", "fetch.prune=true"],
        &plan.from.url,
        &[format!("+refs/heads/*:{prefix}*")],
        Some(&plan.from.auth),
    )?;
    let listing = run_git_env(
        &[
            &git_dir,
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &prefix,
        ],
        &[],
    )?;
    let branches: BTreeMap<String, String> = listing
        .lines()
        .filter_map(|line| {
            let (name, commit) = line.split_once(' ')?;
            Some((name.strip_prefix(&prefix)?.to_owned(), commit.to_owned()))
        })
        .collect();
    if branches.is_empty() {
        return Ok(CopyAnswer { branches });
    }
    // Forced: the fork is the Worker's own until the copy is done, so a
    // branch left behind by an earlier copy is only ever moved to the source's.
    let refspecs: Vec<String> = branches
        .keys()
        .map(|branch| format!("+{prefix}{branch}:refs/heads/{branch}"))
        .collect();
    GitSource::push_refs(&[&git_dir], &plan.to.url, &refspecs, Some(&plan.to.auth))?;
    Ok(CopyAnswer { branches })
}

/// What the Worker hands the container when a mirror's branch moved: bring
/// `mirror.commit` into the project, as `newgit pull --remote` does for a
/// target without `root`, and land it on `main`. The merge lock on `main` is
/// the Worker's, held for as long as this runs.
#[derive(Debug, Clone, Deserialize)]
pub struct PullPlan {
    pub dir: Utf8PathBuf,
    /// Tracker → where it is: every tracker `list` names, and `access`
    /// always. Trackers in `writable` carry a write password.
    pub repos: BTreeMap<String, Source>,
    /// `main`'s index as the lock found it.
    pub list: BTreeMap<String, String>,
    /// The trackers this event may change: what whoever merged it maintains.
    pub writable: Vec<String>,
    /// The export target: `.newgit/exports/<target>.toml` at `list`'s root
    /// commit says which trackers the mirror ships.
    pub target: String,
    pub mirror: Mirror,
    /// Instances whose work in a tracker the mirror withholds may be behind
    /// this commit. The mirror never saw that work, so landing the commit
    /// would drop it.
    #[serde(default)]
    pub instances: Vec<MirrorInstance>,
    /// Only say whether there is anything to land: no lock is held and
    /// nothing moves, so an event for our own export costs no merge a wait.
    #[serde(default)]
    pub peek: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MirrorInstance {
    pub name: String,
    /// Withheld tracker → the commit the instance holds. Empty when its
    /// fork exists and no checkpoint says what is on it.
    pub pins: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mirror {
    pub url: String,
    pub branch: String,
    /// What the event says the branch moved to: exactly what lands.
    pub commit: String,
}

/// How a pull ended. A refusal is an answer, not a failure: the commit is
/// what it is, and running it again gives the same refusal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum PullAnswer {
    /// The commit is `main`'s already: an export of ours, or a redelivery.
    Nothing,
    /// A peek found something to land.
    Work,
    /// Ready to land: every commit is at its landing ref. The Worker lands
    /// `list` as `main`'s index, then moves root's `main` and makes `moves`.
    Landed {
        list: BTreeMap<String, String>,
        commits: usize,
        moves: Vec<crate::remote::RefMove>,
    },
    /// Nothing moved, for this reason.
    Refused { why: String },
}

/// Pull a mirror's commit into `main`. Safe to run again: a run that stopped
/// after moving a ref makes the same commits and finds them there.
pub fn pull_mirror(plan: &PullPlan) -> Result<PullAnswer> {
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
    match land(plan, &store) {
        Err(NewgitError::SplitRefused(why) | NewgitError::Unsupported(why)) => {
            Ok(PullAnswer::Refused { why })
        }
        other => other,
    }
}

fn land(plan: &PullPlan, store: &Utf8Path) -> Result<PullAnswer> {
    let git_dir = format!("--git-dir={store}");
    let git = |args: &[&str]| -> Result<String> {
        let mut full = vec![git_dir.as_str()];
        full.extend_from_slice(args);
        Ok(run_git_env(&full, &[])?.trim().to_owned())
    };
    let is_ancestor = |older: &str, newer: &str| {
        std::process::Command::new("git")
            .args([&git_dir, "merge-base", "--is-ancestor", older, newer])
            .output()
            .is_ok_and(|output| output.status.success())
    };
    let base = plan
        .list
        .get(ROOT_TRACKER)
        .ok_or_else(|| NewgitError::Unsupported("`main` names no root commit".to_owned()))?;

    let mirror_ref = format!("refs/mirrors/{}/tip", plan.target);
    GitSource::fetch_refs(
        &[&git_dir],
        &plan.mirror.url,
        &[format!("+refs/heads/{}:{mirror_ref}", plan.mirror.branch)],
        None,
    )?;
    let commit = &plan.mirror.commit;
    if !git(&["cat-file", "-t", commit]).is_ok_and(|kind| kind == "commit")
        || !is_ancestor(commit, &mirror_ref)
    {
        return Err(NewgitError::Unsupported(format!(
            "`{}` on {} no longer holds commit {}",
            plan.mirror.branch, plan.mirror.url, commit
        )));
    }

    // What the target ships, as root's `main` says.
    let main_tree = {
        let mut objects = Objects::open(store)?;
        let tree = compose::view_tree(&mut objects, base)?;
        objects.flush()?;
        tree
    };
    let all = compose::view_definitions(store, ROOT_TRACKER, &main_tree)?;
    let target = read_target(store, base, &plan.target)?;
    if let Some(root) = &target.root {
        return Err(NewgitError::Unsupported(format!(
            "`{}` is rooted at `{root}`: its commits are that tracker's own, and `newgit pull \
             --remote` brings them in",
            target.name
        )));
    }
    let tracked: Vec<&TrackerDefinition> = all.iter().filter(|definition| definition.git).collect();
    let ships = |name: &str| {
        tracked
            .iter()
            .find(|definition| definition.name == name)
            .is_some_and(|definition| target.lists(&definition.name, &definition.audience))
    };
    if !ships(ROOT_TRACKER) {
        return Err(NewgitError::Unsupported(format!(
            "`{}` does not ship `{ROOT_TRACKER}`, so its commits are not this project's",
            target.name
        )));
    }

    // `main` as the target sees it: composing is deterministic, so a commit
    // that came from here is in this history under the very id it has there.
    let histories = tracker_histories(store, plan.repos.keys());
    let admit = |tracker: &str, commit: &str| {
        histories
            .get(tracker)
            .is_some_and(|history| history.contains(commit))
    };
    let view_of = |view_ref: &str, root: &str, readable: &dyn Fn(&str) -> bool| {
        compose::compose(
            store,
            ROOT_TRACKER,
            &[(view_ref.to_owned(), root.to_owned())],
            &compose::ComposeOptions {
                readable,
                admit: Some(&admit),
            },
        )?
        .into_iter()
        .next()
        .and_then(|view| view.tip)
        .ok_or_else(|| NewgitError::Unsupported(format!("root commit {root} composes to nothing")))
    };
    let everything = |_: &str| true;
    let exported = view_of("refs/mirrors/view/target", base, &ships)?;
    let walk: Vec<String> = git(&[
        "rev-list",
        "--topo-order",
        "--reverse",
        commit,
        "--not",
        &exported,
    ])?
    .lines()
    .map(ToOwned::to_owned)
    .collect();
    if walk.is_empty() {
        return Ok(PullAnswer::Nothing);
    }

    // Work only an instance holds: the branch that was merged on the mirror
    // left it out, and put back from `main` it would vanish without a word.
    // Behind `main`, or already on it, there is nothing to lose.
    for instance in &plan.instances {
        for (tracker, pin) in &instance.pins {
            let on_main = plan
                .list
                .get(tracker)
                .is_some_and(|main| !pin.is_empty() && is_ancestor(pin, main));
            if !on_main {
                return Err(NewgitError::SplitRefused(format!(
                    "commit {commit} on `{}` may hold `{}`'s work, and that instance has work in \
                     `{tracker}`, which that target does not ship, so landing it would lose that \
                     work. Nothing moved: `newgit merge {}`, then redeliver this event",
                    target.name, instance.name, instance.name
                )));
            }
        }
    }
    if plan.peek {
        return Ok(PullAnswer::Work);
    }

    let main_view = view_of("refs/mirrors/view/main", base, &everything)?;
    let withheld: Vec<TrackerDefinition> = tracked
        .iter()
        .filter(|definition| !ships(&definition.name))
        .map(|definition| (*definition).clone())
        .collect();
    let mut views: BTreeMap<String, String> = BTreeMap::new();
    let mapped = compose::bring_in(store, &target.name, &walk, &withheld, &mut |id| {
        if walk.iter().any(|walked| walked == id) {
            return Ok(None);
        }
        let Some((_, root)) = compose::read_root(store, id)? else {
            return Ok(None);
        };
        if let Some(view) = views.get(&root) {
            return Ok(Some(view.clone()));
        }
        let view = view_of("refs/mirrors/view/parent", &root, &everything)?;
        views.insert(root, view.clone());
        Ok(Some(view))
    })?;
    let tip = mapped
        .get(commit)
        .cloned()
        .ok_or_else(|| NewgitError::Unsupported(format!("commit {commit} did not come in")))?;
    if is_ancestor(&tip, &main_view) {
        return Ok(PullAnswer::Nothing);
    }
    if let Some((found, path, owner)) =
        compose::find_outside_git(store, &[&tip, "--not", &main_view], &[], &all)?
    {
        return Err(NewgitError::SplitRefused(format!(
            "commit {found} on `{}` adds `{path}`, which belongs to `{owner}`, a tracker outside \
             Git: its files (secrets among them) never come in or leave through Git, so nothing \
             was pulled",
            target.name
        )));
    }

    // The commits this makes are the same every time, so a run that stopped
    // part way finds its own commits on root's ref.
    let split_one = |view: &str| -> Result<String> {
        let bound = |_: &str| true;
        let audience = |tracker: &str| {
            all.iter()
                .find(|definition| definition.name == tracker)
                .map(|definition| definition.audience.clone())
        };
        Ok(compose::split(
            store,
            view,
            &compose::SplitOptions {
                root: ROOT_TRACKER,
                bound: &bound,
                audience: &audience,
                newgit_made: None,
                need_history: &|_| Ok(()),
            },
        )?
        .root)
    };
    let date = git(&["log", "-1", "--format=%ct +0000", commit])?;
    let identity: Vec<(String, String)> = [
        ("GIT_AUTHOR_NAME", "newgit"),
        ("GIT_AUTHOR_EMAIL", "newgit@localhost"),
        ("GIT_COMMITTER_NAME", "newgit"),
        ("GIT_COMMITTER_EMAIL", "newgit@localhost"),
        ("GIT_AUTHOR_DATE", date.as_str()),
        ("GIT_COMMITTER_DATE", date.as_str()),
    ]
    .iter()
    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
    .collect();
    let message = format!("Merge {} from {}", plan.mirror.branch, target.name);
    let mut root = split_one(&tip)?;
    if !is_ancestor(base, &root) {
        let merge = crate::source::merge_commits(store, &main_view, &tip, &message, &identity)?
            .map_err(|files| {
                NewgitError::Unsupported(format!(
                    "`main` and `{}` on `{}` both moved, and merging them conflicts ({}), so \
                     nothing moved: merge `main` into `{}` there, and the push brings it in",
                    plan.mirror.branch,
                    target.name,
                    files.join(", "),
                    plan.mirror.branch,
                ))
            })?;
        root = split_one(&merge)?;
    }

    let list = compose::index(store, ROOT_TRACKER, &root)?;
    for tracker in list.keys().chain(plan.list.keys()) {
        if list.get(tracker) == plan.list.get(tracker) {
            continue;
        }
        if !plan.list.contains_key(tracker) || !list.contains_key(tracker) {
            return Err(NewgitError::Unsupported(format!(
                "this would add or remove `{tracker}`, which only `newgit merge` does"
            )));
        }
        if !plan.writable.contains(tracker) {
            return Err(NewgitError::Unsupported(format!(
                "this changes `{tracker}`, which whoever pushed it does not maintain"
            )));
        }
    }

    // Each child's `main` catches up as a merge's does: to the pin when
    // behind it, a merge when it moved on.
    let mut mains: Vec<(String, Option<String>, String)> = Vec::new();
    for (tracker, pin) in &list {
        if tracker == ROOT_TRACKER || plan.list.get(tracker) == Some(pin) {
            continue;
        }
        let main_ref = format!("refs/trackers/{tracker}/heads/main");
        let main = git(&["rev-parse", "-q", "--verify", &main_ref]).ok();
        let caught_up = match &main {
            None => pin.clone(),
            Some(main) if is_ancestor(pin, main) => continue,
            Some(main) if is_ancestor(main, pin) => pin.clone(),
            Some(main) => crate::source::merge_commits(store, main, pin, &message, &identity)?
                .map_err(|files| {
                    NewgitError::Unsupported(format!(
                        "`{tracker}`'s `main` moved since root last pinned it, and merging the \
                         two conflicts ({})",
                        files.join(", ")
                    ))
                })?,
        };
        mains.push((tracker.clone(), main, caught_up));
    }

    // Objects first: every commit waits at a landing ref no one reads. The
    // Worker lands `list` by compare-and-swap, and only then do root's
    // `main` and each child's follow. Pushing again is harmless.
    let source_of = |tracker: &str| {
        plan.repos.get(tracker).ok_or_else(|| {
            NewgitError::Unsupported(format!("the plan does not say where `{tracker}` is"))
        })
    };
    let land_at = |tracker: &str, commit: &str| -> Result<()> {
        let source = source_of(tracker)?;
        GitSource::push_leased(
            &[&git_dir],
            &source.url,
            commit,
            &crate::remote::landing_ref(commit),
            None,
            Some(&source.auth),
        )
    };
    land_at(ROOT_TRACKER, &root)?;
    let mut moves = Vec::new();
    for (tracker, before, caught_up) in mains {
        land_at(&tracker, &caught_up)?;
        moves.push(crate::remote::RefMove {
            tracker,
            branch: "main".to_owned(),
            old: before,
            new: caught_up,
        });
    }
    Ok(PullAnswer::Landed {
        list,
        commits: walk.len(),
        moves,
    })
}

/// Export target `name` as root commit `root` defines it.
fn read_target(store: &Utf8Path, root: &str, name: &str) -> Result<ExportTarget> {
    let path = format!(".newgit/exports/{name}.toml");
    let text = run_git_env(
        &[
            &format!("--git-dir={store}"),
            "show",
            &format!("{root}:{path}"),
        ],
        &[],
    )
    .map_err(|_| NewgitError::Unsupported(format!("root's `main` has no `{path}`")))?;
    ExportTarget::from_text(name, Utf8Path::new(&path), &text)
}
