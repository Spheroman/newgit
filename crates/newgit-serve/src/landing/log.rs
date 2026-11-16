//! The log's own Git I/O: making it, counting and reading its entries,
//! writing a batch, and a branch's state at a stamp.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex};

use camino::{Utf8Path, Utf8PathBuf};

use crate::db::{self, List};
use crate::error::{ApiResult, HttpError};
use crate::repos;

use super::entry::{Change, Entry, FORMAT, Move, Stamp};
use super::queue::{Queue, ROOT_WAIT};
use super::{Log, anyhow_of};

/// Who the log's commits are by.
pub(super) const COMMITTER: &str = "newgit serve <serve@newgit.invalid>";

pub(super) fn init_log(dir: &Utf8Path) -> anyhow::Result<()> {
    let output = newgit_core::git::command()
        .args([
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            "--ref-format=reftable",
        ])
        .arg(dir.as_str())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git init {dir}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (key, value) in [
        ("core.fsync", repos::DURABLE_FSYNC),
        ("core.fsyncMethod", "fsync"),
        ("gc.auto", "0"),
    ] {
        let output = repos::git(dir, &["config", key, value])?;
        anyhow::ensure!(output.status.success(), "configuring {dir}");
    }
    Ok(())
}

/// How many landings the log at `dir` holds. Anything but an answer is an
/// error: a log that cannot be read is never taken for an empty one.
pub(super) fn count(dir: &Utf8Path) -> anyhow::Result<Stamp> {
    let output = repos::git(dir, &["rev-list", "--count", "refs/heads/main"])?;
    anyhow::ensure!(
        output.status.success(),
        "the landing log {dir} cannot be read ({}): refusing to start (restore it from a backup)",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().parse()?)
}

pub(super) fn head(dir: &Utf8Path) -> anyhow::Result<String> {
    let output = repos::git(dir, &["rev-parse", "--verify", "refs/heads/main^{commit}"])?;
    anyhow::ensure!(
        output.status.success(),
        "the landing log {dir} has no readable main: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The log's last entry, read and checked: one of a format newer than this
/// server reads refuses the start.
pub(super) fn newest_entry(dir: &Utf8Path) -> anyhow::Result<(Stamp, Entry)> {
    let output = repos::git(dir, &["log", "-1", "--format=%B", "refs/heads/main"])?;
    anyhow::ensure!(
        output.status.success(),
        "the landing log {dir} cannot be read ({}): refusing to start (restore it from a backup)",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_entry(String::from_utf8_lossy(&output.stdout).trim()).map_err(|error| {
        anyhow::anyhow!(
            "the landing log {dir}: {}: refusing to start",
            error.cause()
        )
    })
}

pub(super) fn parse_entry(message: &str) -> ApiResult<(Stamp, Entry)> {
    let (head, body) = message.split_once("\n\n").unwrap_or((message, "{}"));
    let stamp: Stamp = head
        .strip_prefix("landing ")
        .and_then(|stamp| stamp.trim().parse().ok())
        .ok_or_else(|| {
            HttpError::new(500, format!("a log commit that is not a landing: {head}"))
        })?;
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| HttpError::new(500, format!("landing {stamp}'s entry: {error}")))?;
    let format = value
        .get("format")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if format == 0 || format > u64::from(FORMAT) {
        return Err(HttpError::new(
            500,
            format!(
                "landing {stamp} is entry format {format}; this newgit serve reads format {FORMAT}: run the newgit serve that wrote it, or a newer one"
            ),
        ));
    }
    let entry: Entry = serde_json::from_value(value)
        .map_err(|error| HttpError::new(500, format!("landing {stamp}'s entry: {error}")))?;
    Ok((stamp, entry))
}

/// Write `entries` into the log at `dir` as one commit each after stamp
/// `tip`, with one `git fast-import`: the commit point of all of them.
pub(super) fn write_entries(dir: &Utf8Path, tip: Stamp, entries: &[&Entry]) -> ApiResult<()> {
    let now = chrono::Utc::now().timestamp();
    let mut stream = Vec::new();
    let blob = |stream: &mut Vec<u8>, path: &str, text: &str| {
        stream.extend_from_slice(
            format!("M 100644 inline {path}\ndata {}\n{text}\n", text.len()).as_bytes(),
        );
    };
    for (offset, entry) in entries.iter().enumerate() {
        let stamp = tip + 1 + offset as Stamp;
        let body = serde_json::to_string(entry)
            .map_err(|error| HttpError::internal(format!("an entry as JSON: {error}")))?;
        let message = format!("landing {stamp}\n\n{body}\n");
        stream.extend_from_slice(b"commit refs/heads/main\n");
        stream.extend_from_slice(format!("committer {COMMITTER} {now} +0000\n").as_bytes());
        stream.extend_from_slice(format!("data {}\n{message}\n", message.len()).as_bytes());
        if offset == 0 && tip > 0 {
            stream.extend_from_slice(b"from refs/heads/main^0\n");
        }
        for (home, change) in &entry.homes {
            if change.removed {
                stream.extend_from_slice(format!("D {home}\n").as_bytes());
                continue;
            }
            if change.whole || change.made.is_some() {
                stream.extend_from_slice(format!("D {home}\n").as_bytes());
            }
            blob(
                &mut stream,
                &format!("{home}/incarnation"),
                &change.incarnation,
            );
            for (branch, list) in &change.branches {
                stream.extend_from_slice(format!("D {home}/branches/{branch}\n").as_bytes());
                for (tracker, commit) in list {
                    stream.extend_from_slice(
                        format!("M 160000 {commit} {home}/branches/{branch}/{tracker}\n")
                            .as_bytes(),
                    );
                }
            }
            for (tracker, step) in &change.stores {
                blob(&mut stream, &format!("{home}/stores/{tracker}"), &step.to);
            }
        }
        stream.push(b'\n');
    }
    stream.extend_from_slice(b"done\n");
    let mut child = repos::git_command(dir)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&stream)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(HttpError::new(
            500,
            format!(
                "git fast-import into {dir}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(())
}

impl Log {
    /// The log at `<data>/landing.git`. Made only when it did not exist
    /// before this start: a data directory from before the log gets one
    /// whose first entry is what its SQLite says, already in place. A log
    /// that is there and cannot be read, or that ends before what SQLite
    /// says is in place, refuses to start: it is the record of every
    /// landing, and nothing here guesses past it.
    pub fn open(data: &Utf8Path, db: &db::Db) -> anyhow::Result<Self> {
        let dir = data.join("landing.git");
        let existed = dir.exists();
        let connection = db.lock();
        if !existed {
            anyhow::ensure!(
                db::applied(&connection).map_err(anyhow_of)?.is_none()
                    && db::placed_after(&connection).map_err(anyhow_of)?.is_empty(),
                "the landing log {dir} is missing, but {}'s SQLite says landings were put in place from it: refusing to start (restore landing.git from a backup)",
                data
            );
            // Made aside and renamed into place whole, so a log that exists
            // always has its first entry.
            let making = data.join("landing.git.new");
            let _ = std::fs::remove_dir_all(&making);
            init_log(&making)?;
            let mut entry = Entry {
                by: "newgit serve".to_owned(),
                ..Entry::default()
            };
            for (home, (branches, stores)) in db::everything(&connection).map_err(anyhow_of)? {
                let incarnation = db::project(&connection, &home)
                    .map_err(anyhow_of)?
                    .map(|info| info.incarnation)
                    .unwrap_or_default();
                entry.homes.insert(
                    home,
                    Change {
                        incarnation,
                        whole: true,
                        branches,
                        stores: stores
                            .into_iter()
                            .map(|(tracker, to)| (tracker, Move { from: None, to }))
                            .collect(),
                        ..Change::default()
                    },
                );
            }
            write_entries(&making, 0, &[&entry]).map_err(anyhow_of)?;
            std::fs::rename(&making, &dir)?;
            db::set_applied(&connection, 1).map_err(anyhow_of)?;
        }
        let tip = count(&dir)?;
        anyhow::ensure!(
            tip > 0,
            "the landing log {dir} has no landings: refusing to start (restore it from a backup)"
        );
        let tip_commit = head(&dir)?;
        // The newest entry's format, whether or not it is in place: a
        // newer server wrote it, and everything it wrote may rely on it.
        newest_entry(&dir)?;
        let applied = db::applied(&connection).map_err(anyhow_of)?.unwrap_or(0);
        let placed = db::placed_after(&connection).map_err(anyhow_of)?;
        let latest = placed.keys().copied().max().unwrap_or(0).max(applied);
        anyhow::ensure!(
            latest <= tip,
            "{}'s SQLite says landing {latest} is in place, but the landing log {dir} ends at {tip}: refusing to start (the log is behind what was put in place from it; restore it from a backup)",
            data
        );
        drop(connection);
        Ok(Self {
            dir,
            queue: Mutex::new(Queue {
                waiting: VecDeque::new(),
                leading: false,
                held: BTreeMap::new(),
                stuck: Vec::new(),
                tip,
                tip_commit,
                applied,
                above: BTreeSet::new(),
                tokens: 0,
            }),
            changed: Condvar::new(),
            batches: AtomicU64::new(0),
            root_wait_ms: AtomicU64::new(u64::try_from(ROOT_WAIT.as_millis()).unwrap_or(10_000)),
            faults: std::env::var("NEWGIT_SERVE_FAULT_FILE")
                .ok()
                .filter(|path| !path.is_empty())
                .map(Utf8PathBuf::from),
        })
    }

    /// Every entry after stamp `after`, oldest first.
    pub(super) fn entries_after(&self, after: Stamp, tip: Stamp) -> ApiResult<Vec<(Stamp, Entry)>> {
        if tip <= after {
            return Ok(Vec::new());
        }
        let count = (tip - after).to_string();
        let output = repos::git(
            &self.dir,
            &["log", "-n", &count, "--format=%B%x00", "refs/heads/main"],
        )?;
        if !output.status.success() {
            return Err(HttpError::new(
                500,
                format!(
                    "reading the landing log {}: {}",
                    self.dir,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        let mut entries = Vec::new();
        for message in String::from_utf8_lossy(&output.stdout).split('\0') {
            let message = message.trim();
            if message.is_empty() {
                continue;
            }
            entries.push(parse_entry(message)?);
        }
        entries.reverse();
        let expected: Vec<Stamp> = (after + 1..=tip).collect();
        let found: Vec<Stamp> = entries.iter().map(|(stamp, _)| *stamp).collect();
        if found != expected {
            return Err(HttpError::new(
                500,
                format!(
                    "the landing log {} does not number its landings {}..={tip}",
                    self.dir,
                    after + 1
                ),
            ));
        }
        Ok(entries)
    }

    /// Branch `branch` of `home` (incarnation `incarnation`) as landing
    /// `stamp` left it: `None` when it did not exist then, or the project
    /// then was another of the same name. A stamp not in place yet is
    /// refused; so is one whose state shows a landing voided since.
    pub fn list_at(
        &self,
        home: &str,
        incarnation: &str,
        branch: &str,
        stamp: Stamp,
        voided: bool,
    ) -> ApiResult<Option<List>> {
        // One commit, resolved once: the stamp's own, whatever lands next.
        let (applied, tip, tip_commit) = {
            let queue = self.lock();
            (queue.applied, queue.tip, queue.tip_commit.clone())
        };
        if stamp == 0 {
            return Ok(None);
        }
        if stamp > applied {
            return Err(HttpError::new(
                409,
                format!(
                    "landing {stamp} is not in place yet (every landing up to {applied} is): ask again"
                ),
            ));
        }
        if voided {
            return Err(HttpError::new(
                410,
                format!(
                    "{home}'s state at landing {stamp} shows a landing that was voided after it"
                ),
            ));
        }
        let at = format!("{tip_commit}~{}", tip - stamp);
        let path = format!("{home}/branches/{branch}/");
        let marker = format!("{home}/incarnation");
        let output = repos::git(&self.dir, &["ls-tree", "-r", &at, "--", &path, &marker])?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "reading landing {stamp}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let mut list = List::new();
        let mut then = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((meta, name)) = line.split_once('\t') else {
                continue;
            };
            let object = meta.split_whitespace().nth(2).unwrap_or_default();
            if name == marker {
                then = repos::git(&self.dir, &["cat-file", "blob", object])
                    .ok()
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
                continue;
            }
            if let Some(tracker) = name.strip_prefix(&path) {
                list.insert(tracker.to_owned(), object.to_owned());
            }
        }
        if then.as_deref() != Some(incarnation) {
            return Ok(None);
        }
        Ok((!list.is_empty()).then_some(list))
    }
}
