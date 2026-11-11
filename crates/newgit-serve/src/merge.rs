//! `newgit merge` on the remote: begin, the commits sent, and the finish.
//!
//! The wire is the Worker's (docs/design/remote.md, *Merge*), so the CLI is
//! unchanged. The engine is not:
//!
//! - **Checks read the repos here.** Whether a pin moves forward along its
//!   tracker's `main`, whether a commit changes only pins, and what root's
//!   commit pins are local `git` reads, answered at once. Nothing answers 202.
//! - **A landing is one entry in the landing log** (`crate::landing`) over
//!   the whole index, in place (refs, rows, stores) before answering.
//!   Nothing follows later, and nothing is rolled up.
//! - **`merge` lands or it doesn't.** A branch still at the merge's `base`
//!   fast-forwards to the merger's own commits, ids and signatures kept. One
//!   that moved is merged onto by the server (`crate::server_merge`): Git's
//!   merge over the composed tree, split back by tracker, one merge commit
//!   in each tracker both sides moved with the merger's commit as its second
//!   parent. Only a real conflict is refused, naming each file.
//! - **The rules are read at the head it lands on**, on every attempt, never
//!   at the merge's base; a change to the definitions bumps the project's
//!   epoch, and an attempt prepared under another starts again.
//!
//! > **Decision (loud): the anchor decides approval, not what lands.** The
//! > Worker landed only the trackers at and under the anchor and rolled the
//! > parents up later, so merges at different anchors did not wait on each
//! > other across Durable Objects. Here the whole index lands at once, and
//! > two merges begun from the same index at different anchors both land:
//! > the second is merged onto the first. Who must approve is still decided
//! > by the anchor, exactly as before.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiResponse, Caller, ProjectContext};
use crate::db::{self, DbGuard, List};
use crate::error::{ApiResult, HttpError, fail};
use crate::landing::{self, Asked, Change, Entry, Hold, Landing};
use crate::names::{lineage, parent_of, within};
use crate::policy::{self, Facts};
use crate::repos::{self, Repo};

/// How long a merge begun may take to finish.
const PENDING: Duration = Duration::from_secs(70 * 60);
/// How long a merge's write grant lasts: enough to push to landing refs.
const PUSH_SECONDS: i64 = 10 * 60;
/// The largest pack sent before a merge begins.
pub const OBJECTS_MAX_BYTES: usize = 90 * 1024 * 1024;

/// A merge begun and not landed: what was checked when it began.
#[derive(Debug, Clone)]
pub struct Pending {
    by: String,
    target: List,
    /// The trackers whose branches the finish may ask to move.
    writes: Vec<String>,
    anchor: String,
    until: Instant,
}

/// Merges in flight, by project, branch, and the root commit they land.
#[derive(Default)]
pub struct Merges(std::sync::Mutex<BTreeMap<(String, String, String), Pending>>);

impl Merges {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String, String), Pending>> {
        let mut merges = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        merges.retain(|_, pending| pending.until > now);
        merges
    }
}

fn key(project: &str, branch: &str, list: &List) -> (String, String, String) {
    (
        project.to_owned(),
        branch.to_owned(),
        list.get("root").cloned().unwrap_or_default(),
    )
}

pub fn landing_ref(commit: &str) -> String {
    format!("refs/newgit/landing/{commit}")
}

fn is_commit_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn require_list(list: &List) -> ApiResult<()> {
    for (tracker, commit) in list {
        if let Some(problem) = crate::names::part_problem("tracker name", tracker) {
            return fail(400, problem);
        }
        if !is_commit_id(commit) {
            return fail(400, format!("`{tracker}` must name a full commit id"));
        }
    }
    Ok(())
}

// ── What the repos say ────────────────────────────────────────────────────

/// Whether `next`'s tree is `old`'s with only `.newgit/pins` changed.
fn only_pins_moved(dir: &camino::Utf8Path, old: &str, next: &str) -> Option<bool> {
    let output = repos::git(
        dir,
        &[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            old,
            next,
        ],
    )
    .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .all(|path| path.starts_with(".newgit/pins/"))
    })
}

/// Whether any commit `next` brings in (beyond `base` and every commit that
/// was ever this tracker's head on the branch) changes more than pins
/// against its first parent. `None` when the repo cannot say.
fn history_touches(
    dir: &camino::Utf8Path,
    base: &str,
    next: &str,
    approved: &[String],
) -> Option<bool> {
    let mut args = vec![
        "log",
        "--format=>%H %P",
        "--name-only",
        "--diff-merges=first-parent",
        next,
        "--not",
        base,
    ];
    args.extend(approved.iter().map(String::as_str));
    let output = repos::git(dir, &args).ok()?;
    if !output.status.success() {
        return None;
    }
    for entry in String::from_utf8_lossy(&output.stdout).split('>').skip(1) {
        let mut lines = entry.lines();
        let header = lines.next()?;
        if header.split_whitespace().count() < 2 {
            return Some(true); // a root commit: everything in it is new
        }
        if lines
            .filter(|line| !line.is_empty())
            .any(|path| !path.starts_with(".newgit/pins/"))
        {
            return Some(true);
        }
    }
    Some(false)
}

/// Every commit `tracker` was ever the head of on `branch`.
fn past_heads(
    db: &Connection,
    project: &str,
    branch: &str,
    tracker: &str,
) -> ApiResult<Vec<String>> {
    let mut statement = db.prepare(
        "SELECT DISTINCT new FROM head_log WHERE home = ? AND branch = ? AND tracker = ? AND new IS NOT NULL",
    )?;
    let rows = statement.query_map([project, branch, tracker], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The lowest tracker that is or contains every one of `ids`.
fn anchor_of(ids: &[String]) -> String {
    let Some(first) = ids.first() else {
        return "root".to_owned();
    };
    let mut common: Vec<String> = lineage(first).into_iter().map(str::to_owned).collect();
    for id in &ids[1..] {
        let theirs: BTreeSet<&str> = lineage(id).into_iter().collect();
        common.retain(|tracker| theirs.contains(tracker.as_str()));
    }
    common
        .into_iter()
        .next()
        .unwrap_or_else(|| "root".to_owned())
}

/// Where a merge from `base` to `list` lands: the lowest tracker covering
/// every one it touches (changes more than pins in), and every one it moves.
/// A tracker whose commits cannot be read yet counts as touched.
#[allow(clippy::too_many_arguments, reason = "each is one thing a landing is")]
fn anchor_for(
    project: &ProjectContext<'_>,
    db: &Connection,
    by: &str,
    branch: &str,
    base: &List,
    list: &List,
    moved: &[String],
    facts: &BTreeMap<String, Facts>,
) -> ApiResult<String> {
    if moved
        .iter()
        .any(|tracker| !base.contains_key(tracker) || !list.contains_key(tracker))
    {
        return Ok("root".to_owned());
    }
    let mut touched = Vec::new();
    for tracker in moved {
        let dir = project.tracker(tracker).view(by)?;
        let (old, new) = (&base[tracker], &list[tracker]);
        let only = only_pins_moved(&dir, old, new);
        let history = match only {
            Some(true) => history_touches(
                &dir,
                old,
                new,
                &past_heads(db, &project.key, branch, tracker)?,
            ),
            _ => Some(true),
        };
        if only != Some(true) || history != Some(false) {
            touched.push(tracker.clone());
        }
    }
    // A pinned child's pin moving is its parent's change, not a roll-up.
    for tracker in moved {
        let Some(parent) = parent_of(tracker) else {
            continue;
        };
        if touched.iter().any(|known| known == parent)
            || !base.contains_key(parent)
            || !list.contains_key(parent)
        {
            continue;
        }
        if facts.get(tracker).is_some_and(|fact| fact.pinned) {
            touched.push(parent.to_owned());
        }
    }
    let mut anchor = anchor_of(&touched);
    for tracker in moved {
        if !within(tracker, &anchor) && !within(&anchor, tracker) {
            anchor = anchor_of(&[anchor.clone(), tracker.clone()]);
        }
    }
    Ok(anchor)
}

/// The index root's commit pins, all the way down: each tracker's
/// `.newgit/pins/<name>`, read from that tracker's own repo, and for a merge
/// by `by` from its landing too.
pub fn index_of(project: &ProjectContext<'_>, root: &str, by: Option<&str>) -> ApiResult<List> {
    let mut index = List::new();
    let mut stack = vec![("root".to_owned(), root.to_owned())];
    while let Some((tracker, commit)) = stack.pop() {
        if index.contains_key(&tracker) {
            continue;
        }
        let repo = project.tracker(&tracker);
        let dir = match by {
            Some(by) => repo.view(by)?,
            None => repo.dir(),
        };
        index.insert(tracker.clone(), commit.clone());
        for (name, pin) in repos::pins_of(&dir, &commit) {
            let child = if tracker == "root" {
                name
            } else {
                format!("{tracker}__{name}")
            };
            stack.push((child, pin));
        }
    }
    Ok(index)
}

// ── The routes ────────────────────────────────────────────────────────────

/// `POST /objects/<tracker>/<commit>`: a merge's commits sent before it
/// begins, as a thin pack, put at the commit's landing ref.
pub fn receive_objects(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    tracker: &str,
    commit: &str,
    pack: &[u8],
) -> ApiResult<ApiResponse> {
    if caller.kind == "agent" {
        return fail(
            403,
            format!(
                "agent tokens cannot merge; {} can, from their own login",
                caller.login
            ),
        );
    }
    if let Some(problem) = crate::names::part_problem("tracker name", tracker) {
        return fail(400, problem);
    }
    if !is_commit_id(commit) {
        return fail(404, "no such route");
    }
    if pack.is_empty() {
        return fail(411, "send the pack with its length");
    }
    if pack.len() > OBJECTS_MAX_BYTES {
        return fail(
            413,
            format!(
                "a pack this large ({} bytes) is pushed with a write password, not sent here",
                pack.len()
            ),
        );
    }
    let Some(facts) = project.facts(db, tracker)? else {
        return fail(
            404,
            format!(
                "tracker `{tracker}` is not defined on {}'s `access` `main`",
                project.key
            ),
        );
    };
    if !facts.git {
        return fail(
            400,
            format!("`{tracker}` is not in Git; `newgit merge` never carries it"),
        );
    }
    let (who, _) = project.who_for(db, &facts, &project.who(caller))?;
    policy::decide(
        &project.repo_of(tracker),
        false,
        &who,
        Some(&facts),
        &policy::Context::default(),
    )
    .map_err(|why| HttpError::new(403, why))?;
    // Checked to be in its audience's store, as any repo served is: the
    // landing borrows that store's objects.
    let repo = project.served_tracker(db, tracker)?;
    if !repo.exists() {
        return fail(
            404,
            format!("{} does not exist", project.repo_of(tracker).name()),
        );
    }
    // Into the merger's landing, not the store: which store the commit
    // belongs in is the landing's to say (`Repo::landing_dir`).
    let landing = repo.open_landing(&caller.login)?;
    let mut index = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(landing.as_str())
        .args(["index-pack", "--stdin", "--fix-thin", "--strict"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = index.stdin.take() {
        use std::io::Write as _;
        stdin.write_all(pack)?;
    }
    let indexed = index.wait_with_output()?;
    if !indexed.status.success() {
        return fail(
            400,
            format!(
                "the pack for {commit} was refused: {}",
                String::from_utf8_lossy(&indexed.stderr).trim()
            ),
        );
    }
    if !repos::has_commit(&landing, commit) {
        return fail(400, format!("the pack sent for {commit} does not hold it"));
    }
    let reference = landing_ref(commit);
    let output = repos::git(&landing, &["update-ref", &reference, commit])?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "could not set {reference} in {landing}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(ApiResponse {
        status: 200,
        body: json!({ "tracker": tracker, "commit": commit, "ref": reference }),
    })
}

/// What a merge from `base` to `list` may do, by the rules as they are now:
/// the definitions and maintainers on each tracker's `main`, and who is a
/// member. Asked when the merge begins, and again at the head each attempt
/// to finish it lands on (landing.md, *Landing* 2): a maintainer removed in
/// between does not land work.
struct Authorized {
    /// The trackers whose branches the finish may ask to move.
    writes: Vec<String>,
    anchor: String,
}

fn authorize_merge(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    branch: &str,
    base: &List,
    list: &List,
) -> ApiResult<Authorized> {
    let moved: Vec<String> = base
        .keys()
        .chain(list.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|tracker| base.get(*tracker) != list.get(*tracker))
        .cloned()
        .collect();
    let facts = project.definitions(db, moved.iter().map(String::as_str))?;
    // The links its definitions name, recorded before anything is named.
    for (tracker, fact) in &facts {
        if let Some((home, there)) = policy::home_of(fact) {
            db::add_link(db, &project.key, tracker, &home.key(), &there)?;
        }
    }
    let anchor = anchor_for(
        project,
        db,
        &caller.login,
        branch,
        base,
        list,
        &moved,
        &facts,
    )?;
    let who = project.who(caller);
    let mut writes = Vec::new();
    for tracker in &moved {
        let fact = facts.get(tracker);
        let dropped = !list.contains_key(tracker);
        if !base.contains_key(tracker) {
            // New to the branch: it joins through its own seeded repo, added
            // by whoever governs its definition.
            if fact.is_some_and(|fact| !fact.git) {
                return fail(
                    400,
                    format!("`{tracker}` is not in Git; `newgit merge` never carries it"),
                );
            }
            if !project.tracker(tracker).exists() {
                return fail(
                    400,
                    format!(
                        "`{tracker}` has no repo: a new tracker in Git joins by its own seeded repo"
                    ),
                );
            }
            let governor = fact.map_or("access", |fact| fact.source.as_str());
            let governing = facts
                .get(governor)
                .cloned()
                .or_else(|| policy::implicit_facts("access"))
                .unwrap_or_else(|| unreachable!());
            if policy::may_maintain(&governing.maintainers, &who, &project.info.owner, governor)
                .is_err()
            {
                return fail(
                    403,
                    format!(
                        "only the maintainers of the tracker holding `{tracker}`'s definition may add it"
                    ),
                );
            }
            continue;
        }
        let Some(fact) = fact else {
            if dropped {
                continue;
            }
            return fail(
                400,
                format!(
                    "`{tracker}` is not defined on `access`'s `main`: a new tracker in Git joins by its own seeded repo"
                ),
            );
        };
        if !fact.git {
            return fail(
                400,
                format!("`{tracker}` is not in Git; `newgit merge` never carries it"),
            );
        }
        let (policy_who, owner) = project.who_for(db, fact, &who)?;
        policy::decide(
            &project.repo_of(tracker),
            false,
            &policy_who,
            Some(fact),
            &policy::Context::default(),
        )
        .map_err(|why| HttpError::new(403, why))?;
        let repo = project.tracker(tracker);
        let view = repo.view(&caller.login)?;
        let (forward, on_main) = match (tracker.as_str(), list.get(tracker)) {
            ("root", _) | (_, None) => (false, false),
            (_, Some(new)) => (
                repos::has_commit(&view, new) && repos::is_ancestor(&view, &base[tracker], new),
                repo.branch_head("main")
                    .is_some_and(|main| repos::is_ancestor(&view, new, &main)),
            ),
        };
        // Above the anchor nothing is touched, only moved along: approved
        // where it lands.
        if anchor != *tracker && within(&anchor, tracker) {
            if tracker == "root" || !on_main {
                writes.push(tracker.clone());
            }
            continue;
        }
        if tracker != "root" && !dropped && forward && on_main {
            // A pin following its tracker's own `main` is the parent's choice.
            if anchor != "root" {
                let parent = parent_of(tracker).unwrap_or("root");
                let parent_facts = facts
                    .get(parent)
                    .cloned()
                    .or_else(|| policy::implicit_facts("root"))
                    .unwrap_or_else(|| unreachable!());
                policy::may_maintain(&parent_facts.maintainers, &who, &project.info.owner, parent).map_err(|why| {
                    HttpError::new(403, format!("{why} (`{tracker}`'s pin follows its `main`, which is `{parent}`'s to choose)"))
                })?;
            }
            continue;
        }
        let owner = owner.unwrap_or_else(|| project.info.owner.clone());
        if let Err(why) = policy::may_maintain(&fact.maintainers, &policy_who, &owner, tracker) {
            let why = if tracker == "root" || dropped {
                why
            } else if forward {
                format!("{why} (its pin moves to a commit not on its `main`)")
            } else {
                format!("{why} (its pin moves back or sideways along its history)")
            };
            return fail(403, why);
        }
        if tracker == "root" || (!dropped && !on_main) {
            writes.push(tracker.clone());
        }
    }
    Ok(Authorized { writes, anchor })
}

/// Whether index `base` is one `current` came from: its root commit is
/// behind current's, in root's store.
fn was_on(project: &ProjectContext<'_>, base: &List, current: &List) -> bool {
    match (base.get("root"), current.get("root")) {
        (Some(base), Some(current)) => project.tracker("root").is_ancestor(base, current),
        _ => false,
    }
}

#[derive(Deserialize)]
pub struct Begin {
    base: Option<List>,
    list: Option<List>,
    #[serde(default)]
    uploaded: bool,
}

/// `POST /branches/<branch>/merge`: check everything, and remember what was
/// checked. A refused merge holds nothing.
pub fn begin(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    branch: &str,
    begin: Begin,
    origin: &str,
) -> ApiResult<ApiResponse> {
    if caller.kind == "agent" {
        return fail(
            403,
            format!(
                "agent tokens cannot merge; {} can, from their own login",
                caller.login
            ),
        );
    }
    let (Some(base), Some(list)) = (begin.base, begin.list) else {
        return fail(
            400,
            "base (the index you merged onto) and list (the index the merge makes) are required",
        );
    };
    if !list.contains_key("root") || list.get("root") == base.get("root") {
        return fail(
            400,
            "a merge moves `root`: the branch is root's ref, so the new index names a new root commit",
        );
    }
    require_list(&list)?;
    require_list(&base)?;
    let Some(current) = db::list_of(db, &project.key, branch)? else {
        return fail(404, format!("no branch `{branch}` in {}", project.key));
    };
    // A base the branch has moved on from is merged onto what it is now
    // when the merge finishes; one it never was is refused.
    if current != base && !was_on(project, &base, &current) {
        return fail(
            409,
            format!("the base this merge names was never `{branch}`: merge onto what it is now"),
        );
    }
    let Authorized { writes, anchor } = authorize_merge(project, db, caller, branch, &base, &list)?;
    let until = Instant::now() + PENDING;
    project.app.merges.lock().insert(
        key(&project.key, branch, &list),
        Pending {
            by: caller.login.clone(),
            target: list,
            writes: writes.clone(),
            anchor: anchor.clone(),
            until,
        },
    );
    // Not sent ahead: the merger pushes its commits to landing refs, and to
    // nothing else, for minutes.
    let mut credentials = Vec::new();
    if !begin.uploaded {
        for tracker in &writes {
            let repo = project.repo_of(tracker).name();
            db::grant(
                db,
                &repo,
                &caller.token_id,
                crate::git_http::LANDING,
                db::now_seconds() + PUSH_SECONDS,
            )?;
            let expires = caller.expires.min(db::now_seconds() + PUSH_SECONDS);
            credentials.push(json!({
                "tracker": tracker, "repo": repo, "remote": crate::api::git_url(origin, &repo),
                "username": "x", "password": caller.token, "expires_at": db::iso_at(expires),
            }));
        }
    }
    let until = db::iso_at(db::now_seconds() + i64::try_from(PENDING.as_secs()).unwrap_or(0));
    Ok(ApiResponse {
        status: 200,
        body: json!({ "branch": branch, "until": until, "credentials": credentials, "anchor": anchor, "writes": writes }),
    })
}

#[derive(Debug, Deserialize)]
struct RefMove {
    tracker: String,
    branch: String,
    old: Option<String>,
    new: String,
}

/// `PUT /branches/<branch>` with a `base`: land the index the merge began
/// with, as one entry in the landing log, in place before answering. When
/// the branch is still at `base` it fast-forwards to the merger's own
/// commits; when it moved, the server merges onto what it is now
/// ([`crate::server_merge`]) and lands that.
///
/// Each attempt reads the rules at the head it lands on, merges outside any
/// lock, then holds the project, rechecks that the branch and its epoch are
/// still what it merged onto and read the rules at, and lands. An attempt
/// that loses the recheck starts again; after [`FAIR`] such losses, or for a
/// change to the definitions (a barrier), the project is held before the
/// merge, so it lands on the head it merged onto, by itself.
pub fn finish(
    project: &ProjectContext<'_>,
    db: DbGuard<'_>,
    caller: &Caller,
    branch: &str,
    base: &List,
    list: &List,
    moves: Option<&Value>,
) -> ApiResult<ApiResponse> {
    if caller.kind == "agent" {
        return fail(
            403,
            format!(
                "agent tokens cannot merge; {} can, from their own login",
                caller.login
            ),
        );
    }
    let pending_key = key(&project.key, branch, list);
    let pending = project.app.merges.lock().get(&pending_key).cloned();
    let current = db::list_of(&db, &project.key, branch)?;
    let landed_already = current
        .as_ref()
        .is_some_and(|current| landed_in(project, list, current));
    if pending.is_none() && !landed_already {
        return fail(
            400,
            "this is not the index a merge began with: start the merge again",
        );
    }
    if let Some(pending) = &pending {
        if pending.by != caller.login {
            return fail(
                409,
                format!("{} began this merge; only they may land it", pending.by),
            );
        }
        if pending.target != *list {
            return fail(
                400,
                "this is not the index the merge began with: start the merge again",
            );
        }
    }
    let writes = pending
        .as_ref()
        .map(|pending| pending.writes.clone())
        .unwrap_or_default();
    let asked = check_moves(moves, &writes, branch, list)?;
    if landed_already {
        // A finish asked again, its answer lost: it landed, and is in place
        // (the list says so only once it is).
        return answer(
            &db,
            project,
            branch,
            Vec::new(),
            pending.map_or_else(|| "root".to_owned(), |p| p.anchor),
            false,
        );
    }
    let Some(pending) = pending else {
        return fail(
            400,
            "this is not the index a merge began with: start the merge again",
        );
    };
    for (tracker, commit) in list {
        if !repos::has_commit(&project.tracker(tracker).view(&caller.login)?, commit) {
            return fail(
                400,
                format!(
                    "`{tracker}`'s commit {commit} is not on the remote: push it to its landing ref, then finish"
                ),
            );
        }
    }
    if index_of(project, &list["root"], Some(&caller.login))? != *list {
        return fail(
            400,
            "root's commit does not pin what this index says: push the root commit the merge made to its landing ref, then finish",
        );
    }
    if !repos::is_ancestor(
        &project.tracker("root").view(&caller.login)?,
        &base["root"],
        &list["root"],
    ) {
        return fail(
            400,
            format!(
                "root's commit is not made on the base this merge names: merge onto `{branch}` again"
            ),
        );
    }
    // Its roots: this project, and the home of each linked tracker it asks
    // to move. Held from deciding which store each new commit goes into
    // until it has landed: a merge's commits never reach a store whose
    // readers the landing that is checked would not let read them.
    let mut roots = BTreeSet::from([project.key.clone()]);
    for step in &asked {
        if let Some((home, _)) = project.links.get(&step.tracker) {
            roots.insert(home.clone());
        }
    }
    let barrier = changes_definitions(project, &caller.login, base, list);
    drop(db);
    let started = Instant::now();
    // Another merge holding the roots longer than a landing waits is not a
    // refusal: this one waits for its turn, up to `TURN_WAIT` in all.
    let waits = |error: &HttpError| landing::is_busy(error) && started.elapsed() < TURN_WAIT;
    let mut lost = 0;
    loop {
        // A barrier, or a merge that lost too often, holds its roots before
        // it merges: it lands on the head it merged onto.
        let turn = if barrier || lost >= FAIR {
            match project.app.log.hold(
                project.app,
                roots.clone(),
                &format!("a merge into `{branch}` has a turn to itself"),
            ) {
                Ok(turn) => Some(turn),
                Err(error) if waits(&error) => continue,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let attempt = Attempt {
            caller,
            branch,
            base,
            list,
            moves,
            pending: &pending,
            roots: &roots,
            barrier,
        };
        match attempt.run(project, turn.as_ref()) {
            Ok(Some(answered)) => return Ok(answered),
            Ok(None) => lost += 1,
            Err(error) if waits(&error) => lost += 1,
            Err(error) => return Err(error),
        }
    }
}

/// How long a merge's finish waits for its turn, in all, before it is
/// answered busy (503): under the client's own minute.
const TURN_WAIT: Duration = Duration::from_secs(40);

/// How many times a merge loses the recheck before it holds its roots
/// first (landing.md, *Landing* 6).
const FAIR: usize = 3;

/// Whether index `list` has landed on a branch now at `current`: it is the
/// list, or its root commit is behind the branch's.
fn landed_in(project: &ProjectContext<'_>, list: &List, current: &List) -> bool {
    current == list
        || match (list.get("root"), current.get("root")) {
            (Some(root), Some(now)) => {
                root != now && project.tracker("root").is_ancestor(root, now)
            }
            _ => false,
        }
}

/// Whether moving `base` to `list` changes what `.newgit/trackers/` says:
/// `access`'s own files, or any tracker's own `.newgit/trackers/`. Such a
/// merge is a barrier (landing.md, *Landing* 5). One that cannot be read is
/// taken for one.
fn changes_definitions(project: &ProjectContext<'_>, by: &str, base: &List, list: &List) -> bool {
    for (tracker, new) in list {
        let Some(old) = base.get(tracker) else {
            continue;
        };
        if old == new {
            continue;
        }
        let Ok(dir) = project.tracker(tracker).view(by) else {
            return true;
        };
        let Ok(output) = repos::git(
            &dir,
            &["diff-tree", "-r", "--name-only", "--no-commit-id", old, new],
        ) else {
            return true;
        };
        if !output.status.success() {
            return true;
        }
        let paths = String::from_utf8_lossy(&output.stdout);
        let mut paths = paths.lines();
        if tracker == "access" {
            if paths.any(|path| !path.starts_with(".newgit/pins/")) {
                return true;
            }
        } else if paths.any(|path| path.starts_with(".newgit/trackers/")) {
            return true;
        }
    }
    false
}

/// One attempt to finish a merge.
struct Attempt<'a> {
    caller: &'a Caller,
    branch: &'a str,
    base: &'a List,
    list: &'a List,
    moves: Option<&'a Value>,
    pending: &'a Pending,
    roots: &'a BTreeSet<String>,
    barrier: bool,
}

impl Attempt<'_> {
    /// Landed (its answer), or `None`: something landed on the branch, or
    /// changed the definitions, between its merge and its recheck.
    fn run(
        &self,
        at: &ProjectContext<'_>,
        turn: Option<&Hold<'_>>,
    ) -> ApiResult<Option<ApiResponse>> {
        let app = at.app;
        let login = &self.caller.login;
        let branch = self.branch;
        let db = app.db.lock();
        // The project as it is now: its rows, links, and the caller's
        // membership.
        let project = ProjectContext::load(app, &db, self.caller, &at.id)?;
        let Some(head) = db::list_of(&db, &project.key, branch)? else {
            return fail(404, format!("no branch `{branch}` in {}", project.key));
        };
        if landed_in(&project, self.list, &head) {
            return answer(
                &db,
                &project,
                branch,
                Vec::new(),
                self.pending.anchor.clone(),
                false,
            )
            .map(Some);
        }
        let epoch = db::epoch(&db, &project.key)?;
        // The rules at the head it lands on, never at its base.
        let authorized = authorize_merge(&project, &db, self.caller, branch, self.base, self.list)?;
        if !within(&authorized.anchor, &self.pending.anchor) {
            return fail(
                409,
                "this merge touches more than it did when it began: start the merge again",
            );
        }
        let asked = check_moves(self.moves, &authorized.writes, branch, self.list)?;
        drop(db);
        // Merged outside any lock, unless it has its turn.
        let merged = if head == *self.base {
            None
        } else {
            Some(crate::server_merge::merge(
                &project, login, branch, self.base, &head, self.list,
            )?)
        };
        let target = merged
            .as_ref()
            .map_or_else(|| self.list.clone(), |merged| merged.list.clone());
        app.log.pause(&format!("delay-merge-by-{login}"));
        let own = match turn {
            Some(_) => None,
            None => Some(app.log.hold(
                app,
                self.roots.clone(),
                &format!("a merge into `{branch}` is placing its commits"),
            )?),
        };
        let Some(hold) = turn.or(own.as_ref()) else {
            return Err(HttpError::internal("a merge landed without its roots held"));
        };
        let db = app.db.lock();
        // The recheck: nothing landed on the branch, and the definitions did
        // not change, since it merged and read the rules.
        if db::list_of(&db, &project.key, branch)?.as_ref() != Some(&head)
            || db::epoch(&db, &project.key)? != epoch
        {
            return Ok(None);
        }
        let asked = follow(&project, login, asked, self.list, &target);
        let mut prepared = prepare(&project, &db, login, branch, &head, &target, &asked)?;
        if let Some(change) = prepared.landing.entry.homes.get_mut(&project.key)
            && self.barrier
        {
            change.epoch = Some(epoch + 1);
        }
        prepared.landing.epochs = vec![(project.key.clone(), epoch)];
        drop(db);
        // Each tracker's new commits into a store that may hold them whether
        // or not this lands, outside the db lock and inside the roots' hold.
        for (target, borrow, commit) in &prepared.placements {
            target.take_landing(login, commit, borrow)?;
        }
        let landed = app.log.land_holding(app, prepared.landing, hold);
        drop(own);
        let db = app.db.lock();
        let (behind, pending) = match landed {
            Ok(landed) => (
                landed
                    .behind
                    .iter()
                    .map(|step| {
                        format!(
                            "`{}`'s `{}` moved while this merge ran",
                            step.tracker,
                            step.reference.trim_start_matches("refs/heads/")
                        )
                    })
                    .collect(),
                landed.pending,
            ),
            Err(error) if error.status == 409 => {
                let now = db::list_of(&db, &project.key, branch)?;
                if now.as_ref() == Some(&target) {
                    // Landed by an earlier ask whose answer was lost.
                    (Vec::new(), None)
                } else if now.as_ref() != Some(&head) || db::epoch(&db, &project.key)? != epoch {
                    return Ok(None);
                } else {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        };
        db.execute(
            "DELETE FROM grants WHERE token_id = ? AND refs = 'refs/newgit/landing/'",
            [&self.caller.token_id],
        )?;
        // The merger's commits a merge commit landed in place of wait for
        // nothing now: their landing refs go (their objects came with it).
        for (tracker, commit) in self.list {
            if target.get(tracker) != Some(commit)
                && let Ok(landing) = project.tracker(tracker).landing_dir(login)
                && repos::store_exists(&landing)
            {
                let _ = repos::git(&landing, &["update-ref", "-d", &landing_ref(commit)]);
            }
        }
        let mut answered = answer(
            &db,
            &project,
            branch,
            behind,
            authorized.anchor,
            merged.is_some(),
        )?;
        if let Some(pending) = pending {
            // It landed: readers see it. Only removing what it moved from
            // where it was is left, and is tried again.
            answered.body["pending"] = json!(format!(
                "landed; removing what it moved from where it was failed ({pending}) and is tried again at each landing on this project and when the server starts"
            ));
        }
        Ok(Some(answered))
    }
}

/// The asked moves of a merge the server merged: a tracker branch asked to
/// move to the merger's commit moves to the commit that lands in its place,
/// from where it is now, when that comes after it. Any other is asked as it
/// was, and said back as behind when it moved meanwhile.
fn follow(
    project: &ProjectContext<'_>,
    by: &str,
    asked: Vec<RefMove>,
    list: &List,
    target: &List,
) -> Vec<RefMove> {
    asked
        .into_iter()
        .map(|step| {
            let Some(landing) = target.get(&step.tracker) else {
                return step;
            };
            if list.get(&step.tracker) != Some(&step.new) || *landing == step.new {
                return step;
            }
            let repo = project.tracker(&step.tracker);
            let now = repo.branch_head(&step.branch);
            let after = |now: &String| {
                repo.view(by)
                    .is_ok_and(|view| repos::is_ancestor(&view, now, landing))
            };
            if now.as_ref().is_some_and(|now| !after(now)) {
                return step;
            }
            RefMove {
                old: now,
                new: landing.clone(),
                ..step
            }
        })
        .collect()
}

/// A merge's landing, ready to be checked under the lock and written down,
/// and the commits to bring into stores before it is.
struct Prepared {
    landing: Landing,
    /// Each commit waiting in the merger's landing, the repo it goes into
    /// before the landing is written down, and the repo it resolves
    /// against: where its tracker is.
    placements: Vec<(Repo, Repo, String)>,
}

/// What landing `list` onto `base` writes down, and where its new commits
/// go. Definitions are read from each tracker's `main`, so only a landing on
/// `main` changes them; for one, they are read from the commits it lands,
/// and every tracker whose audience that changes moves store in the same
/// landing (`ProjectContext::moves`).
///
/// A tracker's new commits wait in the merger's landing until they go into a
/// store whose readers may see them whether or not the landing happens:
///
/// - a tracker staying where it is, or widening, takes them into the store it
///   is in now, before the landing is written down (a widening copies them on
///   once it is, with the rest of its history);
/// - a narrowing takes them into its new, narrower store before, too;
/// - a move between audiences not ordered by inclusion (`user:a` to
///   `user:b`, a private instance's to the members') takes them into neither
///   before: they go straight into the new store after the landing is
///   written down (`Change::takes`).
///
/// So a landing that is refused, or never written down, has made nothing
/// more readable than it was, and one that is has made all of it.
fn prepare(
    project: &ProjectContext<'_>,
    db: &Connection,
    by: &str,
    branch: &str,
    base: &List,
    list: &List,
    asked: &[RefMove],
) -> ApiResult<Prepared> {
    let mut names = project.placed();
    names.extend(list.keys().cloned());
    let after = if branch == "main" {
        let mut at = BTreeMap::new();
        for (tracker, commit) in list {
            if !project.links.contains_key(tracker) {
                let view = project.tracker(tracker).view(by)?;
                at.insert(tracker.clone(), (view, commit.clone()));
            }
        }
        policy::load_definitions_at(
            &project.app.config.data,
            db,
            &project.id,
            names.iter().map(String::as_str),
            &at,
        )
    } else {
        project.definitions(db, names.iter().map(String::as_str))?
    };
    let mut listed: BTreeSet<String> = db::listed_trackers(db, &project.key)?.into_iter().collect();
    listed.extend(list.keys().cloned());
    let stores = project.moves(&after, &listed)?;
    let mut change = Change {
        incarnation: project.info.incarnation.clone(),
        branches: [(branch.to_owned(), list.clone())].into(),
        ..Change::default()
    };
    let mut placements = Vec::new();
    let mut placed = Vec::new();
    for (tracker, commit) in list {
        if project.links.contains_key(tracker) {
            continue;
        }
        let here = project.own(tracker);
        placed.push(landing::Placed {
            home: project.key.clone(),
            tracker: tracker.clone(),
            store: db::tracker_store(db, &project.key, tracker)?,
        });
        // Its landing ref goes once the commit is on the branch.
        change
            .refs
            .entry(tracker.clone())
            .or_default()
            .insert(landing_ref(commit), landing::RefMove::to(None));
        let to = stores
            .get(tracker)
            .and_then(|step| repos::Store::parse(&step.to));
        match to {
            None => placements.push((here.clone(), here, commit.clone())),
            Some(to) if to.includes(&here.store) => {
                placements.push((here.clone(), here, commit.clone()));
            }
            Some(to) if here.store.includes(&to) => {
                let there = Repo {
                    store: to,
                    ..here.clone()
                };
                placements.push((there, here, commit.clone()));
            }
            Some(_) => {
                if here.waits_in_landing(by, commit) {
                    change
                        .takes
                        .entry(tracker.clone())
                        .or_default()
                        .push(commit.clone());
                }
            }
        }
    }
    change.refs.entry("root".to_owned()).or_default().insert(
        format!("refs/heads/{branch}"),
        landing::RefMove::to(Some(list["root"].clone())),
    );
    change.stores = stores;
    let mut homes = BTreeMap::new();
    let mut asked_refs = Vec::new();
    for step in asked {
        let (home, there) = match project.links.get(&step.tracker) {
            Some((home, there)) => (home.clone(), there.clone()),
            None => (project.key.clone(), step.tracker.clone()),
        };
        placed.push(landing::Placed {
            home: home.clone(),
            tracker: there.clone(),
            store: db::tracker_store(db, &home, &there)?,
        });
        // Its home named in the entry before the landing waits, so its root
        // is taken with the rest.
        if home != project.key && !homes.contains_key(&home) {
            let Some(info) = db::project(db, &home)? else {
                return fail(
                    409,
                    format!(
                        "`{}` links to `{home}`, which no longer exists",
                        step.tracker
                    ),
                );
            };
            homes.insert(
                home.clone(),
                Change {
                    incarnation: info.incarnation,
                    ..Change::default()
                },
            );
        }
        asked_refs.push(Asked {
            home,
            tracker: there,
            reference: format!("refs/heads/{}", step.branch),
            old: step.old.clone(),
            new: step.new.clone(),
        });
    }
    homes.insert(project.key.clone(), change);
    Ok(Prepared {
        landing: Landing {
            entry: Entry {
                by: by.to_owned(),
                homes,
                ..Entry::default()
            },
            expect: vec![landing::Expect {
                home: project.key.clone(),
                branch: branch.to_owned(),
                list: Some(base.clone()),
            }],
            placed,
            asked: asked_refs,
            epochs: Vec::new(),
        },
        placements,
    })
}

/// The branch as it landed, and how: `merged` when the server merged it onto
/// what landed since its base, so the list is not the merger's own.
fn answer(
    db: &Connection,
    project: &ProjectContext<'_>,
    branch: &str,
    behind: Vec<String>,
    anchor: String,
    merged: bool,
) -> ApiResult<ApiResponse> {
    let mut found = db::branches(db, &project.key)?
        .into_iter()
        .find(|found| found["name"] == branch)
        .unwrap_or(Value::Null);
    found["behind"] = json!(behind);
    found["anchor"] = json!(anchor);
    found["merged"] = json!(merged);
    Ok(ApiResponse {
        status: 200,
        body: found,
    })
}

/// The moves a finish asks for, each one the merge may make.
fn check_moves(
    moves: Option<&Value>,
    writes: &[String],
    branch: &str,
    list: &List,
) -> ApiResult<Vec<RefMove>> {
    let Some(moves) = moves.filter(|moves| !moves.is_null()) else {
        return Ok(Vec::new());
    };
    let moves: Vec<RefMove> = serde_json::from_value(moves.clone()).map_err(|_| {
        HttpError::new(
            400,
            "each move names a tracker, a branch, and a full commit id",
        )
    })?;
    for step in &moves {
        if !is_commit_id(&step.new) || step.old.as_deref().is_some_and(|old| !is_commit_id(old)) {
            return fail(
                400,
                "each move names a tracker, a branch, and a full commit id",
            );
        }
        if step.tracker == "root" {
            return fail(400, "root's branch follows the list; it is not asked for");
        }
        if !writes.contains(&step.tracker) {
            return fail(
                403,
                format!(
                    "this merge was not handed `{}` to write, so it may not move it",
                    step.tracker
                ),
            );
        }
        if step.branch != "main" && step.branch != branch {
            return fail(
                400,
                format!(
                    "a merge into `{branch}` moves `{}`'s `main` or `{branch}`, not `{}`",
                    step.tracker, step.branch
                ),
            );
        }
        if step.branch == branch && branch != "main" && list.get(&step.tracker) != Some(&step.new) {
            return fail(
                400,
                format!(
                    "`{}`'s `{branch}` moves to the commit the index pins",
                    step.tracker
                ),
            );
        }
    }
    Ok(moves)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_are_the_lowest_common_tracker() {
        assert_eq!(anchor_of(&[]), "root");
        assert_eq!(anchor_of(&["a__b".into(), "a__c".into()]), "a");
        assert_eq!(anchor_of(&["a__b".into(), "d".into()]), "root");
        assert_eq!(anchor_of(&["a__b".into()]), "a__b");
    }
}
