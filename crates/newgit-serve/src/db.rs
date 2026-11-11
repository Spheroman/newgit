//! Everything *about* history, in one SQLite database: what the Worker kept
//! in its Durable Objects (users' tokens, each project's members, branches,
//! instances, forks, checkpoints, tags, links), and which store each
//! tracker's history is in.
//!
//! Branches' lists (`heads`, `head_log`) and which store each tracker is in
//! (`tracker_stores`) follow the landing log (`crate::landing`): they are
//! written only by applying a landing, each landing in one transaction, and
//! repaired from the log at startup. `landings_placed` says which landings
//! are in place, written in the same transaction as the rows they make, and
//! `landing_applied` is the stamp every landing up to is (the rows below it
//! are folded into it).
//!
//! Every project-scoped table is keyed by `home` first (`<owner>/<project>`),
//! so moving a set of homes to another node is a `WHERE home ...` away. One
//! connection behind one lock: SQLite serializes writers anyway, and a write
//! here takes microseconds.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use camino::Utf8Path;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::{Value, json};

use crate::error::ApiResult;

const SCHEMA: &str = r"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS tokens (
    id TEXT PRIMARY KEY, login TEXT NOT NULL, kind TEXT NOT NULL, agent TEXT,
    project TEXT, instance TEXT, created_at TEXT NOT NULL, expires_at TEXT NOT NULL,
    revoked_at TEXT, last_used_at TEXT);
CREATE INDEX IF NOT EXISTS tokens_by_login ON tokens (login);
CREATE TABLE IF NOT EXISTS projects (
    home TEXT PRIMARY KEY, owner TEXT NOT NULL, name TEXT NOT NULL,
    created_at TEXT NOT NULL, default_branch TEXT NOT NULL DEFAULT 'main');
CREATE TABLE IF NOT EXISTS members (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, login TEXT NOT NULL,
    added_by TEXT NOT NULL, added_at TEXT NOT NULL, PRIMARY KEY (home, login));
CREATE TABLE IF NOT EXISTS branches (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, name TEXT NOT NULL,
    created_by TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    PRIMARY KEY (home, name));
CREATE TABLE IF NOT EXISTS heads (
    home TEXT NOT NULL, branch TEXT NOT NULL, tracker TEXT NOT NULL, commit_id TEXT NOT NULL,
    PRIMARY KEY (home, branch, tracker),
    FOREIGN KEY (home, branch) REFERENCES branches ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS head_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, home TEXT NOT NULL, branch TEXT NOT NULL,
    tracker TEXT NOT NULL, old TEXT, new TEXT, moved_by TEXT NOT NULL, moved_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS instances (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, name TEXT NOT NULL,
    owner TEXT NOT NULL, agent TEXT, base_rev TEXT NOT NULL, head_rev TEXT,
    private INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    PRIMARY KEY (home, name));
CREATE TABLE IF NOT EXISTS forks (
    home TEXT NOT NULL, instance TEXT NOT NULL, tracker TEXT NOT NULL,
    PRIMARY KEY (home, instance, tracker),
    FOREIGN KEY (home, instance) REFERENCES instances ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS checkpoints (
    home TEXT NOT NULL, instance TEXT NOT NULL, id TEXT NOT NULL, created_by TEXT NOT NULL,
    agent TEXT, created_at TEXT NOT NULL, record TEXT NOT NULL,
    PRIMARY KEY (home, instance, id),
    FOREIGN KEY (home, instance) REFERENCES instances ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS tags (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, name TEXT NOT NULL,
    list TEXT NOT NULL, annotation TEXT, created_by TEXT NOT NULL, created_at TEXT NOT NULL,
    PRIMARY KEY (home, name));
CREATE TABLE IF NOT EXISTS links (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, tracker TEXT NOT NULL,
    home_project TEXT NOT NULL, home_tracker TEXT NOT NULL, PRIMARY KEY (home, tracker));
CREATE TABLE IF NOT EXISTS tracker_stores (
    home TEXT NOT NULL REFERENCES projects ON DELETE CASCADE, tracker TEXT NOT NULL,
    store TEXT NOT NULL, PRIMARY KEY (home, tracker));
CREATE TABLE IF NOT EXISTS landing_applied (
    id INTEGER PRIMARY KEY CHECK (id = 1), stamp INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS landings_placed (
    stamp INTEGER PRIMARY KEY, finished INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS landings_voided (
    stamp INTEGER NOT NULL, by_stamp INTEGER NOT NULL, home TEXT NOT NULL,
    PRIMARY KEY (stamp, home));
CREATE TABLE IF NOT EXISTS grants (
    repo TEXT NOT NULL, token_id TEXT NOT NULL, refs TEXT NOT NULL, expires_at INTEGER NOT NULL,
    PRIMARY KEY (repo, token_id, refs));
";

/// Columns added since a table was first made: added to a database made
/// before them.
const ADDED: &[(&str, &str, &str)] = &[
    ("branches", "stamp", "INTEGER"),
    ("head_log", "stamp", "INTEGER"),
    ("projects", "incarnation", "TEXT"),
];

pub struct Db {
    connection: Mutex<Connection>,
}

thread_local! {
    static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The one connection, locked. While a thread holds it, that thread may not
/// wait on a landing ([`held_here`]): applying one takes this lock.
pub struct DbGuard<'a>(MutexGuard<'a, Connection>);

impl std::ops::Deref for DbGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl std::ops::DerefMut for DbGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.0
    }
}

impl Drop for DbGuard<'_> {
    fn drop(&mut self) {
        HELD.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

/// Whether this thread holds the db lock.
pub fn held_here() -> bool {
    HELD.with(|held| held.get() > 0)
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn iso_at(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A branch's list: one commit per tracker in Git, by tracker name.
pub type List = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub owner: String,
    pub name: String,
    pub created_at: String,
    pub default_branch: String,
    /// Which project of this name it is: made new each time a project is
    /// made, so a landing written for a deleted one never acts on one made
    /// again under its name (`crate::landing`).
    pub incarnation: String,
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub owner: String,
    pub agent: Option<String>,
    pub base_rev: String,
    pub head_rev: Option<String>,
    pub forks: Vec<String>,
    pub private: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Instance {
    pub fn json(&self) -> Value {
        json!({
            "name": self.name, "owner": self.owner, "agent": self.agent,
            "baseRev": self.base_rev, "headRev": self.head_rev, "forks": self.forks,
            "private": self.private, "createdAt": self.created_at, "updatedAt": self.updated_at,
        })
    }

    /// Only its owner sees a private instance.
    pub fn visible_to(&self, login: &str) -> bool {
        !self.private || self.owner == login
    }
}

/// A token as recorded: what `GET /v1/tokens` lists.
#[derive(Debug, Clone)]
pub struct TokenRecord {
    pub id: String,
    pub login: String,
    pub kind: String,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub instance: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

impl TokenRecord {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "agent": self.agent, "project": self.project,
            "instance": self.instance, "createdAt": self.created_at, "expiresAt": self.expires_at,
            "revokedAt": self.revoked_at, "lastUsedAt": self.last_used_at,
        })
    }
}

impl Db {
    pub fn open(path: &Utf8Path) -> anyhow::Result<Self> {
        let connection = Connection::open(path.as_std_path())?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        for (table, column, kind) in ADDED {
            let has: bool = connection
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?"
                ))?
                .exists([column])?;
            if !has {
                connection
                    .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
            }
        }
        // A landing applied again (replayed at startup) logs nothing twice.
        connection.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS head_log_by_stamp ON head_log (stamp, home, branch, tracker)",
        )?;
        // A project from before incarnations is its first one.
        connection.execute(
            "UPDATE projects SET incarnation = lower(hex(randomblob(16))) WHERE incarnation IS NULL",
            [],
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn lock(&self) -> DbGuard<'_> {
        let guard = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HELD.with(|held| held.set(held.get() + 1));
        DbGuard(guard)
    }
}

// ── Tokens ────────────────────────────────────────────────────────────────

pub fn register_token(db: &Connection, claims: &crate::tokens::Claims) -> ApiResult<TokenRecord> {
    db.execute(
        "INSERT INTO tokens (id, login, kind, agent, project, instance, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            claims.jti,
            claims.sub,
            claims.kind,
            claims.agent,
            claims.project,
            claims.instance,
            iso_at(claims.iat),
            iso_at(claims.exp)
        ],
    )?;
    Ok(token(db, &claims.jti)?.unwrap_or_else(|| unreachable!()))
}

fn token_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRecord> {
    Ok(TokenRecord {
        id: row.get("id")?,
        login: row.get("login")?,
        kind: row.get("kind")?,
        agent: row.get("agent")?,
        project: row.get("project")?,
        instance: row.get("instance")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        revoked_at: row.get("revoked_at")?,
        last_used_at: row.get("last_used_at")?,
    })
}

pub fn token(db: &Connection, id: &str) -> ApiResult<Option<TokenRecord>> {
    Ok(db
        .query_row("SELECT * FROM tokens WHERE id = ?", [id], token_row)
        .optional()?)
}

/// Whether token `id` is recorded for `login` and not revoked; notes its use,
/// to the minute.
pub fn check_token(db: &Connection, login: &str, id: &str) -> ApiResult<bool> {
    let live = db
        .query_row(
            "SELECT last_used_at FROM tokens WHERE id = ? AND login = ? AND revoked_at IS NULL",
            [id, login],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(last) = live else { return Ok(false) };
    let now = now_iso();
    if last
        .as_deref()
        .is_none_or(|last| last.get(..16) != now.get(..16))
    {
        db.execute(
            "UPDATE tokens SET last_used_at = ? WHERE id = ?",
            [&now, id],
        )?;
    }
    Ok(true)
}

pub fn tokens_of(db: &Connection, login: &str) -> ApiResult<Vec<TokenRecord>> {
    let mut statement =
        db.prepare("SELECT * FROM tokens WHERE login = ? ORDER BY created_at, id")?;
    let rows = statement.query_map([login], token_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn revoke_token(db: &Connection, login: &str, id: &str) -> ApiResult<bool> {
    let changed = db.execute(
        "UPDATE tokens SET revoked_at = ? WHERE id = ? AND login = ? AND revoked_at IS NULL",
        params![now_iso(), id, login],
    )?;
    db.execute("DELETE FROM grants WHERE token_id = ?", [id])?;
    Ok(changed > 0)
}

/// Revoke every token pinned to `project` (and, given, to its `instance`).
pub fn revoke_pinned(db: &Connection, project: &str, instance: Option<&str>) -> ApiResult<usize> {
    let changed = match instance {
        None => db.execute(
            "UPDATE tokens SET revoked_at = ? WHERE project = ? AND revoked_at IS NULL",
            params![now_iso(), project],
        )?,
        Some(instance) => db.execute(
            "UPDATE tokens SET revoked_at = ? WHERE project = ? AND instance = ? AND revoked_at IS NULL",
            params![now_iso(), project, instance],
        )?,
    };
    Ok(changed)
}

// ── Projects ──────────────────────────────────────────────────────────────

pub fn project(db: &Connection, home: &str) -> ApiResult<Option<ProjectInfo>> {
    Ok(db
        .query_row(
            "SELECT owner, name, created_at, default_branch, incarnation FROM projects WHERE home = ?",
            [home],
            |row| {
                Ok(ProjectInfo {
                    owner: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    default_branch: row.get(3)?,
                    incarnation: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                })
            },
        )
        .optional()?)
}

pub fn owned_count(db: &Connection, login: &str) -> ApiResult<usize> {
    let count: i64 = db.query_row(
        "SELECT count(*) FROM projects WHERE owner = ?",
        [login],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(0))
}

/// Create a project with its owner as its first member, as a new
/// incarnation; `false` when it exists. Only a landing makes a project on a
/// running server (`crate::landing`); this is also how a data directory from
/// before the landing log got its projects.
pub fn create_project(db: &Connection, home: &str, owner: &str, name: &str) -> ApiResult<bool> {
    create_incarnation(db, home, owner, name, &crate::tokens::random_hex(16))
}

/// [`create_project`] as incarnation `incarnation`.
pub fn create_incarnation(
    db: &Connection,
    home: &str,
    owner: &str,
    name: &str,
    incarnation: &str,
) -> ApiResult<bool> {
    let now = now_iso();
    let inserted = db.execute(
        "INSERT OR IGNORE INTO projects (home, owner, name, created_at, incarnation) VALUES (?, ?, ?, ?, ?)",
        params![home, owner, name, now, incarnation],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    db.execute(
        "INSERT INTO members VALUES (?, ?, ?, ?)",
        params![home, owner, owner, now],
    )?;
    Ok(true)
}

pub fn delete_project(db: &Connection, home: &str) -> ApiResult<()> {
    db.execute("DELETE FROM head_log WHERE home = ?", [home])?;
    db.execute("DELETE FROM projects WHERE home = ?", [home])?;
    db.execute(
        "DELETE FROM grants WHERE repo LIKE ? ESCAPE '\\'",
        [format!("{}.%", home.replace('/', ".").replace('_', "\\_"))],
    )?;
    Ok(())
}

pub fn is_member(db: &Connection, home: &str, login: &str) -> ApiResult<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM members WHERE home = ? AND login = ?",
            [home, login],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn members(db: &Connection, home: &str) -> ApiResult<Vec<Value>> {
    let mut statement =
        db.prepare("SELECT login, added_by, added_at FROM members WHERE home = ? ORDER BY login")?;
    let rows = statement.query_map([home], |row| {
        Ok(json!({ "login": row.get::<_, String>(0)?, "addedBy": row.get::<_, String>(1)?, "addedAt": row.get::<_, String>(2)? }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn add_member(db: &Connection, home: &str, login: &str, by: &str) -> ApiResult<bool> {
    Ok(db.execute(
        "INSERT OR IGNORE INTO members VALUES (?, ?, ?, ?)",
        params![home, login, by, now_iso()],
    )? > 0)
}

pub fn remove_member(db: &Connection, home: &str, login: &str) -> ApiResult<bool> {
    Ok(db.execute(
        "DELETE FROM members WHERE home = ? AND login = ?",
        [home, login],
    )? > 0)
}

// ── Branches ──────────────────────────────────────────────────────────────

pub fn list_of(db: &Connection, home: &str, branch: &str) -> ApiResult<Option<List>> {
    let exists = db
        .query_row(
            "SELECT 1 FROM branches WHERE home = ? AND name = ?",
            [home, branch],
            |_| Ok(()),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut statement =
        db.prepare("SELECT tracker, commit_id FROM heads WHERE home = ? AND branch = ?")?;
    let rows = statement.query_map([home, branch], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(Some(rows.collect::<Result<_, _>>()?))
}

pub fn branches(db: &Connection, home: &str) -> ApiResult<Vec<Value>> {
    let mut statement = db.prepare(
        "SELECT name, created_by, created_at, updated_at, stamp FROM branches WHERE home = ? ORDER BY name",
    )?;
    let rows: Vec<(String, String, String, String, Option<i64>)> = statement
        .query_map([home], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (name, by, created, updated, stamp) in rows {
        let list = list_of(db, home, &name)?.unwrap_or_default();
        out.push(json!({
            "name": name, "list": list, "createdBy": by, "createdAt": created,
            "updatedAt": updated, "stamp": stamp,
        }));
    }
    Ok(out)
}

/// Every tracker any branch of the project lists.
pub fn listed_trackers(db: &Connection, home: &str) -> ApiResult<Vec<String>> {
    let mut statement =
        db.prepare("SELECT DISTINCT tracker FROM heads WHERE home = ? ORDER BY tracker")?;
    let rows = statement.query_map([home], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Branch `branch` of `home` set to `list` by `by`, as landing `stamp` left
/// it: made if it is not yet, and each tracker it moves logged. Only applying
/// a landing calls this (`crate::landing`); run again for the same stamp, it
/// changes nothing.
pub fn set_branch(
    db: &Connection,
    home: &str,
    branch: &str,
    list: &List,
    by: &str,
    stamp: u64,
) -> ApiResult<()> {
    let now = now_iso();
    let stamp = i64::try_from(stamp).unwrap_or(i64::MAX);
    let before = list_of(db, home, branch)?;
    if before.is_none() {
        db.execute(
            "INSERT INTO branches (home, name, created_by, created_at, updated_at, stamp) VALUES (?, ?, ?, ?, ?, ?)",
            params![home, branch, by, now, now, stamp],
        )?;
    } else {
        db.execute(
            "UPDATE branches SET updated_at = ?, stamp = ? WHERE home = ? AND name = ?",
            params![now, stamp, home, branch],
        )?;
    }
    let before = before.unwrap_or_default();
    let trackers: std::collections::BTreeSet<&String> = before.keys().chain(list.keys()).collect();
    for tracker in trackers {
        let (old, new) = (before.get(tracker), list.get(tracker));
        if old == new {
            continue;
        }
        match new {
            Some(new) => db.execute(
                "INSERT OR REPLACE INTO heads VALUES (?, ?, ?, ?)",
                params![home, branch, tracker, new],
            )?,
            None => db.execute(
                "DELETE FROM heads WHERE home = ? AND branch = ? AND tracker = ?",
                params![home, branch, tracker],
            )?,
        };
        db.execute(
            "INSERT OR IGNORE INTO head_log (home, branch, tracker, old, new, moved_by, moved_at, stamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![home, branch, tracker, old, new, by, now, stamp],
        )?;
    }
    Ok(())
}

/// One home's branches' lists, and the store each of its trackers is in.
pub type HomeState = (BTreeMap<String, List>, BTreeMap<String, String>);

/// Every project's branches' lists and trackers' stores, as SQLite has them:
/// what the landing log starts from when it is made for a data directory
/// from before it.
pub fn everything(db: &Connection) -> ApiResult<BTreeMap<String, HomeState>> {
    let mut out = BTreeMap::new();
    let mut statement = db.prepare("SELECT home FROM projects ORDER BY home")?;
    let homes: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for home in homes {
        out.insert(home.clone(), home_state(db, &home)?);
    }
    Ok(out)
}

/// One home's branches' lists and trackers' stores, as SQLite has them.
pub fn home_state(db: &Connection, home: &str) -> ApiResult<HomeState> {
    let mut lists = BTreeMap::new();
    let mut statement = db.prepare("SELECT name FROM branches WHERE home = ?")?;
    let names: Vec<String> = statement
        .query_map([home], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for name in names {
        lists.insert(name.clone(), list_of(db, home, &name)?.unwrap_or_default());
    }
    Ok((lists, tracker_stores(db, home)?))
}

/// The stamp every landing up to is in place, finished included.
pub fn applied(db: &Connection) -> ApiResult<Option<u64>> {
    let stamp: Option<i64> = db
        .query_row(
            "SELECT stamp FROM landing_applied WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stamp.and_then(|stamp| u64::try_from(stamp).ok()))
}

pub fn set_applied(db: &Connection, stamp: u64) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO landing_applied VALUES (1, ?)",
        [i64::try_from(stamp).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

fn sql_stamp(stamp: u64) -> i64 {
    i64::try_from(stamp).unwrap_or(i64::MAX)
}

/// How far landing `stamp` got: `None`, not in place; `Some(false)`, in place
/// with what it moved still to remove where it was; `Some(true)`, done.
pub fn placed(db: &Connection, stamp: u64) -> ApiResult<Option<bool>> {
    if applied(db)?.is_some_and(|floor| stamp <= floor) {
        return Ok(Some(true));
    }
    let finished: Option<bool> = db
        .query_row(
            "SELECT finished FROM landings_placed WHERE stamp = ?",
            [sql_stamp(stamp)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(finished)
}

/// Landing `stamp` is in place (`finished`: and nothing of it is left to
/// do). Written in the transaction that makes its rows, so whether a landing
/// is in place and its rows can never disagree. Folds what is done into
/// `landing_applied`.
pub fn set_placed(db: &Connection, stamp: u64, finished: bool) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO landings_placed VALUES (?, ?)",
        params![sql_stamp(stamp), finished],
    )?;
    let mut floor = applied(db)?.unwrap_or(0);
    loop {
        let next = floor + 1;
        let done: Option<bool> = db
            .query_row(
                "SELECT finished FROM landings_placed WHERE stamp = ?",
                [sql_stamp(next)],
                |row| row.get(0),
            )
            .optional()?;
        if done != Some(true) {
            break;
        }
        db.execute(
            "DELETE FROM landings_placed WHERE stamp = ?",
            [sql_stamp(next)],
        )?;
        floor = next;
    }
    set_applied(db, floor)
}

/// Stamps after `landing_applied` that are in place, finished or not.
pub fn placed_after(db: &Connection) -> ApiResult<BTreeMap<u64, bool>> {
    let mut statement = db.prepare("SELECT stamp, finished FROM landings_placed")?;
    let rows = statement.query_map([], |row| {
        Ok((
            u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
            row.get::<_, bool>(1)?,
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Landing `stamp`, which touched `home`, was voided by landing `by`.
pub fn set_voided(db: &Connection, stamp: u64, by: u64, home: &str) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO landings_voided VALUES (?, ?, ?)",
        params![sql_stamp(stamp), sql_stamp(by), home],
    )?;
    Ok(())
}

/// Whether `home`'s state at `stamp`, as the log's tree has it, shows a
/// landing that was voided after it: one written down at or before `stamp`
/// and voided after it.
pub fn voided_at(db: &Connection, home: &str, stamp: u64) -> ApiResult<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM landings_voided WHERE home = ? AND stamp <= ? AND by_stamp > ?",
            params![home, sql_stamp(stamp), sql_stamp(stamp)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A seed's grant ends once its tracker is on a branch: from then on its
/// history moves only by merging.
pub fn end_seed_grants<'t>(
    db: &Connection,
    home: &str,
    trackers: impl IntoIterator<Item = &'t String>,
) -> ApiResult<()> {
    for tracker in trackers {
        db.execute(
            "DELETE FROM grants WHERE repo = ?",
            [format!("{}.t.{tracker}", home.replace('/', "."))],
        )?;
    }
    Ok(())
}

pub fn set_default_branch(db: &Connection, home: &str, branch: &str) -> ApiResult<bool> {
    if list_of(db, home, branch)?.is_none() {
        return Ok(false);
    }
    db.execute(
        "UPDATE projects SET default_branch = ? WHERE home = ?",
        [branch, home],
    )?;
    Ok(true)
}

// ── Links ─────────────────────────────────────────────────────────────────

/// Tracker → its home `(project, tracker)`, for each tracker linked here.
pub fn links(db: &Connection, home: &str) -> ApiResult<BTreeMap<String, (String, String)>> {
    let mut statement =
        db.prepare("SELECT tracker, home_project, home_tracker FROM links WHERE home = ?")?;
    let rows = statement.query_map([home], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn add_link(
    db: &Connection,
    home: &str,
    tracker: &str,
    project: &str,
    there: &str,
) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO links VALUES (?, ?, ?, ?)",
        params![home, tracker, project, there],
    )?;
    Ok(())
}

// ── Stores ────────────────────────────────────────────────────────────────

/// Tracker → the store its history is in (`crate::repos::Store::name`), for
/// each tracker of `home` that has one.
pub fn tracker_stores(db: &Connection, home: &str) -> ApiResult<BTreeMap<String, String>> {
    let mut statement = db.prepare("SELECT tracker, store FROM tracker_stores WHERE home = ?")?;
    let rows = statement.query_map([home], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn tracker_store(db: &Connection, home: &str, tracker: &str) -> ApiResult<Option<String>> {
    Ok(db
        .query_row(
            "SELECT store FROM tracker_stores WHERE home = ? AND tracker = ?",
            [home, tracker],
            |row| row.get(0),
        )
        .optional()?)
}

/// Where `tracker`'s history is: written only by applying a landing.
pub fn set_tracker_store(db: &Connection, home: &str, tracker: &str, store: &str) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO tracker_stores VALUES (?, ?, ?)",
        params![home, tracker, store],
    )?;
    Ok(())
}

// ── Instances ─────────────────────────────────────────────────────────────

fn instance_row(
    db: &Connection,
    home: &str,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Instance> {
    let name: String = row.get("name")?;
    let mut statement = db.prepare_cached(
        "SELECT tracker FROM forks WHERE home = ? AND instance = ? ORDER BY tracker",
    )?;
    let forks = statement
        .query_map([home, &name], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(Instance {
        name,
        owner: row.get("owner")?,
        agent: row.get("agent")?,
        base_rev: row.get("base_rev")?,
        head_rev: row.get("head_rev")?,
        forks,
        private: row.get::<_, i64>("private")? == 1,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub fn instance(db: &Connection, home: &str, name: &str) -> ApiResult<Option<Instance>> {
    Ok(db
        .query_row(
            "SELECT * FROM instances WHERE home = ? AND name = ?",
            [home, name],
            |row| instance_row(db, home, row),
        )
        .optional()?)
}

pub fn instances(db: &Connection, home: &str) -> ApiResult<Vec<Instance>> {
    let mut statement =
        db.prepare("SELECT * FROM instances WHERE home = ? ORDER BY created_at, name")?;
    let rows = statement.query_map([home], |row| instance_row(db, home, row))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn create_instance(
    db: &Connection,
    home: &str,
    name: &str,
    owner: &str,
    base_rev: &str,
    agent: Option<&str>,
    private: bool,
) -> ApiResult<Option<Instance>> {
    let now = now_iso();
    let inserted = db.execute(
        "INSERT OR IGNORE INTO instances (home, name, owner, agent, base_rev, private, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![home, name, owner, agent, base_rev, i64::from(private), now, now],
    )?;
    if inserted == 0 {
        return Ok(None);
    }
    instance(db, home, name)
}

pub fn set_head(
    db: &Connection,
    home: &str,
    name: &str,
    head: &str,
) -> ApiResult<Option<Instance>> {
    db.execute(
        "UPDATE instances SET head_rev = ?, updated_at = ? WHERE home = ? AND name = ?",
        params![head, now_iso(), home, name],
    )?;
    instance(db, home, name)
}

pub fn delete_instance(db: &Connection, home: &str, name: &str) -> ApiResult<bool> {
    Ok(db.execute(
        "DELETE FROM instances WHERE home = ? AND name = ?",
        [home, name],
    )? > 0)
}

pub fn add_fork(db: &Connection, home: &str, instance: &str, tracker: &str) -> ApiResult<()> {
    db.execute(
        "INSERT OR IGNORE INTO forks VALUES (?, ?, ?)",
        [home, instance, tracker],
    )?;
    Ok(())
}

pub fn has_fork(db: &Connection, home: &str, instance: &str, tracker: &str) -> ApiResult<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM forks WHERE home = ? AND instance = ? AND tracker = ?",
            [home, instance, tracker],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

// ── Checkpoints ───────────────────────────────────────────────────────────

fn checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let record: String = row.get("record")?;
    Ok(json!({
        "instance": row.get::<_, String>("instance")?,
        "id": row.get::<_, String>("id")?,
        "createdBy": row.get::<_, String>("created_by")?,
        "agent": row.get::<_, Option<String>>("agent")?,
        "createdAt": row.get::<_, String>("created_at")?,
        "record": serde_json::from_str::<Value>(&record).unwrap_or(Value::Null),
    }))
}

pub fn put_checkpoint(
    db: &Connection,
    home: &str,
    instance: &str,
    id: &str,
    by: &str,
    agent: Option<&str>,
    record: &Value,
) -> ApiResult<Value> {
    let now = now_iso();
    db.execute(
        "INSERT OR REPLACE INTO checkpoints VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![home, instance, id, by, agent, now, record.to_string()],
    )?;
    Ok(
        json!({ "instance": instance, "id": id, "createdBy": by, "agent": agent, "createdAt": now, "record": record }),
    )
}

pub fn checkpoint(
    db: &Connection,
    home: &str,
    instance: &str,
    id: &str,
) -> ApiResult<Option<Value>> {
    Ok(db
        .query_row(
            "SELECT * FROM checkpoints WHERE home = ? AND instance = ? AND id = ?",
            [home, instance, id],
            checkpoint_row,
        )
        .optional()?)
}

/// Checkpoints `viewer` may see: none of another user's private instance.
pub fn checkpoints(
    db: &Connection,
    home: &str,
    instance: Option<&str>,
    viewer: &str,
) -> ApiResult<Vec<Value>> {
    let mut statement = db.prepare(
        "SELECT c.* FROM checkpoints c JOIN instances i ON i.home = c.home AND i.name = c.instance
         WHERE c.home = ?1 AND (?2 IS NULL OR c.instance = ?2) AND (i.private = 0 OR i.owner = ?3)
         ORDER BY c.created_at, c.instance, c.id",
    )?;
    let rows = statement.query_map(params![home, instance, viewer], checkpoint_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

// ── Tags ──────────────────────────────────────────────────────────────────

fn tag_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let list: String = row.get("list")?;
    let annotation: Option<String> = row.get("annotation")?;
    Ok(json!({
        "name": row.get::<_, String>("name")?,
        "list": serde_json::from_str::<Value>(&list).unwrap_or(Value::Null),
        "annotation": annotation.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
        "createdBy": row.get::<_, String>("created_by")?,
        "createdAt": row.get::<_, String>("created_at")?,
    }))
}

pub fn tags(db: &Connection, home: &str) -> ApiResult<Vec<Value>> {
    let mut statement = db.prepare("SELECT * FROM tags WHERE home = ? ORDER BY name")?;
    let rows = statement.query_map([home], tag_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn tag(db: &Connection, home: &str, name: &str) -> ApiResult<Option<Value>> {
    Ok(db
        .query_row(
            "SELECT * FROM tags WHERE home = ? AND name = ?",
            [home, name],
            tag_row,
        )
        .optional()?)
}

/// Make a tag; `false` when one of that name exists (a tag never moves).
pub fn create_tag(
    db: &Connection,
    home: &str,
    name: &str,
    list: &List,
    annotation: &Value,
    by: &str,
) -> ApiResult<bool> {
    let annotation = (!annotation.is_null()).then(|| annotation.to_string());
    Ok(db.execute(
        "INSERT OR IGNORE INTO tags VALUES (?, ?, ?, ?, ?, ?)",
        params![
            home,
            name,
            serde_json::to_string(list).unwrap_or_default(),
            annotation,
            by,
            now_iso()
        ],
    )? > 0)
}

pub fn delete_tag(db: &Connection, home: &str, name: &str) -> ApiResult<bool> {
    Ok(db.execute("DELETE FROM tags WHERE home = ? AND name = ?", [home, name])? > 0)
}

// ── Grants ────────────────────────────────────────────────────────────────

/// Let token `token_id` push to `refs` (a prefix; `*` for any) in `repo` until
/// `expires_at`: the exceptions to the standing rules, such as the seed of a
/// new tracker's repo.
pub fn grant(
    db: &Connection,
    repo: &str,
    token_id: &str,
    refs: &str,
    expires_at: i64,
) -> ApiResult<()> {
    db.execute(
        "INSERT OR REPLACE INTO grants VALUES (?, ?, ?, ?)",
        params![repo, token_id, refs, expires_at],
    )?;
    Ok(())
}

/// The ref prefixes token `token_id` was granted in `repo`, still live.
pub fn granted(db: &Connection, repo: &str, token_id: &str) -> ApiResult<Vec<String>> {
    let mut statement =
        db.prepare("SELECT refs FROM grants WHERE repo = ? AND token_id = ? AND expires_at > ?")?;
    let rows = statement.query_map(params![repo, token_id, now_seconds()], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
