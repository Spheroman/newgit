//! The mirrors' rows in SQLite: the pull queue, how pulls ended, what
//! GitHub's pull request events said, and how each export stands. None of it
//! is a landing: pulls land through `merge`, and exports are derived.

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ApiResult;

pub const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS mirror_pulls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    home TEXT NOT NULL,
    target TEXT NOT NULL, repo TEXT NOT NULL, branch TEXT NOT NULL,
    commit_id TEXT NOT NULL, pusher TEXT NOT NULL, delivery TEXT,
    queued_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT, retry_at INTEGER, step TEXT, lookup TEXT,
    touched_at INTEGER NOT NULL, tag TEXT);
CREATE INDEX IF NOT EXISTS mirror_pulls_by_home ON mirror_pulls (home, id);
CREATE TABLE IF NOT EXISTS mirror_ended (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    home TEXT NOT NULL,
    target TEXT NOT NULL, commit_id TEXT NOT NULL, outcome TEXT NOT NULL,
    why TEXT, at INTEGER NOT NULL, blocked_by TEXT, tag TEXT);
CREATE INDEX IF NOT EXISTS mirror_ended_by_home ON mirror_ended (home, seq);
CREATE TABLE IF NOT EXISTS mirror_heads (
    home TEXT NOT NULL,
    repo TEXT NOT NULL, merged TEXT NOT NULL, head TEXT NOT NULL, branch TEXT NOT NULL,
    at INTEGER NOT NULL, PRIMARY KEY (home, repo, merged, head));
CREATE TABLE IF NOT EXISTS mirror_exports (
    home TEXT NOT NULL,
    target TEXT NOT NULL, url TEXT NOT NULL,
    tip TEXT, exported_at INTEGER, behind INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0, last_error TEXT, retry_at INTEGER,
    PRIMARY KEY (home, target));
";

/// How many ended pulls are kept per target, of those said one by one: what
/// `newgit status` reads.
pub const ENDED_KEPT: i64 = 10;
/// How many pulls waiting on a refused one are kept per target: they are
/// only counted, and never push out the refusal they wait on.
pub const WAITING_KEPT: i64 = 100;
/// How many tags' pulls are kept per target, each tag's latest.
pub const TAGS_KEPT: i64 = 20;

pub fn create(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(SCHEMA)
}

/// A project deleted: its mirrors' rows go with it, so one made again under
/// its name starts with none. Not foreign keys to `projects`: every landing
/// updates that row, and the checks cost a landing's transaction more than
/// a request waiting on the db lock can spare (`rooted_lock_wait.rs`).
pub fn forget(db: &Connection, home: &str) -> ApiResult<()> {
    for table in [
        "mirror_pulls",
        "mirror_ended",
        "mirror_heads",
        "mirror_exports",
    ] {
        db.execute(&format!("DELETE FROM {table} WHERE home = ?"), [home])?;
    }
    Ok(())
}

/// Asking GitHub which pull request a commit came from: since when it has
/// listed none, when it was last asked, and why it could not answer, how
/// many times in a row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lookup {
    #[serde(default)]
    pub since: Option<i64>,
    pub last: i64,
    #[serde(default)]
    pub failed: Option<String>,
    #[serde(default)]
    pub failures: u32,
}

/// One verified push event, waiting for or being worked on.
#[derive(Debug, Clone)]
pub struct Pull {
    pub id: i64,
    pub home: String,
    pub target: String,
    /// `owner/name` on GitHub, as the signed event gives it.
    pub repo: String,
    pub branch: String,
    /// What the event said moved there: the branch's commit, or for a tag
    /// its object (the tag object of an annotated one).
    pub commit: String,
    /// A tag made on the mirror, by name: nothing lands but the tag.
    pub tag: Option<String>,
    /// Who pushed, by GitHub login: the login newgit knows them by.
    pub pusher: String,
    pub queued_at: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub retry_at: Option<i64>,
    pub step: Option<String>,
    pub lookup: Option<Lookup>,
}

fn pull_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Pull> {
    let lookup: Option<String> = row.get("lookup")?;
    Ok(Pull {
        id: row.get("id")?,
        home: row.get("home")?,
        target: row.get("target")?,
        repo: row.get("repo")?,
        branch: row.get("branch")?,
        commit: row.get("commit_id")?,
        tag: row.get("tag")?,
        pusher: row.get("pusher")?,
        queued_at: row.get("queued_at")?,
        attempts: row.get("attempts")?,
        last_error: row.get("last_error")?,
        retry_at: row.get("retry_at")?,
        step: row.get("step")?,
        lookup: lookup.and_then(|text| serde_json::from_str(&text).ok()),
    })
}

/// What queuing an event did.
#[derive(Debug, PartialEq, Eq)]
pub enum Queued {
    New(i64),
    /// This delivery, or a pull of the same commit to the same target, is
    /// queued already.
    Already(i64),
}

/// Queue a pull, unless one of the same commit for the same target is
/// waiting already (a redelivery, or GitHub sending one event twice). A
/// delivery whose pull has ended is queued again: redelivering is how a
/// pull is asked again. `tag`: a tag made on the mirror, `commit` its
/// object; a tag's pull and its commit's branch pull are two pulls, and the
/// tag's waits behind every pull queued before it, so a pull request merged
/// and then tagged is on `main` when its tag is looked for.
#[expect(clippy::too_many_arguments, reason = "one row's columns")]
pub fn queue(
    db: &Connection,
    home: &str,
    target: &str,
    repo: &str,
    branch: &str,
    commit: &str,
    tag: Option<&str>,
    pusher: &str,
    delivery: Option<&str>,
    now: i64,
) -> ApiResult<Queued> {
    let waiting: Option<i64> = db
        .query_row(
            "SELECT id FROM mirror_pulls WHERE home = ?1 AND ((target = ?2 AND commit_id = ?3 AND tag IS ?5) OR (delivery IS NOT NULL AND delivery = ?4)) ORDER BY id LIMIT 1",
            params![home, target, commit, delivery, tag],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = waiting {
        return Ok(Queued::Already(id));
    }
    db.execute(
        "INSERT INTO mirror_pulls (home, target, repo, branch, commit_id, pusher, delivery, queued_at, touched_at, step, tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 'queued', ?9)",
        params![home, target, repo, branch, commit, pusher, delivery, now, tag],
    )?;
    Ok(Queued::New(db.last_insert_rowid()))
}

pub fn pulls(db: &Connection, home: &str) -> ApiResult<Vec<Pull>> {
    let mut statement =
        db.prepare_cached("SELECT * FROM mirror_pulls WHERE home = ? ORDER BY id")?;
    let rows = statement.query_map([home], pull_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The projects with a pull waiting, each with its oldest.
pub fn heads(db: &Connection) -> ApiResult<Vec<Pull>> {
    let mut statement = db.prepare_cached(
        "SELECT * FROM mirror_pulls WHERE id IN (SELECT MIN(id) FROM mirror_pulls GROUP BY home) ORDER BY id",
    )?;
    let rows = statement.query_map([], pull_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn set_step(db: &Connection, id: i64, step: &str, now: i64) -> ApiResult<()> {
    db.execute(
        "UPDATE mirror_pulls SET step = ?2, touched_at = ?3 WHERE id = ?1",
        params![id, step, now],
    )?;
    Ok(())
}

pub fn set_lookup(db: &Connection, id: i64, lookup: &Lookup, now: i64) -> ApiResult<()> {
    db.execute(
        "UPDATE mirror_pulls SET lookup = ?2, step = 'asking GitHub', touched_at = ?3 WHERE id = ?1",
        params![id, serde_json::to_string(lookup).unwrap_or_default(), now],
    )?;
    Ok(())
}

/// A failed attempt: said, counted, and tried again at `retry_at`. Returns
/// how many attempts have failed.
pub fn failed(db: &Connection, id: i64, why: &str, retry_at: i64, now: i64) -> ApiResult<u32> {
    db.execute(
        "UPDATE mirror_pulls SET attempts = attempts + 1, last_error = ?2, retry_at = ?3, touched_at = ?4 WHERE id = ?1",
        params![id, why, retry_at, now],
    )?;
    Ok(db.query_row(
        "SELECT attempts FROM mirror_pulls WHERE id = ?",
        [id],
        |row| row.get(0),
    )?)
}

/// How a pull ended, for [`end`].
#[derive(Debug, Clone, Copy)]
pub struct Ending<'a> {
    /// `landed`, `nothing`, `refused`, or `gave up`; for a tag's pull,
    /// `tagged` when it made the tag and `nothing` when it was there.
    pub outcome: &'a str,
    pub why: Option<&'a str>,
    /// The commit of the refused pull this one waits on.
    pub blocked_by: Option<&'a str>,
    /// Whether the target's trouble is over with it: it landed, or the mirror
    /// has nothing `main` lacks. A pull that only found its own commit on
    /// `main` (an old delivery sent again) while the mirror holds more says
    /// nothing of that.
    pub settles: bool,
}

impl Ending<'_> {
    fn is_trouble(&self) -> bool {
        matches!(self.outcome, "refused" | "gave up")
    }
}

/// A pull ends: off the queue, and how it ended kept for `newgit status`.
///
/// What is kept, per target: the last pull that settled it (landed, or found
/// nothing), then the trouble since. So one that settles replaces every row
/// the target had; a pull refused again (a redelivery) replaces its own; and
/// one that settles nothing and is no trouble is kept only when there is no
/// trouble for it to hide.
///
/// A tag's pull is kept beside those, as its tag's latest: it lands nothing,
/// so it neither ends the branch's trouble nor is ended with it. A refused
/// tag stays said until that tag is made (or found made).
pub fn end(db: &Connection, pull: &Pull, ending: &Ending<'_>, now: i64) -> ApiResult<()> {
    db.execute("DELETE FROM mirror_pulls WHERE id = ?", [pull.id])?;
    let target = params![pull.home, pull.target];
    let insert = |db: &Connection| {
        db.execute(
            "INSERT INTO mirror_ended (home, target, commit_id, outcome, why, at, blocked_by, tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![pull.home, pull.target, pull.commit, ending.outcome, ending.why, now, ending.blocked_by, pull.tag],
        )
    };
    if let Some(tag) = &pull.tag {
        db.execute(
            "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag = ?3",
            params![pull.home, pull.target, tag],
        )?;
        insert(db)?;
        db.execute(
            "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NOT NULL AND seq NOT IN (SELECT seq FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NOT NULL ORDER BY seq DESC LIMIT ?3)",
            params![pull.home, pull.target, TAGS_KEPT],
        )?;
        return Ok(());
    }
    if ending.settles {
        db.execute(
            "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL",
            target,
        )?;
    } else if ending.is_trouble() {
        db.execute(
            "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND commit_id = ?3 AND outcome IN ('refused', 'gave up')",
            params![pull.home, pull.target, pull.commit],
        )?;
    } else {
        let troubled: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND outcome IN ('refused', 'gave up'))",
            target,
            |row| row.get(0),
        )?;
        if troubled {
            return Ok(());
        }
        db.execute(
            "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL",
            target,
        )?;
    }
    insert(db)?;
    // Those said one by one and those only counted are kept apart, so a run
    // of pulls waiting on a refusal never pushes that refusal out; and none
    // is kept waiting on one that is gone.
    for (waiting, kept) in [("IS NULL", ENDED_KEPT), ("IS NOT NULL", WAITING_KEPT)] {
        db.execute(
            &format!(
                "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND blocked_by {waiting} AND seq NOT IN (SELECT seq FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND blocked_by {waiting} ORDER BY seq DESC LIMIT ?3)"
            ),
            params![pull.home, pull.target, kept],
        )?;
    }
    db.execute(
        "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND blocked_by IS NOT NULL AND blocked_by NOT IN (SELECT commit_id FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND blocked_by IS NULL AND outcome = 'refused')",
        target,
    )?;
    Ok(())
}

/// A pull of `target` was refused for a reason of its own, and would have
/// landed `walk` too: the trouble kept for pulls of those commits is its
/// trouble now. What stopped them may be over (the refused pull got past
/// it), and said beside this refusal it would be advice already taken.
pub fn supersede(db: &Connection, home: &str, target: &str, walk: &[String]) -> ApiResult<()> {
    for ended in ended(db, home)? {
        if ended.target == target
            && ended.tag.is_none()
            && matches!(ended.outcome.as_str(), "refused" | "gave up")
            && walk.contains(&ended.commit)
        {
            db.execute(
                "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND commit_id = ?3",
                params![home, target, ended.commit],
            )?;
        }
    }
    Ok(())
}

/// `target`'s mirror is where `main` is (an export just found or put it
/// there): whatever was refused or given up from it is over, with or without
/// an event to say so. Not a refused tag: an export makes none.
pub fn over(db: &Connection, home: &str, target: &str) -> ApiResult<()> {
    db.execute(
        "DELETE FROM mirror_ended WHERE home = ?1 AND target = ?2 AND tag IS NULL AND outcome IN ('refused', 'gave up')",
        params![home, target],
    )?;
    Ok(())
}

/// One pull that ended.
#[derive(Debug, Clone)]
pub struct Ended {
    pub target: String,
    pub commit: String,
    pub outcome: String,
    pub why: Option<String>,
    pub at: i64,
    /// The commit of the refused pull this one waits on, when it was
    /// refused only for carrying that one's commits.
    pub blocked_by: Option<String>,
    /// The tag it was the pull of.
    pub tag: Option<String>,
}

/// Every pull kept that ended, oldest first.
pub fn ended(db: &Connection, home: &str) -> ApiResult<Vec<Ended>> {
    let mut statement = db.prepare_cached(
        "SELECT target, commit_id, outcome, why, at, blocked_by, tag FROM mirror_ended WHERE home = ? ORDER BY seq",
    )?;
    let rows = statement.query_map([home], |row| {
        Ok(Ended {
            target: row.get(0)?,
            commit: row.get(1)?,
            outcome: row.get(2)?,
            why: row.get(3)?,
            at: row.get(4)?,
            blocked_by: row.get(5)?,
            tag: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A pull request GitHub said was merged as `merged`, from `head` on branch
/// `branch` of the same repository.
pub fn note_head(
    db: &Connection,
    home: &str,
    repo: &str,
    merged: &str,
    head: &str,
    branch: &str,
    now: i64,
) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO mirror_heads (home, repo, merged, head, branch, at) VALUES (?1, lower(?2), ?3, ?4, ?5, ?6)",
        params![home, repo, merged, head, branch, now],
    )?;
    // Kept a month: long enough for any pull to ask, short enough to stay small.
    db.execute(
        "DELETE FROM mirror_heads WHERE home = ?1 AND at < ?2",
        params![home, now - 30 * 86_400],
    )?;
    Ok(())
}

/// The heads of the pull requests GitHub's events said were merged as `merged`.
pub fn heads_of(db: &Connection, home: &str, repo: &str, merged: &str) -> ApiResult<Vec<String>> {
    let mut statement = db.prepare_cached(
        "SELECT head FROM mirror_heads WHERE home = ?1 AND repo = lower(?2) AND merged = ?3",
    )?;
    let rows = statement.query_map(params![home, repo, merged], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// How one export target stands.
#[derive(Debug, Clone, Default)]
pub struct Export {
    pub target: String,
    pub url: String,
    /// What was last pushed (or found there already).
    pub tip: Option<String>,
    pub exported_at: Option<i64>,
    /// Landings on the project since the last export that finished.
    pub behind: i64,
    pub failures: u32,
    pub last_error: Option<String>,
    pub retry_at: Option<i64>,
}

pub fn exports(db: &Connection, home: &str) -> ApiResult<Vec<Export>> {
    let mut statement = db.prepare_cached(
        "SELECT target, url, tip, exported_at, behind, failures, last_error, retry_at FROM mirror_exports WHERE home = ? ORDER BY target",
    )?;
    let rows = statement.query_map([home], |row| {
        Ok(Export {
            target: row.get(0)?,
            url: row.get(1)?,
            tip: row.get(2)?,
            exported_at: row.get(3)?,
            behind: row.get(4)?,
            failures: row.get(5)?,
            last_error: row.get(6)?,
            retry_at: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// `landings` more landings on `home`: each of its targets is that much
/// further behind, until its export finishes.
pub fn add_behind(db: &Connection, home: &str, landings: u64) -> ApiResult<()> {
    db.execute(
        "UPDATE mirror_exports SET behind = behind + ?2 WHERE home = ?1",
        params![home, i64::try_from(landings).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Make sure `target` has a row: a target new to `main` starts one landing behind.
pub fn ensure_export(db: &Connection, home: &str, target: &str, url: &str) -> ApiResult<()> {
    db.execute(
        "INSERT INTO mirror_exports (home, target, url, behind) VALUES (?1, ?2, ?3, 1) ON CONFLICT (home, target) DO UPDATE SET url = excluded.url",
        params![home, target, url],
    )?;
    Ok(())
}

/// Targets no longer declared on `main`: their rows go.
pub fn keep_exports(db: &Connection, home: &str, targets: &[String]) -> ApiResult<()> {
    let listed: Vec<String> = exports(db, home)?
        .into_iter()
        .map(|export| export.target)
        .collect();
    for target in listed {
        if !targets.contains(&target) {
            db.execute(
                "DELETE FROM mirror_exports WHERE home = ?1 AND target = ?2",
                params![home, target],
            )?;
        }
    }
    Ok(())
}

/// An export finished: the mirror has `tip`, and is behind by what landed
/// since the lists it was composed from were read (`seen` landings).
pub fn exported(
    db: &Connection,
    home: &str,
    target: &str,
    tip: &str,
    seen: i64,
    now: i64,
) -> ApiResult<()> {
    db.execute(
        "UPDATE mirror_exports SET tip = ?3, exported_at = ?5, behind = MAX(behind - ?4, 0), failures = 0, last_error = NULL, retry_at = NULL WHERE home = ?1 AND target = ?2",
        params![home, target, tip, seen, now],
    )?;
    Ok(())
}

pub fn export_failed(
    db: &Connection,
    home: &str,
    target: &str,
    why: &str,
    retry_at: i64,
) -> ApiResult<u32> {
    db.execute(
        "UPDATE mirror_exports SET failures = failures + 1, last_error = ?3, retry_at = ?4 WHERE home = ?1 AND target = ?2",
        params![home, target, why, retry_at],
    )?;
    Ok(db.query_row(
        "SELECT failures FROM mirror_exports WHERE home = ?1 AND target = ?2",
        params![home, target],
        |row| row.get(0),
    )?)
}

fn iso(seconds: i64) -> String {
    crate::db::iso_at(seconds)
}

/// What `GET …/state` says of the mirrors: pulls waiting, pulls that ended
/// (per target, the last that settled it and the trouble since; a run of
/// refusals that each only waited on the first said once, on the first), and
/// every export.
pub fn state(db: &Connection, home: &str, threads_running: bool, now: i64) -> ApiResult<Value> {
    let waiting = pulls(db, home)?;
    let mut pulls_json = Vec::new();
    for (place, pull) in waiting.iter().enumerate() {
        let step = if place == 0 {
            pull.step.clone()
        } else {
            Some("queued".to_owned())
        };
        let mut entry = json!({
            "target": pull.target,
            "commit": pull.commit,
            "queuedAt": iso(pull.queued_at),
            "attempts": pull.attempts,
            "lastError": pull.last_error,
            "step": step,
        });
        if let Some(tag) = &pull.tag {
            entry["tag"] = json!(tag);
        }
        if let Some(at) = pull.retry_at.filter(|at| *at > now) {
            entry["retryAt"] = json!(iso(at));
        }
        if let Some(failed) = pull
            .lookup
            .as_ref()
            .and_then(|lookup| lookup.failed.clone())
        {
            entry["lookupFailed"] = json!(failed);
        }
        if !threads_running {
            entry["stalled"] = json!(true);
        }
        pulls_json.push(entry);
    }
    let all = ended(db, home)?;
    let mut ended_json: Vec<Value> = Vec::new();
    for one in &all {
        if one.blocked_by.is_some() {
            continue;
        }
        let mut why = one.why.clone();
        // Each pull waiting on this one, once, however often it was sent.
        let mut waiting_on: Vec<&str> = Vec::new();
        for other in &all {
            if other.target == one.target
                && other.blocked_by.as_deref() == Some(one.commit.as_str())
                && !waiting_on.contains(&other.commit.as_str())
            {
                waiting_on.push(&other.commit);
            }
        }
        if let Some(last) = waiting_on.last() {
            let count = waiting_on.len();
            why = Some(format!(
                "{}. {count} later pull{} (the latest at {}) carr{} this commit and wait{} on it",
                why.unwrap_or_default().trim_end_matches('.'),
                if count == 1 { "" } else { "s" },
                &last[..last.len().min(12)],
                if count == 1 { "ies" } else { "y" },
                if count == 1 { "s" } else { "" },
            ));
        }
        let mut entry = json!({
            "target": one.target, "commit": one.commit, "outcome": one.outcome,
            "why": why, "at": iso(one.at),
        });
        if let Some(tag) = &one.tag {
            entry["tag"] = json!(tag);
        }
        ended_json.push(entry);
    }
    // What is kept is exactly what is to be said, so trouble comes last: a
    // reader that takes a later `landed` or `tagged` for the end of an
    // earlier refusal (as `newgit status` does, per target) is not told a
    // tag made after a refusal ended it.
    ended_json.sort_by_key(|one| matches!(one["outcome"].as_str(), Some("refused" | "gave up")));
    let exports_json: Vec<Value> = exports(db, home)?
        .into_iter()
        .map(|export| {
            let state = if export.last_error.is_some() {
                "failing"
            } else if export.behind > 0 {
                "behind"
            } else {
                "up to date"
            };
            json!({
                "target": export.target, "url": export.url, "state": state,
                "behind": export.behind, "tip": export.tip,
                "exportedAt": export.exported_at.map(iso),
                "failures": export.failures, "lastError": export.last_error,
                "retryAt": export.retry_at.filter(|at| *at > now).map(iso),
            })
        })
        .collect();
    Ok(json!({ "pulls": pulls_json, "ended": ended_json, "exports": exports_json }))
}
