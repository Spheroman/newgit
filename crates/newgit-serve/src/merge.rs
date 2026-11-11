//! `newgit merge` on the remote: begin, the commits sent, and the finish.
//!
//! The wire is the Worker's (docs/design/remote.md, *Merge*), so the CLI is
//! unchanged. The engine is not:
//!
//! - **Checks read the repos here.** Whether a pin moves forward along its
//!   tracker's `main`, whether a commit changes only pins, and what root's
//!   commit pins are local `git` reads, answered at once. Nothing answers 202.
//! - **A landing is one SQLite transaction** over the whole index, compared
//!   with the `base` the merge began from, then each repo's branch moved by
//!   `git update-ref` before answering. Nothing follows later, and nothing is
//!   rolled up: the merger's own commits are the branch's history, ids and
//!   signatures kept.
//!
//! > **Decision (loud): the anchor decides approval, not what lands.** The
//! > Worker landed only the trackers at and under the anchor and rolled the
//! > parents up later, so merges at different anchors did not wait on each
//! > other across Durable Objects. Here a landing is microseconds, so the
//! > whole index lands at once. Two merges begun from the same index at
//! > different anchors now conflict: the second is told to merge again. Who
//! > must approve is still decided by the anchor, exactly as before.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiResponse, Caller, ProjectContext};
use crate::db::{self, List};
use crate::error::{ApiResult, HttpError, fail};
use crate::names::{lineage, parent_of, within};
use crate::policy::{self, Facts};
use crate::repos::{self, RefUpdate};

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
    /// The anchor was read from the merge's own commits.
    read: bool,
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
/// `None` for the `read` flag when a commit could not be read: root, then.
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
) -> ApiResult<(String, bool)> {
    if moved
        .iter()
        .any(|tracker| !base.contains_key(tracker) || !list.contains_key(tracker))
    {
        return Ok(("root".to_owned(), true));
    }
    let mut touched = Vec::new();
    let mut read = true;
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
        if only.is_none() || history.is_none() {
            read = false;
        }
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
    Ok((anchor, read))
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
    let Some(current) = db::list_of(db, &project.key, branch)? else {
        return fail(404, format!("no branch `{branch}` in {}", project.key));
    };
    if current != base {
        return fail(
            409,
            format!("`{branch}` moved since you looked: merge again onto what it is now"),
        );
    }
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
    let (anchor, read) = anchor_for(
        project,
        db,
        &caller.login,
        branch,
        &base,
        &list,
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
    let until = Instant::now() + PENDING;
    project.app.merges.lock().insert(
        key(&project.key, branch, &list),
        Pending {
            by: caller.login.clone(),
            target: list,
            writes: writes.clone(),
            anchor: anchor.clone(),
            read,
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
/// with, all at once, then move every repo's branch before answering.
pub fn finish(
    project: &ProjectContext<'_>,
    db: &mut Connection,
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
    let current = db::list_of(db, &project.key, branch)?;
    let landed_already = current.as_ref() == Some(list);
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
        // A finish asked again, its answer lost (or its objects not yet in
        // place): it landed, and lands and follows again.
        land(project, db, &caller.login, branch, list)?;
        let behind = follow(project, branch, list, &asked)?;
        return answer(
            db,
            project,
            branch,
            behind,
            pending.map_or_else(|| "root".to_owned(), |p| p.anchor),
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
    let moved: Vec<String> = base
        .keys()
        .chain(list.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|tracker| base.get(*tracker) != list.get(*tracker))
        .cloned()
        .collect();
    let anchor = if pending.read {
        pending.anchor.clone()
    } else {
        let facts = project.definitions(db, moved.iter().map(String::as_str))?;
        anchor_for(
            project,
            db,
            &caller.login,
            branch,
            base,
            list,
            &moved,
            &facts,
        )?
        .0
    };
    if !within(&anchor, &pending.anchor) {
        return fail(
            409,
            "this merge touches more than it did when it began: start the merge again",
        );
    }
    if current.as_ref() != Some(base) {
        return fail(
            409,
            format!("`{branch}` moved since you looked: merge again onto what it is now"),
        );
    }
    // Its objects into the stores the landing calls for, before it is
    // written down: under the db lock nothing lands beside it, and a landing
    // whose objects have nowhere safe to go does not land.
    land(project, db, &caller.login, branch, list)?;
    // The commit point.
    let transaction = db.transaction()?;
    if db::list_of(&transaction, &project.key, branch)?.as_ref() != Some(base) {
        return fail(
            409,
            format!("`{branch}` moved since you looked: merge again onto what it is now"),
        );
    }
    let now = db::now_iso();
    for tracker in &moved {
        let (old, new) = (base.get(tracker), list.get(tracker));
        match new {
            Some(new) => transaction.execute(
                "INSERT OR REPLACE INTO heads VALUES (?, ?, ?, ?)",
                params![project.key, branch, tracker, new],
            )?,
            None => transaction.execute(
                "DELETE FROM heads WHERE home = ? AND branch = ? AND tracker = ?",
                params![project.key, branch, tracker],
            )?,
        };
        transaction.execute(
            "INSERT INTO head_log (home, branch, tracker, old, new, moved_by, moved_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![project.key, branch, tracker, old, new, caller.login, now],
        )?;
    }
    transaction.execute(
        "UPDATE branches SET updated_at = ? WHERE home = ? AND name = ?",
        params![now, project.key, branch],
    )?;
    transaction.execute(
        "DELETE FROM grants WHERE token_id = ? AND refs = 'refs/newgit/landing/'",
        [&caller.token_id],
    )?;
    transaction.commit()?;
    let behind = follow(project, branch, list, &asked)?;
    project.app.trackers_moved();
    // A landing may have changed definitions, and so audiences.
    project.settle_after_landing(db);
    answer(db, project, branch, behind, anchor)
}

fn answer(
    db: &Connection,
    project: &ProjectContext<'_>,
    branch: &str,
    behind: Vec<String>,
    anchor: String,
) -> ApiResult<ApiResponse> {
    let mut found = db::branches(db, &project.key)?
        .into_iter()
        .find(|found| found["name"] == branch)
        .unwrap_or(Value::Null);
    found["behind"] = json!(behind);
    found["anchor"] = json!(anchor);
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

/// Every commit of `list` waiting in `by`'s landings, brought into the store
/// its tracker's definition calls for as this landing leaves it: the tracker
/// moves there first, then its new commits follow it. So a landing that
/// narrows a tracker's audience never writes what it lands into the wider
/// store. Definitions are read from each tracker's `main`, so only a landing
/// on `main` changes them; for one, they are read from the commits it lands.
/// Every store a tracker moves to is checked before anything moves, so a
/// store in the way made for another audience refuses the landing whole.
/// Run again by a finish asked again: what is in place already is left.
fn land(
    project: &ProjectContext<'_>,
    db: &Connection,
    by: &str,
    branch: &str,
    list: &List,
) -> ApiResult<()> {
    let names = list.keys().map(String::as_str);
    let after = if branch == "main" {
        let mut at = BTreeMap::new();
        for (tracker, commit) in list {
            if !project.links.contains_key(tracker) {
                let view = project.tracker(tracker).view(by)?;
                at.insert(tracker.clone(), (view, commit.clone()));
            }
        }
        policy::load_definitions_at(&project.app.config.data, db, &project.id, names, &at)
    } else {
        project.definitions(db, names)?
    };
    for tracker in list.keys() {
        let Some(fact) = after.get(tracker) else {
            continue;
        };
        let to = repos::Store::for_tracker(Some(fact), tracker);
        if !project.links.contains_key(tracker) && project.own(tracker).store != to {
            repos::check_store(&to.dir(&project.home()), &to)?;
        }
    }
    for (tracker, commit) in list {
        project.settle(db, tracker, after.get(tracker))?;
        project.tracker(tracker).take_landing(by, commit)?;
    }
    Ok(())
}

/// Each repo's branch after a landing: root's to the list, each asked-for
/// move by compare-and-swap, and the landing refs gone. What could not move
/// (a child's `main` moved meanwhile) is said, and the landing stands.
fn follow(
    project: &ProjectContext<'_>,
    branch: &str,
    list: &List,
    moves: &[RefMove],
) -> ApiResult<Vec<String>> {
    let mut behind = Vec::new();
    project
        .tracker("root")
        .set_refs(&[(format!("refs/heads/{branch}"), list["root"].clone())])?;
    for step in moves {
        let repo = project.tracker(&step.tracker);
        let reference = format!("refs/heads/{}", step.branch);
        let moved = repo.update_refs(&[RefUpdate {
            reference: &reference,
            new: &step.new,
            old: step.old.as_deref(),
        }])?;
        if !moved && repo.branch_head(&step.branch).as_deref() != Some(step.new.as_str()) {
            behind.push(format!(
                "`{}`'s `{}` moved while this merge ran",
                step.tracker, step.branch
            ));
        }
    }
    for (tracker, commit) in list {
        let _ = project.tracker(tracker).delete_ref(&landing_ref(commit));
    }
    Ok(behind)
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
