//! The landing log: the commit point of every landing on this server
//! (docs/design/landing.md, *Landing*, items 3 and 4).
//!
//! A landing is anything that changes what readers resolve a tracker to: a
//! merge's finish, a branch made, a tracker made or moved between stores, a
//! project deleted. Each is one [`Entry`], written as one commit to a hidden
//! Git repository, `<data>/landing.git`, and its place in that history (1 for
//! the first commit, 2 for the next) is its **stamp**. The commit is the
//! commit point: once it is written the landing happened, and everything
//! else follows it.
//!
//! - **The log's tree is the state.** Each commit's tree names every
//!   branch's list as gitlinks (`<owner>/<project>/branches/<branch>/<tracker>`)
//!   and the store each tracker is in (`<owner>/<project>/stores/<tracker>`),
//!   so "state at stamp T" is the tree of commit T, never a commit date. Its
//!   message holds the entry itself: what to put in place.
//! - **Repo refs and SQLite follow it.** [`apply`] puts an entry in place:
//!   a tracker moving between stores is copied first, then every ref it moves
//!   in each store in one transaction and every row in one SQLite
//!   transaction, together under the db lock, so a reader sees all of a
//!   landing or none of it; then what moved is removed where it was. SQLite's
//!   `landing_applied` says how far that got, and at startup every entry
//!   after it is applied again ([`Log::replay`]), so a crash between the log
//!   and the refs is repaired before anything is served.
//! - **Readers resolve heads through it.** Lists and stores are read from
//!   SQLite's rows, which change only by applying an entry, all at once.
//! - **Group commit.** Landings that arrive together are written by one
//!   `git fast-import`, each its own commit, and put in place together. The
//!   first to arrive leads: it takes everything waiting, checks each in order
//!   against what is there and what the batch lands before it, writes them,
//!   and puts them in place, while the next ones queue behind it.
//! - **Roots' locks.** Only one batch is checked and written at a time, and a
//!   landing whose roots (projects) are held waits; it takes every root it
//!   touches at once, so a tracker linked into several roots stays atomic
//!   with all of them.
//! - **A landing that moves storage holds its roots, not the log.** Copying a
//!   tracker into another store can be large, so it is done by the landing's
//!   own thread after the batch is written, holding only its roots: other
//!   projects land meanwhile.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::App;
use crate::db::{self, List};
use crate::error::{ApiResult, HttpError};
use crate::names::parse_project_key;
use crate::repos::{self, Repo, Store};

/// A landing's place in the log: 1 for the first, one more for each after.
pub type Stamp = u64;

/// The most landings one batch writes.
const BATCH: usize = 512;
/// How long a move waits for pushes that began where a tracker was.
const PUSHES_DRAIN: Duration = Duration::from_secs(10 * 60);
/// Who the log's commits are by.
const COMMITTER: &str = "newgit serve <serve@newgit.invalid>";

/// One landing, as the log keeps it: everything to put in place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Who landed it; a merge's objects wait in their landing.
    pub by: String,
    /// What it changes, by home (`<owner>/<project>`): the roots it touches.
    #[serde(default)]
    pub homes: BTreeMap<String, Change>,
}

/// What one landing changes in one home.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Branches whose lists it sets, whole; made if they are not yet.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, List>,
    /// Trackers whose history it puts in another store, or makes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<String, Move>,
    /// Tracker refs it moves, as the tracker's readers name them: to a
    /// commit, or `None` to delete.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refs: BTreeMap<String, BTreeMap<String, Option<String>>>,
    /// Commits waiting in the lander's landing (`Repo::landing_dir`) that go
    /// into a moving tracker's new store once this is written down, and not
    /// before: for a narrowing, or a move between audiences not ordered by
    /// inclusion, no store may hold them until the landing happened.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub takes: BTreeMap<String, Vec<String>>,
    /// The project is deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// A tracker's history put in store `to`: moved from `from`, or made there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub from: Option<String>,
    pub to: String,
}

impl Entry {
    fn roots(&self) -> BTreeSet<String> {
        self.homes.keys().cloned().collect()
    }

    /// Whether putting it in place copies or removes storage, which can be
    /// large: done by its own thread, holding its roots, not the log.
    fn moves_storage(&self) -> bool {
        self.homes
            .values()
            .any(|change| change.removed || change.stores.values().any(|step| step.from.is_some()))
    }
}

/// A landing asked for: its entry, and what must still be true when it is
/// checked under the lock (the recheck of landing.md's item 3).
#[derive(Debug, Default)]
pub struct Landing {
    pub entry: Entry,
    /// Each branch's list as it must be: `None`, a branch this makes.
    pub expect: Vec<Expect>,
    /// Where each tracker it writes into must still be (`None`: no row).
    pub placed: Vec<Placed>,
    /// Refs moved only if each is still at `old` when it lands; one that
    /// moved meanwhile is said ([`Landed::behind`]) and the landing stands.
    pub asked: Vec<Asked>,
}

#[derive(Debug, Clone)]
pub struct Expect {
    pub home: String,
    pub branch: String,
    pub list: Option<List>,
}

#[derive(Debug, Clone)]
pub struct Placed {
    pub home: String,
    pub tracker: String,
    pub store: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Asked {
    pub home: String,
    pub tracker: String,
    /// As the tracker's readers name it: `refs/heads/main`.
    pub reference: String,
    pub old: Option<String>,
    pub new: String,
    /// Where the ref is now.
    pub repo: Repo,
}

/// A landing written down and in place.
#[derive(Debug)]
pub struct Landed {
    pub stamp: Stamp,
    /// Asked refs that had moved meanwhile, and so were not moved.
    pub behind: Vec<Asked>,
}

/// What a batch's leader decided for one landing.
enum Outcome {
    Landed(Landed),
    /// Written down; its own thread puts it in place, holding its roots.
    Apply(Stamp, Entry, Landed),
    /// Its roots have landings written down and not in place: its thread
    /// tries them again, holding those roots, then asks again.
    Retry(Vec<(Stamp, Entry)>),
}

struct Slot {
    roots: BTreeSet<String>,
    moves_storage: bool,
    landing: Mutex<Option<Landing>>,
    outcome: Mutex<Option<ApiResult<Outcome>>>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Arc<Slot>>,
    leading: bool,
    /// Roots whose landing is being put in place off the leader.
    held: BTreeSet<String>,
    /// Landings written down whose putting in place failed, oldest first.
    stuck: Vec<(Stamp, Entry)>,
    /// The last stamp written.
    tip: Stamp,
    /// Every landing up to here is in place.
    applied: Stamp,
    /// Landings in place after `applied`, with one before them not yet.
    above: BTreeSet<Stamp>,
}

impl Queue {
    fn stuck_roots(&self) -> BTreeSet<String> {
        self.stuck
            .iter()
            .flat_map(|(_, entry)| entry.homes.keys().cloned())
            .collect()
    }

    /// `stamp` is in place; returns the new low-water mark if it moved.
    fn mark(&mut self, stamp: Stamp) -> Option<Stamp> {
        self.stuck.retain(|(stuck, _)| *stuck != stamp);
        if stamp <= self.applied {
            return None;
        }
        self.above.insert(stamp);
        let before = self.applied;
        while self.above.remove(&(self.applied + 1)) {
            self.applied += 1;
        }
        (self.applied != before).then_some(self.applied)
    }
}

pub struct Log {
    dir: Utf8PathBuf,
    queue: Mutex<Queue>,
    changed: Condvar,
    batches: AtomicU64,
    /// `NEWGIT_SERVE_FAULT_FILE`: while the file it names says
    /// `before-log-commit` or `after-log-commit`, the next batch stops the
    /// process there, as a kill would. For the crash tests.
    faults: Option<Utf8PathBuf>,
}

impl Log {
    /// The log at `<data>/landing.git`, made if it is not yet. A data
    /// directory from before the log gets one whose first entry is what its
    /// SQLite says, already in place.
    pub fn open(data: &Utf8Path, db: &db::Db) -> anyhow::Result<Self> {
        let dir = data.join("landing.git");
        if !repos::store_exists(&dir) {
            let output = Command::new("git")
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
                ("core.fsync", "committed"),
                ("core.fsyncMethod", "fsync"),
                ("gc.auto", "0"),
            ] {
                let output = repos::git(&dir, &["config", key, value])?;
                anyhow::ensure!(output.status.success(), "configuring {dir}");
            }
        }
        let log = Self {
            dir,
            queue: Mutex::default(),
            changed: Condvar::new(),
            batches: AtomicU64::new(0),
            faults: std::env::var("NEWGIT_SERVE_FAULT_FILE")
                .ok()
                .filter(|path| !path.is_empty())
                .map(Utf8PathBuf::from),
        };
        let mut tip = log.count()?;
        let connection = db.lock();
        if tip == 0 {
            // From before the log: what SQLite says is the first entry.
            let mut entry = Entry {
                by: "newgit serve".to_owned(),
                homes: BTreeMap::new(),
            };
            for (home, (branches, stores)) in db::everything(&connection).map_err(anyhow_of)? {
                entry.homes.insert(
                    home,
                    Change {
                        branches,
                        stores: stores
                            .into_iter()
                            .map(|(tracker, to)| (tracker, Move { from: None, to }))
                            .collect(),
                        ..Change::default()
                    },
                );
            }
            log.write(0, &[&entry]).map_err(anyhow_of)?;
            db::set_applied(&connection, 1).map_err(anyhow_of)?;
            tip = 1;
        }
        let applied = db::applied(&connection)
            .map_err(anyhow_of)?
            .unwrap_or(0)
            .min(tip);
        drop(connection);
        {
            let mut queue = log.lock();
            queue.tip = tip;
            queue.applied = applied;
        }
        Ok(log)
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The last stamp written.
    pub fn tip(&self) -> Stamp {
        self.lock().tip
    }

    /// How many batches have been written: fewer than the landings in them
    /// when landings arrived together.
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::SeqCst)
    }

    /// Stop the process here if the fault file says to.
    fn fault(&self, at: &str) {
        let Some(path) = &self.faults else {
            return;
        };
        if std::fs::read_to_string(path).is_ok_and(|fault| fault.trim() == at) {
            eprintln!("newgit serve: stopping at {at} ({path})");
            std::process::abort();
        }
    }

    fn count(&self) -> anyhow::Result<Stamp> {
        let output = repos::git(&self.dir, &["rev-list", "--count", "refs/heads/main"])?;
        if !output.status.success() {
            return Ok(0);
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().parse()?)
    }

    /// Land `landing`: checked against what is there under its roots'
    /// locks, written down, and in place before this returns. Never called
    /// while this thread holds the db lock, which putting it in place takes.
    pub fn land(&self, app: &App, landing: Landing) -> ApiResult<Landed> {
        if db::held_here() {
            return Err(HttpError::internal(
                "a landing was waited on with the db lock held",
            ));
        }
        let slot = Arc::new(Slot {
            roots: landing.entry.roots(),
            moves_storage: landing.entry.moves_storage(),
            landing: Mutex::new(Some(landing)),
            outcome: Mutex::new(None),
        });
        let mut queue = self.lock();
        queue.waiting.push_back(slot.clone());
        loop {
            let outcome = lock(&slot.outcome).take();
            if let Some(outcome) = outcome {
                drop(queue);
                match outcome? {
                    Outcome::Landed(landed) => return Ok(landed),
                    Outcome::Apply(stamp, entry, landed) => {
                        let applied = apply(app, stamp, &entry);
                        self.finished(app, &slot.roots, &[(stamp, entry)], applied.is_ok());
                        applied.map_err(|error| not_in_place(stamp, &error))?;
                        return Ok(landed);
                    }
                    Outcome::Retry(entries) => {
                        let mut roots = slot.roots.clone();
                        for (_, entry) in &entries {
                            roots.extend(entry.roots());
                        }
                        let mut done = Vec::new();
                        let mut failed = None;
                        for (stamp, entry) in &entries {
                            match apply(app, *stamp, entry) {
                                Ok(()) => done.push((*stamp, entry.clone())),
                                Err(error) => {
                                    failed = Some((*stamp, error));
                                    break;
                                }
                            }
                        }
                        self.finished(app, &roots, &done, true);
                        if let Some((stamp, error)) = failed {
                            return Err(HttpError::new(
                                503,
                                format!(
                                    "an earlier landing here (stamp {stamp}) is written down but not yet in place ({}); nothing new lands until it is, so try again",
                                    error.message
                                ),
                            ));
                        }
                        queue = self.lock();
                        queue.waiting.push_back(slot.clone());
                        continue;
                    }
                }
            }
            // Lead when nobody is, and this landing's roots are free; wait
            // otherwise, for a batch or a root to finish.
            if queue.leading || !slot.roots.is_disjoint(&queue.held) {
                queue = self
                    .changed
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            queue = self.lead(app, queue);
        }
    }

    /// Take everything waiting that may land now, and land it as one batch.
    fn lead<'q>(&'q self, app: &App, mut queue: MutexGuard<'q, Queue>) -> MutexGuard<'q, Queue> {
        queue.leading = true;
        let stuck_roots = queue.stuck_roots();
        let mut batch = Vec::new();
        let mut later = VecDeque::new();
        // Roots a landing in this batch moves storage in: nothing after it
        // there lands before it is in place.
        let mut taken = BTreeSet::new();
        let mut hold = BTreeSet::new();
        while let Some(slot) = queue.waiting.pop_front() {
            if batch.len() >= BATCH
                || !slot.roots.is_disjoint(&queue.held)
                || !slot.roots.is_disjoint(&taken)
                || !slot.roots.is_disjoint(&hold)
            {
                later.push_back(slot);
                continue;
            }
            if !slot.roots.is_disjoint(&stuck_roots) {
                let entries: Vec<(Stamp, Entry)> = queue
                    .stuck
                    .iter()
                    .filter(|(_, entry)| !entry.roots().is_disjoint(&slot.roots))
                    .cloned()
                    .collect();
                hold.extend(slot.roots.iter().cloned());
                for (_, entry) in &entries {
                    hold.extend(entry.roots());
                }
                *lock(&slot.outcome) = Some(Ok(Outcome::Retry(entries)));
                continue;
            }
            if slot.moves_storage {
                taken.extend(slot.roots.iter().cloned());
            }
            batch.push(slot);
        }
        queue.waiting = later;
        queue.held.extend(hold);
        let tip = queue.tip;
        drop(queue);

        let (outcomes, written, marks) = self.process(app, tip, &batch);

        let mut queue = self.lock();
        queue.tip += written;
        let mut low = None;
        for (stamp, ok, entry) in marks {
            if ok {
                low = queue.mark(stamp).or(low);
            } else {
                queue.stuck.push((stamp, entry));
            }
        }
        for (slot, outcome) in batch.iter().zip(outcomes) {
            if let Ok(Outcome::Apply(..)) = &outcome {
                queue.held.extend(slot.roots.iter().cloned());
            }
            *lock(&slot.outcome) = Some(outcome);
        }
        queue.leading = false;
        self.changed.notify_all();
        drop(queue);
        if let Some(low) = low {
            persist_applied(app, low);
        }
        self.lock()
    }

    /// Check, write, and put in place one batch. Returns each landing's
    /// outcome, how many were written, and each written one put in place
    /// here (stamp, whether it is, the entry).
    #[allow(clippy::type_complexity, reason = "one batch's three answers")]
    fn process(
        &self,
        app: &App,
        tip: Stamp,
        batch: &[Arc<Slot>],
    ) -> (Vec<ApiResult<Outcome>>, Stamp, Vec<(Stamp, bool, Entry)>) {
        let mut outcomes: Vec<Option<ApiResult<Outcome>>> = Vec::new();
        let mut accepted: Vec<(usize, Entry, Vec<Asked>)> = Vec::new();
        {
            let db = app.db.lock();
            let mut lists: BTreeMap<(String, String), Option<List>> = BTreeMap::new();
            let mut stores: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
            for (index, slot) in batch.iter().enumerate() {
                let Some(landing) = lock(&slot.landing).take() else {
                    outcomes.push(Some(Err(HttpError::internal("a landing asked twice"))));
                    continue;
                };
                match check(&db, landing, &mut lists, &mut stores) {
                    Ok((entry, behind)) => {
                        outcomes.push(None);
                        accepted.push((index, entry, behind));
                    }
                    Err(error) => outcomes.push(Some(Err(error))),
                }
            }
        }
        let mut marks = Vec::new();
        if accepted.is_empty() {
            return (outcomes.into_iter().map(answered).collect(), 0, marks);
        }
        self.fault("before-log-commit");
        let entries: Vec<&Entry> = accepted.iter().map(|(_, entry, _)| entry).collect();
        if let Err(error) = self.write(tip, &entries) {
            for (index, _, _) in &accepted {
                outcomes[*index] = Some(Err(HttpError::internal(format!(
                    "writing the landing log: {}",
                    error.message
                ))));
            }
            return (outcomes.into_iter().map(answered).collect(), 0, marks);
        }
        self.batches.fetch_add(1, Ordering::SeqCst);
        self.fault("after-log-commit");
        let written = accepted.len() as Stamp;
        // Everything that moves no storage, put in place together: one ref
        // transaction per store and one SQLite transaction for the batch.
        let mut here = Vec::new();
        for (offset, (index, entry, behind)) in accepted.into_iter().enumerate() {
            let stamp = tip + 1 + offset as Stamp;
            let landed = Landed { stamp, behind };
            if batch[index].moves_storage {
                outcomes[index] = Some(Ok(Outcome::Apply(stamp, entry, landed)));
            } else {
                here.push((index, stamp, entry, landed));
            }
        }
        let together: Vec<(Stamp, &Entry)> = here
            .iter()
            .map(|(_, stamp, entry, _)| (*stamp, entry))
            .collect();
        let applied = apply_all(app, &together);
        for (index, stamp, entry, landed) in here {
            match &applied {
                Ok(()) => {
                    marks.push((stamp, true, entry));
                    outcomes[index] = Some(Ok(Outcome::Landed(landed)));
                }
                Err(error) => {
                    outcomes[index] = Some(Err(not_in_place(stamp, error)));
                    marks.push((stamp, false, entry));
                }
            }
        }
        (outcomes.into_iter().map(answered).collect(), written, marks)
    }

    /// Landings put in place (or not) by a landing's own thread, which held
    /// `roots`: let go of them.
    fn finished(&self, app: &App, roots: &BTreeSet<String>, entries: &[(Stamp, Entry)], ok: bool) {
        let mut queue = self.lock();
        let mut low = None;
        for (stamp, entry) in entries {
            if ok {
                low = queue.mark(*stamp).or(low);
            } else if !queue.stuck.iter().any(|(stuck, _)| stuck == stamp) {
                queue.stuck.push((*stamp, entry.clone()));
            }
        }
        for root in roots {
            queue.held.remove(root);
        }
        self.changed.notify_all();
        drop(queue);
        if let Some(low) = low {
            persist_applied(app, low);
        }
    }

    /// Write `entries` as one commit each after stamp `tip`, with one `git
    /// fast-import`: the commit point of all of them.
    fn write(&self, tip: Stamp, entries: &[&Entry]) -> ApiResult<()> {
        let now = chrono::Utc::now().timestamp();
        let mut stream = Vec::new();
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
                    stream.extend_from_slice(
                        format!(
                            "M 100644 inline {home}/stores/{tracker}\ndata {}\n{}\n",
                            step.to.len(),
                            step.to
                        )
                        .as_bytes(),
                    );
                }
            }
            stream.push(b'\n');
        }
        stream.extend_from_slice(b"done\n");
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(self.dir.as_str())
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
            return Err(HttpError::internal(format!(
                "git fast-import into {}: {}",
                self.dir,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Every entry after stamp `after`, oldest first.
    fn entries_after(&self, after: Stamp, tip: Stamp) -> ApiResult<Vec<(Stamp, Entry)>> {
        if tip <= after {
            return Ok(Vec::new());
        }
        let count = (tip - after).to_string();
        let output = repos::git(
            &self.dir,
            &["log", "-n", &count, "--format=%B%x00", "refs/heads/main"],
        )?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "reading {}: {}",
                self.dir,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let mut entries = Vec::new();
        for message in String::from_utf8_lossy(&output.stdout).split('\0') {
            let message = message.trim();
            if message.is_empty() {
                continue;
            }
            let (head, body) = message.split_once("\n\n").unwrap_or((message, "{}"));
            let stamp: Stamp = head
                .strip_prefix("landing ")
                .and_then(|stamp| stamp.trim().parse().ok())
                .ok_or_else(|| {
                    HttpError::internal(format!("a log commit that is not a landing: {head}"))
                })?;
            let entry: Entry = serde_json::from_str(body).map_err(|error| {
                HttpError::internal(format!("landing {stamp}'s entry: {error}"))
            })?;
            entries.push((stamp, entry));
        }
        entries.reverse();
        Ok(entries)
    }

    /// Put in place every landing written down and not yet in place: run at
    /// startup, before anything is served. One that fails is said, and its
    /// roots refuse new landings (503) until it is put in place, which each
    /// landing asking there tries again.
    pub fn replay(&self, app: &App) -> ApiResult<()> {
        let (applied, tip) = {
            let queue = self.lock();
            (queue.applied, queue.tip)
        };
        let entries = self.entries_after(applied, tip)?;
        let mut stuck_roots = BTreeSet::new();
        for (stamp, entry) in entries {
            let ok = if entry.roots().is_disjoint(&stuck_roots) {
                match apply(app, stamp, &entry) {
                    Ok(()) => true,
                    Err(error) => {
                        eprintln!(
                            "newgit serve: landing {stamp} is written down and could not be put in place: {}",
                            error.message
                        );
                        false
                    }
                }
            } else {
                false
            };
            if !ok {
                stuck_roots.extend(entry.roots());
            }
            self.finished(app, &BTreeSet::new(), &[(stamp, entry)], ok);
        }
        Ok(())
    }

    /// Branch `branch` of `home` as landing `stamp` left it: `None` when it
    /// did not exist then, or no landing has that stamp.
    pub fn list_at(&self, home: &str, branch: &str, stamp: Stamp) -> ApiResult<Option<List>> {
        let tip = self.tip();
        if stamp == 0 || stamp > tip {
            return Ok(None);
        }
        let at = format!("refs/heads/main~{}", tip - stamp);
        let path = format!("{home}/branches/{branch}/");
        let output = repos::git(&self.dir, &["ls-tree", "-r", &at, "--", &path])?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "reading landing {stamp}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let mut list = List::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((meta, name)) = line.split_once('\t') else {
                continue;
            };
            let Some(tracker) = name.strip_prefix(&path) else {
                continue;
            };
            if let Some(commit) = meta.split_whitespace().nth(2) {
                list.insert(tracker.to_owned(), commit.to_owned());
            }
        }
        Ok((!list.is_empty()).then_some(list))
    }
}

/// The checks under the lock: every branch still as the landing expects,
/// every tracker still where it expects, each asked ref still at its old
/// commit (one moved meanwhile is left, and said). What this batch lands
/// before it counts.
fn check(
    db: &rusqlite::Connection,
    landing: Landing,
    lists: &mut BTreeMap<(String, String), Option<List>>,
    stores: &mut BTreeMap<(String, String), Option<String>>,
) -> ApiResult<(Entry, Vec<Asked>)> {
    let Landing {
        mut entry,
        expect,
        placed,
        asked,
    } = landing;
    for want in &expect {
        let key = (want.home.clone(), want.branch.clone());
        let now = match lists.get(&key) {
            Some(list) => list.clone(),
            None => db::list_of(db, &want.home, &want.branch)?,
        };
        if now != want.list {
            return Err(match (&want.list, now) {
                (None, Some(_)) => HttpError::new(
                    409,
                    format!(
                        "branch `{}` exists; a branch moves only through `newgit merge`",
                        want.branch
                    ),
                ),
                _ => HttpError::new(
                    409,
                    format!(
                        "`{}` moved since you looked: merge again onto what it is now",
                        want.branch
                    ),
                ),
            });
        }
    }
    for want in &placed {
        let key = (want.home.clone(), want.tracker.clone());
        let now = match stores.get(&key) {
            Some(store) => store.clone(),
            None => db::tracker_store(db, &want.home, &want.tracker)?,
        };
        if now != want.store {
            return Err(HttpError::new(
                409,
                format!(
                    "`{}` moved to another store since you looked: merge again",
                    want.tracker
                ),
            ));
        }
    }
    let mut behind = Vec::new();
    for step in asked {
        let now = step.repo.branch_head_at(&step.reference);
        if now.as_deref() == Some(step.new.as_str()) {
            continue;
        }
        if now != step.old {
            behind.push(step);
            continue;
        }
        entry
            .homes
            .entry(step.home.clone())
            .or_default()
            .refs
            .entry(step.tracker.clone())
            .or_default()
            .insert(step.reference.clone(), Some(step.new.clone()));
    }
    for (home, change) in &entry.homes {
        for (branch, list) in &change.branches {
            lists.insert((home.clone(), branch.clone()), Some(list.clone()));
        }
        for (tracker, step) in &change.stores {
            stores.insert((home.clone(), tracker.clone()), Some(step.to.clone()));
        }
    }
    Ok((entry, behind))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn persist_applied(app: &App, stamp: Stamp) {
    let db = app.db.lock();
    if let Err(error) = db::set_applied(&db, stamp) {
        eprintln!(
            "newgit serve: noting landing {stamp} in place: {}",
            error.message
        );
    }
}

fn not_in_place(stamp: Stamp, error: &HttpError) -> HttpError {
    HttpError::new(
        500,
        format!(
            "landed (stamp {stamp}), but not yet in place ({}): nothing on this server shows it until it is; it is tried again at the next landing here and at startup",
            error.message
        ),
    )
}

fn anyhow_of(error: HttpError) -> anyhow::Error {
    anyhow::anyhow!("{}", error.message)
}

fn answered(outcome: Option<ApiResult<Outcome>>) -> ApiResult<Outcome> {
    outcome.unwrap_or_else(|| Err(HttpError::internal("a landing left unanswered")))
}

fn store_of(name: &str) -> ApiResult<Store> {
    Store::parse(name).ok_or_else(|| HttpError::internal(format!("no store `{name}`")))
}

/// Put landing `stamp` in place: after its commit in the log, and run again
/// at startup until it is (each step is safe to run twice).
///
/// 1. Each tracker it moves between stores is copied into its new store,
///    and the commits it takes from the lander's landing follow, resolving
///    against where the tracker was. Nothing says it is there yet.
/// 2. Every ref it moves, one transaction per store, and every row it
///    changes, one SQLite transaction, together under the db lock: readers
///    see all of it or none of it.
/// 3. What moved is removed where it was, once every push that began there
///    (before readers moved) is done; a deleted project's home goes.
pub fn apply(app: &App, stamp: Stamp, entry: &Entry) -> ApiResult<()> {
    apply_all(app, &[(stamp, entry)])
}

/// [`apply`] for several landings in stamp order, put in place together:
/// each ref takes the last value any of them gives it, in one transaction
/// per store, and every row in one SQLite transaction. A batch's leader
/// calls this for every landing in it that moves no storage.
fn apply_all(app: &App, entries: &[(Stamp, &Entry)]) -> ApiResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let data = &app.config.data;
    let home_dir = |home: &str| -> ApiResult<Utf8PathBuf> {
        parse_project_key(home)
            .map(|id| id.dir(data))
            .ok_or_else(|| HttpError::internal(format!("`{home}` is not a project")))
    };
    let mut moving = Vec::new();
    for (_, entry) in entries {
        for (home, change) in &entry.homes {
            if change.removed {
                continue;
            }
            for (tracker, step) in &change.stores {
                let Some(from) = &step.from else {
                    // Made there: the repo was made before it landed.
                    continue;
                };
                let to = store_of(&step.to)?;
                let repo = Repo::tracker(&home_dir(home)?, Some(store_of(from)?), tracker);
                let copied = repo.copy_to(&to)?;
                let borrow = if repos::store_exists(&repo.dir()) {
                    repo.clone()
                } else {
                    copied.to.clone()
                };
                for commit in change.takes.get(tracker).into_iter().flatten() {
                    copied.to.take_landing(&entry.by, commit, &borrow)?;
                }
                moving.push((home.clone(), tracker.clone(), repo, copied));
            }
        }
    }
    {
        let db = app.db.lock();
        // Store, then full ref name, then where it ends up.
        let mut refs: BTreeMap<Utf8PathBuf, BTreeMap<String, Option<String>>> = BTreeMap::new();
        // Where each tracker is as the landings before leave it.
        let mut stores: BTreeMap<(String, String), Option<Store>> = BTreeMap::new();
        for (_, entry) in entries {
            for (home, change) in &entry.homes {
                if change.removed {
                    continue;
                }
                for (tracker, step) in &change.stores {
                    stores.insert((home.clone(), tracker.clone()), Some(store_of(&step.to)?));
                }
                for (tracker, moved) in &change.refs {
                    let key = (home.clone(), tracker.clone());
                    let store = match stores.get(&key) {
                        Some(store) => store.clone(),
                        None => db::tracker_store(&db, home, tracker)?
                            .and_then(|name| Store::parse(&name)),
                    };
                    let repo = Repo::tracker(&home_dir(home)?, store, tracker);
                    let at = refs.entry(repo.dir()).or_default();
                    for (reference, new) in moved {
                        at.insert(repo.full(reference), new.clone());
                    }
                }
            }
        }
        for (dir, moved) in &refs {
            let lines: Vec<String> = moved
                .iter()
                .map(|(reference, new)| match new {
                    Some(new) => format!("update {reference} {new}"),
                    None => format!("delete {reference}"),
                })
                .collect();
            repos::transaction(dir, &lines)?;
        }
        let transaction = db.unchecked_transaction()?;
        for (stamp, entry) in entries {
            for (home, change) in &entry.homes {
                if change.removed {
                    db::delete_project(&transaction, home)?;
                    continue;
                }
                for (branch, list) in &change.branches {
                    db::set_branch(&transaction, home, branch, list, &entry.by, *stamp)?;
                }
                for (tracker, step) in &change.stores {
                    db::set_tracker_store(&transaction, home, tracker, &step.to)?;
                }
            }
        }
        transaction.commit()?;
        app.trackers_moved();
    }
    for (home, tracker, repo, copied) in &moving {
        app.pushes
            .drain(home, tracker, &repo.store.name(), PUSHES_DRAIN);
        repo.finish_move(copied)?;
    }
    for (_, entry) in entries {
        for (home, change) in &entry.homes {
            if change.removed {
                repos::delete(&home_dir(home)?)?;
            }
        }
    }
    Ok(())
}

/// Pushes running into each tracker's namespace in a store: a move waits
/// for those that began where the tracker was before it removes it there.
#[derive(Default)]
pub struct Pushes {
    running: Mutex<BTreeMap<(String, String, String), usize>>,
    done: Condvar,
}

/// One push, counted until it is dropped.
pub struct Pushing {
    app: Arc<App>,
    key: (String, String, String),
}

impl Drop for Pushing {
    fn drop(&mut self) {
        let pushes = &self.app.pushes;
        let mut running = lock(&pushes.running);
        if let Some(count) = running.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                running.remove(&self.key);
            }
        }
        pushes.done.notify_all();
    }
}

impl Pushes {
    /// Count a push into `tracker` of `home` in `store`. Taken under the db
    /// lock, where it was decided which store the push goes to, so a move
    /// either sees it or sent it to the new store.
    pub fn start(app: &Arc<App>, home: &str, tracker: &str, store: &str) -> Pushing {
        let key = (home.to_owned(), tracker.to_owned(), store.to_owned());
        *lock(&app.pushes.running).entry(key.clone()).or_default() += 1;
        Pushing {
            app: app.clone(),
            key,
        }
    }

    /// Wait, at most `limit`, for every push into `tracker` of `home` in
    /// `store` to end.
    pub fn drain(&self, home: &str, tracker: &str, store: &str, limit: Duration) {
        let key = (home.to_owned(), tracker.to_owned(), store.to_owned());
        let running = lock(&self.running);
        let (_running, waited) = self
            .done
            .wait_timeout_while(running, limit, |running| running.contains_key(&key))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waited.timed_out() {
            eprintln!(
                "newgit serve: a push into {tracker} of {home} in {store} ran past {} s while it moved; what it leaves there stays",
                limit.as_secs()
            );
        }
    }
}
