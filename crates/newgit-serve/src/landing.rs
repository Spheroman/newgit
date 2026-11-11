//! The landing log: the commit point of every landing on this server
//! (docs/design/landing.md, *Landing*, items 3 and 4).
//!
//! A landing is anything that changes what readers resolve a tracker to: a
//! project made or deleted, a merge's finish, a branch made, a tracker made
//! or moved between stores. Each is one [`Entry`], written as one commit to a
//! hidden Git repository, `<data>/landing.git`, and its place in that history
//! (1 for the first commit, 2 for the next) is its **stamp**. The commit is
//! the commit point: once it is written the landing happened, and everything
//! else follows it.
//!
//! - **Nothing that cannot be put in place is written down.** The check under
//!   the lock ([`check`]) verifies everything [`apply`] relies on: the
//!   project is the incarnation the landing was written for, every branch and
//!   store is as it expects, every ref it moves is where it says (its `old`,
//!   recorded in the entry), and every commit a ref moves to is in the store
//!   it lands in.
//! - **The log's tree is the state.** Each commit's tree names every
//!   branch's list as gitlinks (`<owner>/<project>/branches/<branch>/<tracker>`),
//!   the store each tracker is in (`<owner>/<project>/stores/<tracker>`), and
//!   the project's incarnation (`<owner>/<project>/incarnation`), so "state at
//!   stamp T" is the tree of commit T, never a commit date. Its message holds
//!   the entry itself: what to put in place.
//! - **Repo refs and SQLite follow it.** [`apply`] puts entries in place: a
//!   tracker moving between stores is copied first; then every store's ref
//!   transaction is prepared, and only once all are, committed; then every
//!   row, with the stamp marked in place, in one SQLite transaction; then what
//!   moved is removed where it was, and the stamp marked finished. Refs are
//!   synced before the rows are written, and the rows synced before anything
//!   is answered, so a stamp marked in place is in place, after power loss
//!   too. At startup every entry not marked is put in place again
//!   ([`Log::replay`]) before anything is served, and never one that is.
//! - **Readers resolve heads through it.** Lists and stores are read from
//!   SQLite's rows, which change only by applying an entry, all at once.
//! - **Group commit.** Landings that arrive together are written by one
//!   `git fast-import`, each its own commit, and put in place together; when
//!   that fails they are put in place one by one, so only the one at fault
//!   is stuck. The first to arrive leads: it takes everything waiting, checks
//!   each in order against what is there and what the batch lands before it,
//!   writes them, and puts them in place, while the next ones queue.
//! - **Roots' locks.** A landing names every root (project) it touches in its
//!   entry before it waits, and takes them all at once, so a tracker linked
//!   into several roots stays atomic with all of them. [`Log::hold`] takes
//!   them ahead of a landing, for work that must not race one (a merge
//!   placing its commits). A landing that waits too long for a root is
//!   answered 503, with what holds it.
//! - **A landing that moves storage holds its roots, not the log.** Copying a
//!   tracker into another store can be large, so it is done by the landing's
//!   own thread after the batch is written, holding only its roots.
//! - **A landing that is written down and cannot be put in place is stuck.**
//!   Its roots refuse new landings with a 503 that says why, every landing
//!   asking there tries it again first, and so does startup. An operator who
//!   cannot fix the cause voids it ([`Log::void`], `newgit serve
//!   --void-landing`): a compensating entry, appended, never an edit. Once a
//!   void is written down, the landings it names are never put in place:
//!   every retry and replay reads ahead for voids first.
//! - **A landing in place has landed.** One whose rows are written and whose
//!   removal from the store it left failed is answered as landed (with the
//!   removal pending), stays stuck until that is finished, and is never
//!   voided.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::App;
use crate::db::{self, List};
use crate::error::{ApiResult, HttpError};
use crate::names::parse_project_key;
use crate::repos::{self, Repo, Store};

/// A landing's place in the log: 1 for the first, one more for each after.
pub type Stamp = u64;

/// The entry format this server writes and reads. An entry of a newer format
/// is refused, never guessed at; a field added later is defaulted when an
/// entry without it is read (#411's epochs and merge commit, #412's rooted
/// tips).
pub const FORMAT: u32 = 1;

/// The most landings one batch writes.
const BATCH: usize = 512;
/// How long a move waits for pushes that began where a tracker was.
const PUSHES_DRAIN: Duration = Duration::from_secs(10 * 60);
/// How long a landing waits for a root another landing holds before it is
/// answered 503, by default.
const ROOT_WAIT: Duration = Duration::from_secs(10);
/// Who the log's commits are by.
const COMMITTER: &str = "newgit serve <serve@newgit.invalid>";

/// One landing, as the log keeps it: everything to put in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// [`FORMAT`] when it was written.
    pub format: u32,
    /// Who landed it; a merge's objects wait in their landing.
    pub by: String,
    /// What it changes, by home (`<owner>/<project>`): the roots it touches,
    /// every one named before it waits for them.
    #[serde(default)]
    pub homes: BTreeMap<String, Change>,
    /// Landings this one voids: written down, never put in place, and given
    /// up by an operator. Each of their homes is set back to what SQLite
    /// has, which they never changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voids: Vec<Stamp>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            format: FORMAT,
            by: String::new(),
            homes: BTreeMap::new(),
            voids: Vec::new(),
        }
    }
}

/// What one landing changes in one home.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Which project of this name it was written for
    /// (`db::ProjectInfo::incarnation`). It acts on that one only: a project
    /// deleted and made again under the same name is another.
    #[serde(default)]
    pub incarnation: String,
    /// The project is made by this landing, as `incarnation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made: Option<Made>,
    /// This is the home's whole state, not a change to it: the log's tree
    /// forgets anything else it had there (a void, or the first entry of a
    /// data directory from before the log).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole: bool,
    /// Branches whose lists it sets, whole; made if they are not yet.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, List>,
    /// Trackers whose history it puts in another store, or makes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<String, Move>,
    /// Tracker refs it moves, by tracker, as the tracker's readers name them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refs: BTreeMap<String, BTreeMap<String, RefMove>>,
    /// Commits waiting in the lander's landing (`Repo::landing_dir`) that go
    /// into a moving tracker's new store once this is written down, and not
    /// before: for a move between audiences not ordered by inclusion, no
    /// store may hold them until the landing happened.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub takes: BTreeMap<String, Vec<String>>,
    /// Moves of the landings this voids that copied a tracker toward a store
    /// it never reached: what was copied there is removed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unmoves: BTreeMap<String, Move>,
    /// The project is deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// A project made, by whom and under what name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Made {
    pub owner: String,
    pub name: String,
}

/// A tracker's history put in store `to`: moved from `from`, or made there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub from: Option<String>,
    pub to: String,
}

/// One ref a landing moves: from `old` (what it was when the landing was
/// checked; `None`, absent) to `new` (`None`, deleted). Put in place by
/// compare-and-swap: a ref already at `new` is done, and one at neither is
/// left as it is and said, never overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefMove {
    #[serde(default)]
    pub old: Option<String>,
    pub new: Option<String>,
    /// Asked for by the lander with its own `old` ([`Asked`]): one that
    /// moved meanwhile is said back to it ([`Landed::behind`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub asked: bool,
}

impl RefMove {
    /// A move to `new` from wherever the ref is when the landing is checked.
    pub fn to(new: Option<String>) -> Self {
        Self {
            old: None,
            new,
            asked: false,
        }
    }
}

impl Entry {
    fn roots(&self) -> BTreeSet<String> {
        self.homes.keys().cloned().collect()
    }

    /// Whether putting it in place copies or removes storage, which can be
    /// large: done by its own thread, holding its roots, not the log. Such a
    /// landing is in place before what it moved is removed where it was.
    fn moves_storage(&self) -> bool {
        self.homes.values().any(|change| {
            change.removed
                || !change.unmoves.is_empty()
                || change.stores.values().any(|step| step.from.is_some())
        })
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
    /// Each one's home is in the entry already, so its root is taken.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub home: String,
    pub tracker: String,
    /// As the tracker's readers name it: `refs/heads/main`.
    pub reference: String,
    pub old: Option<String>,
    pub new: String,
}

/// A landing written down and in place.
#[derive(Debug)]
pub struct Landed {
    pub stamp: Stamp,
    /// Asked refs that had moved meanwhile, and so were not moved.
    pub behind: Vec<Asked>,
    /// In place, readers see it, and removing what it moved from where it
    /// was failed, for this reason: that is tried again at each landing on
    /// its projects and at each start, and they refuse new landings (503)
    /// until it is done.
    pub pending: Option<String>,
}

/// What a batch's leader decided for one landing.
enum Outcome {
    Landed(Landed),
    /// Written down; its own thread puts it in place, holding its roots
    /// (the leader took the ones named, for it).
    Apply(Stamp, Entry, Landed, BTreeSet<String>),
    /// Its roots have landings written down and not in place: its thread
    /// tries them again, then asks again.
    Stuck,
}

struct Slot {
    roots: BTreeSet<String>,
    moves_storage: bool,
    /// Whose roots count as its own: its [`Hold`]'s, or its own.
    token: u64,
    landing: Mutex<Option<Landing>>,
    outcome: Mutex<Option<ApiResult<Outcome>>>,
}

/// A landing written down that could not be put in place, or finished, and
/// why.
#[derive(Debug, Clone)]
pub struct Stuck {
    pub stamp: Stamp,
    pub entry: Entry,
    pub cause: String,
    /// In place (its rows are written, readers see it), and only removing
    /// what it moved from where it was is left. Such a landing has landed:
    /// it is finished, never voided.
    pub placed: bool,
}

impl Stuck {
    fn new(stamp: Stamp, entry: Entry) -> Self {
        Self {
            stamp,
            entry,
            cause: String::new(),
            placed: false,
        }
    }

    /// What a landing refused because of it is told.
    fn refusal(&self) -> HttpError {
        let homes = homes_of(&self.entry);
        let (stamp, by, cause) = (self.stamp, &self.entry.by, &self.cause);
        let message = if self.placed {
            format!(
                "landing {stamp} (by {by}, in {homes}) is in place, and removing what it moved from where it was failed: {cause}. Nothing new lands in {homes} until that is done; it is tried again at each landing there and when `newgit serve` starts"
            )
        } else if !self.entry.voids.is_empty() {
            format!(
                "landing {stamp}, which voids {}, is written down but could not be put in place: {cause}. Nothing new lands in {homes} until it is; it is tried again at each landing there and when `newgit serve` starts",
                stamps(&self.entry.voids)
            )
        } else {
            format!(
                "landing {stamp} (by {by}, in {homes}) is written down but could not be put in place: {cause}. Nothing new lands in {homes} until it is; it is tried again at each landing there and when `newgit serve` starts, and an operator who cannot fix the cause voids it with `newgit serve --void-landing {stamp}`"
            )
        };
        HttpError::new(503, message)
    }
}

fn stamps(stamps: &[Stamp]) -> String {
    stamps
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What putting one landing in place came to.
enum Put {
    /// In place and finished; the asked refs it found moved, and left.
    Done(Vec<Skipped>),
    /// In place (readers see it); removing what it moved from where it was
    /// failed, for this reason.
    Unfinished(Vec<Skipped>, String),
    /// Not in place, for this reason.
    Failed(String),
    /// A later entry voids it: never put in place, whatever the void's own
    /// fate.
    Voided,
}

/// What the queue records for one landing put in place (or not).
enum Mark {
    /// In place and finished, with every landing it voids.
    Done(Stamp, Vec<Stamp>),
    Stuck(Stuck),
    /// Nothing changes: a voided landing waits for its void.
    Nothing,
}

/// A landing in place, as its lander is told: the asked refs it found
/// moved, and why removing what it moved from where it was is pending.
type Answered = (Vec<Skipped>, Option<String>);

/// What a landing put in place (or not) answers its lander, and what the
/// queue records for it. A landing in place has landed, finished or not.
fn settle(stuck: Stuck, put: Put) -> (ApiResult<Answered>, Mark) {
    match put {
        Put::Done(skipped) => (
            Ok((skipped, None)),
            Mark::Done(stuck.stamp, stuck.entry.voids),
        ),
        Put::Unfinished(skipped, cause) => {
            let stuck = Stuck {
                cause: cause.clone(),
                placed: true,
                ..stuck
            };
            eprintln!("newgit serve: {}", stuck.refusal().message);
            (Ok((skipped, Some(cause))), Mark::Stuck(stuck))
        }
        Put::Failed(cause) => {
            let stuck = Stuck {
                cause,
                placed: false,
                ..stuck
            };
            let refusal = stuck.refusal();
            eprintln!("newgit serve: {}", refusal.message);
            (Err(refusal), Mark::Stuck(stuck))
        }
        Put::Voided => (
            Err(HttpError::new(
                409,
                format!("landing {} is voided", stuck.stamp),
            )),
            Mark::Nothing,
        ),
    }
}

fn homes_of(entry: &Entry) -> String {
    entry.homes.keys().cloned().collect::<Vec<_>>().join(", ")
}

struct Queue {
    waiting: VecDeque<Arc<Slot>>,
    leading: bool,
    /// Roots held off the leader, by whom and why.
    held: BTreeMap<String, (u64, String)>,
    /// Landings written down whose putting in place failed, oldest first.
    stuck: Vec<Stuck>,
    /// The last stamp written, and its commit.
    tip: Stamp,
    tip_commit: String,
    /// Every landing up to here is in place, finished.
    applied: Stamp,
    /// Landings in place after `applied`, with one before them not yet.
    above: BTreeSet<Stamp>,
    tokens: u64,
}

impl Queue {
    fn stuck_roots(&self) -> BTreeSet<String> {
        self.stuck
            .iter()
            .flat_map(|stuck| stuck.entry.homes.keys().cloned())
            .collect()
    }

    /// Who else holds one of `roots`, and why: `None` when nobody but
    /// `token` does.
    fn blocker(&self, roots: &BTreeSet<String>, token: u64) -> Option<String> {
        roots.iter().find_map(|root| match self.held.get(root) {
            Some((holder, why)) if *holder != token => Some(format!("{root}: {why}")),
            _ => None,
        })
    }

    fn token(&mut self) -> u64 {
        self.tokens += 1;
        self.tokens
    }

    /// Hold each of `roots` nobody holds for `token`; returns those taken.
    fn take(&mut self, roots: &BTreeSet<String>, token: u64, why: &str) -> BTreeSet<String> {
        let mut taken = BTreeSet::new();
        for root in roots {
            if !self.held.contains_key(root) {
                self.held.insert(root.clone(), (token, why.to_owned()));
                taken.insert(root.clone());
            }
        }
        taken
    }

    fn release(&mut self, roots: &BTreeSet<String>, token: u64) {
        for root in roots {
            if self
                .held
                .get(root)
                .is_some_and(|(holder, _)| *holder == token)
            {
                self.held.remove(root);
            }
        }
    }

    /// `stamp` is in place.
    fn mark(&mut self, stamp: Stamp) {
        self.stuck.retain(|stuck| stuck.stamp != stamp);
        if stamp <= self.applied {
            return;
        }
        self.above.insert(stamp);
        while self.above.remove(&(self.applied + 1)) {
            self.applied += 1;
        }
    }

    fn stick(&mut self, stuck: Stuck) {
        self.stuck.retain(|known| known.stamp != stuck.stamp);
        self.stuck.push(stuck);
        self.stuck.sort_by_key(|stuck| stuck.stamp);
    }

    fn record(&mut self, mark: Mark) {
        match mark {
            Mark::Done(stamp, voids) => {
                self.mark(stamp);
                for voided in voids {
                    self.mark(voided);
                }
            }
            Mark::Stuck(stuck) => self.stick(stuck),
            Mark::Nothing => {}
        }
    }
}

pub struct Log {
    dir: Utf8PathBuf,
    queue: Mutex<Queue>,
    changed: Condvar,
    batches: AtomicU64,
    root_wait_ms: AtomicU64,
    /// `NEWGIT_SERVE_FAULT_FILE`: while a line of the file it names says a
    /// fault point (`before-log-commit`, `after-log-commit`,
    /// `between-store-commits`, `before-rows`, `during-copy`), the process
    /// stops there, as a kill would; `delay-batch` makes each batch's leader
    /// wait a moment before it takes what is waiting. For the crash tests.
    faults: Option<Utf8PathBuf>,
}

/// Roots held ahead of a landing ([`Log::hold`]): no other landing touches
/// them until it is dropped.
pub struct Hold<'l> {
    log: &'l Log,
    token: u64,
    roots: BTreeSet<String>,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let mut queue = self.log.lock();
        queue.release(&self.roots, self.token);
        self.log.changed.notify_all();
    }
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

    fn lock(&self) -> MutexGuard<'_, Queue> {
        lock(&self.queue)
    }

    /// The last stamp written.
    pub fn tip(&self) -> Stamp {
        self.lock().tip
    }

    /// Every landing up to this stamp is in place.
    pub fn applied(&self) -> Stamp {
        self.lock().applied
    }

    /// Landings written down and not in place, oldest first.
    pub fn stuck(&self) -> Vec<Stuck> {
        self.lock().stuck.clone()
    }

    /// How many batches have been written: fewer than the landings in them
    /// when landings arrived together.
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::SeqCst)
    }

    /// How long a landing waits for a root another holds before it is
    /// answered 503.
    pub fn set_root_wait(&self, wait: Duration) {
        self.root_wait_ms.store(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    }

    fn root_wait(&self) -> Duration {
        Duration::from_millis(self.root_wait_ms.load(Ordering::SeqCst))
    }

    fn armed(&self, at: &str) -> bool {
        self.faults.as_ref().is_some_and(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|faults| faults.lines().any(|fault| fault.trim() == at))
        })
    }

    /// Stop the process here if the fault file says to.
    fn fault(&self, at: &str) {
        if self.armed(at) {
            eprintln!("newgit serve: stopping at {at}");
            std::process::abort();
        }
    }

    /// Hold `roots` for work that must not race a landing there: wait (at
    /// most the root wait) until nobody else holds them, trying first any
    /// landing stuck there. Never called with the db lock held.
    pub fn hold(&self, app: &App, roots: BTreeSet<String>, why: &str) -> ApiResult<Hold<'_>> {
        if db::held_here() {
            return Err(HttpError::internal(
                "roots were waited on with the db lock held",
            ));
        }
        let deadline = Instant::now() + self.root_wait();
        let mut queue = self.lock();
        let token = queue.token();
        loop {
            if !queue.stuck_roots().is_disjoint(&roots) {
                drop(queue);
                self.retry(app, &roots, token)?;
                queue = self.lock();
                continue;
            }
            let blocker = queue.blocker(&roots, token);
            if !queue.leading && blocker.is_none() {
                queue.take(&roots, token, why);
                return Ok(Hold {
                    log: self,
                    token,
                    roots,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(busy(blocker));
            }
            queue = self
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Land `landing`: checked against what is there under its roots'
    /// locks, written down, and in place before this returns. Never called
    /// while this thread holds the db lock, which putting it in place takes.
    pub fn land(&self, app: &App, landing: Landing) -> ApiResult<Landed> {
        self.land_with(app, landing, None)
    }

    /// [`Self::land`] for roots `hold` holds already: they are its own.
    pub fn land_holding(&self, app: &App, landing: Landing, hold: &Hold<'_>) -> ApiResult<Landed> {
        self.land_with(app, landing, Some(hold.token))
    }

    fn land_with(&self, app: &App, landing: Landing, token: Option<u64>) -> ApiResult<Landed> {
        if db::held_here() {
            return Err(HttpError::internal(
                "a landing was waited on with the db lock held",
            ));
        }
        let mut queue = self.lock();
        let token = token.unwrap_or_else(|| queue.token());
        let slot = Arc::new(Slot {
            roots: landing.entry.roots(),
            moves_storage: landing.entry.moves_storage(),
            token,
            landing: Mutex::new(Some(landing)),
            outcome: Mutex::new(None),
        });
        queue.waiting.push_back(slot.clone());
        let mut deadline = Instant::now() + self.root_wait();
        loop {
            let outcome = lock(&slot.outcome).take();
            if let Some(outcome) = outcome {
                drop(queue);
                match outcome? {
                    Outcome::Landed(landed) => return Ok(landed),
                    Outcome::Apply(stamp, entry, mut landed, marked) => {
                        let note = |why: String| {
                            let mut queue = self.lock();
                            for root in &marked {
                                if let Some(held) = queue.held.get_mut(root) {
                                    held.1.clone_from(&why);
                                }
                            }
                        };
                        let stuck = Stuck::new(stamp, entry.clone());
                        let put = self
                            .put_in_place(app, std::slice::from_ref(&stuck), &note)
                            .pop()
                            .unwrap_or(Put::Failed("nothing was put in place".to_owned()));
                        let (answer, mark) = settle(stuck, put);
                        let mut queue = self.lock();
                        queue.release(&marked, token);
                        queue.record(mark);
                        self.changed.notify_all();
                        let (skipped, pending) = answer?;
                        landed.behind.extend(behind_of(&entry, stamp, &skipped));
                        landed.pending = pending;
                        return Ok(landed);
                    }
                    Outcome::Stuck => {
                        self.retry(app, &slot.roots, token)?;
                        queue = self.lock();
                        queue.waiting.push_back(slot.clone());
                        deadline = Instant::now() + self.root_wait();
                        continue;
                    }
                }
            }
            // Lead when nobody is, and this landing's roots are free; wait
            // otherwise, for a batch or a root to finish.
            let blocker = queue.blocker(&slot.roots, token);
            if queue.leading || blocker.is_some() {
                let now = Instant::now();
                if blocker.is_some() && now >= deadline {
                    let waiting = queue.waiting.len();
                    queue.waiting.retain(|waiting| !Arc::ptr_eq(waiting, &slot));
                    if queue.waiting.len() < waiting {
                        return Err(busy(blocker));
                    }
                }
                queue = self
                    .changed
                    .wait_timeout(
                        queue,
                        deadline
                            .saturating_duration_since(now)
                            .max(Duration::from_millis(50)),
                    )
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
                continue;
            }
            queue = self.lead(app, queue);
        }
    }

    /// Put in place again every landing stuck in `roots` (and in the roots
    /// those touch), oldest first, holding those roots for `token`. Answers
    /// 503 with the cause of the first that is still stuck.
    fn retry(&self, app: &App, roots: &BTreeSet<String>, token: u64) -> ApiResult<()> {
        let deadline = Instant::now() + self.root_wait();
        let mut queue = self.lock();
        let (entries, needed) = loop {
            let mut needed = roots.clone();
            loop {
                let before = needed.len();
                for stuck in &queue.stuck {
                    if !stuck.entry.roots().is_disjoint(&needed) {
                        needed.extend(stuck.entry.roots());
                    }
                }
                if needed.len() == before {
                    break;
                }
            }
            let entries: Vec<Stuck> = queue
                .stuck
                .iter()
                .filter(|stuck| !stuck.entry.roots().is_disjoint(&needed))
                .cloned()
                .collect();
            if entries.is_empty() {
                return Ok(());
            }
            let blocker = queue.blocker(&needed, token);
            if !queue.leading && blocker.is_none() {
                break (entries, needed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(busy(blocker));
            }
            queue = self
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        let why = format!(
            "landing {} is being put in place again",
            entries.first().map_or(0, |stuck| stuck.stamp)
        );
        let taken = queue.take(&needed, token, &why);
        drop(queue);
        let puts = self.put_in_place(app, &entries, &|_| {});
        let mut queue = self.lock();
        queue.release(&taken, token);
        let mut refusal = None;
        for (stuck, put) in entries.into_iter().zip(puts) {
            let (_, mark) = settle(stuck, put);
            if let Mark::Stuck(stuck) = &mark
                && refusal.is_none()
                && !stuck.entry.roots().is_disjoint(roots)
            {
                refusal = Some(stuck.refusal());
            }
            queue.record(mark);
        }
        self.changed.notify_all();
        match refusal {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// Put each of `entries` in place, one by one in stamp order, as far as
    /// SQLite says it is not: one in place and not finished is finished,
    /// one whose roots an earlier one left stuck is not tried, and one that
    /// a later entry among them voids is never put in place (the void puts
    /// back whatever of it is there).
    fn put_in_place(&self, app: &App, entries: &[Stuck], note: &dyn Fn(String)) -> Vec<Put> {
        let voided: BTreeSet<Stamp> = entries
            .iter()
            .flat_map(|stuck| stuck.entry.voids.iter().copied())
            .collect();
        let mut failed: BTreeMap<String, Stamp> = BTreeMap::new();
        let mut puts = Vec::new();
        for stuck in entries {
            if voided.contains(&stuck.stamp) {
                puts.push(Put::Voided);
                continue;
            }
            if let Some(before) = stuck.entry.roots().iter().find_map(|root| failed.get(root)) {
                puts.push(Put::Failed(format!(
                    "landing {before}, before it in {}, is stuck",
                    homes_of(&stuck.entry)
                )));
                continue;
            }
            let put = match placed_of(app, stuck.stamp) {
                Ok(Some(true)) => Put::Done(Vec::new()),
                Ok(Some(false)) => match finish(app, stuck.stamp, &stuck.entry, note) {
                    Ok(()) => Put::Done(Vec::new()),
                    Err(error) => Put::Unfinished(Vec::new(), error.cause().to_owned()),
                },
                Ok(None) => match apply_entries(app, &[(stuck.stamp, &stuck.entry)], self, note) {
                    Ok(Applied {
                        skipped,
                        unfinished: None,
                    }) => Put::Done(skipped),
                    Ok(Applied {
                        skipped,
                        unfinished: Some(error),
                    }) => Put::Unfinished(skipped, error.cause().to_owned()),
                    Err(error) => Put::Failed(error.cause().to_owned()),
                },
                Err(error) => Put::Failed(error.cause().to_owned()),
            };
            if !matches!(put, Put::Done(_)) {
                for root in stuck.entry.roots() {
                    failed.insert(root, stuck.stamp);
                }
            }
            puts.push(put);
        }
        puts
    }

    /// Take everything waiting that may land now, and land it as one batch.
    fn lead<'q>(&'q self, app: &App, mut queue: MutexGuard<'q, Queue>) -> MutexGuard<'q, Queue> {
        queue.leading = true;
        if self.armed("delay-batch") {
            drop(queue);
            std::thread::sleep(Duration::from_millis(1500));
            queue = self.lock();
        }
        let stuck_roots = queue.stuck_roots();
        let mut batch = Vec::new();
        let mut later = VecDeque::new();
        // Roots a landing in this batch moves storage in: nothing after it
        // there lands before it is in place.
        let mut taken = BTreeSet::new();
        while let Some(slot) = queue.waiting.pop_front() {
            if batch.len() >= BATCH
                || queue.blocker(&slot.roots, slot.token).is_some()
                || !slot.roots.is_disjoint(&taken)
            {
                later.push_back(slot);
                continue;
            }
            if !slot.roots.is_disjoint(&stuck_roots) {
                *lock(&slot.outcome) = Some(Ok(Outcome::Stuck));
                continue;
            }
            if slot.moves_storage {
                taken.extend(slot.roots.iter().cloned());
            }
            batch.push(slot);
        }
        queue.waiting = later;
        let tip = queue.tip;
        drop(queue);

        let processed = self.process(app, tip, &batch);

        let mut queue = self.lock();
        queue.tip += processed.written;
        if let Some(commit) = processed.tip_commit {
            queue.tip_commit = commit;
        }
        for mark in processed.marks {
            queue.record(mark);
        }
        for (slot, outcome) in batch.iter().zip(processed.outcomes) {
            let outcome = match outcome {
                Ok(Outcome::Apply(stamp, entry, landed, _)) => {
                    let marked = queue.take(
                        &slot.roots,
                        slot.token,
                        &format!("landing {stamp} is moving storage"),
                    );
                    Ok(Outcome::Apply(stamp, entry, landed, marked))
                }
                other => other,
            };
            *lock(&slot.outcome) = Some(outcome);
        }
        queue.leading = false;
        self.changed.notify_all();
        queue
    }

    /// Check, write, and put in place one batch.
    fn process(&self, app: &App, tip: Stamp, batch: &[Arc<Slot>]) -> Processed {
        let mut outcomes: Vec<Option<ApiResult<Outcome>>> = Vec::new();
        let mut accepted: Vec<(usize, Entry, Vec<Asked>)> = Vec::new();
        {
            let db = app.db.lock();
            let mut seen = Seen::default();
            let landings: Vec<Option<Landing>> = batch
                .iter()
                .map(|slot| lock(&slot.landing).take())
                .collect();
            if let Err(error) = seen.read_ahead(&app.config.data, &db, landings.iter().flatten()) {
                eprintln!(
                    "newgit serve: reading a batch's stores ahead: {}",
                    error.cause()
                );
            }
            for (index, landing) in landings.into_iter().enumerate() {
                let Some(landing) = landing else {
                    outcomes.push(Some(Err(HttpError::internal("a landing asked twice"))));
                    continue;
                };
                match check(&app.config.data, &db, landing, &mut seen) {
                    Ok((entry, behind)) => {
                        outcomes.push(None);
                        accepted.push((index, entry, behind));
                    }
                    Err(error) => outcomes.push(Some(Err(error))),
                }
            }
        }
        let mut processed = Processed::default();
        if accepted.is_empty() {
            processed.outcomes = outcomes.into_iter().map(answered).collect();
            return processed;
        }
        self.fault("before-log-commit");
        let entries: Vec<&Entry> = accepted.iter().map(|(_, entry, _)| entry).collect();
        if let Err(error) = write_entries(&self.dir, tip, &entries) {
            // Whether the log moved is what it says, not what fast-import
            // answered.
            let moved = count(&self.dir).unwrap_or(tip);
            if moved == tip {
                for (index, _, _) in &accepted {
                    outcomes[*index] = Some(Err(HttpError::new(
                        500,
                        format!("writing the landing log: {}", error.cause()),
                    )));
                }
                processed.outcomes = outcomes.into_iter().map(answered).collect();
                return processed;
            }
            if moved != tip + entries.len() as Stamp {
                eprintln!(
                    "newgit serve: the landing log moved from {tip} to {moved} writing {} landings: stopping",
                    entries.len()
                );
                std::process::abort();
            }
        }
        processed.tip_commit = head(&self.dir).ok();
        self.batches.fetch_add(1, Ordering::SeqCst);
        self.fault("after-log-commit");
        processed.written = accepted.len() as Stamp;
        // Everything that moves no storage, put in place together: one ref
        // transaction per store and one SQLite transaction for the batch.
        let mut here = Vec::new();
        for (offset, (index, entry, behind)) in accepted.into_iter().enumerate() {
            let stamp = tip + 1 + offset as Stamp;
            let landed = Landed {
                stamp,
                behind,
                pending: None,
            };
            if batch[index].moves_storage {
                outcomes[index] = Some(Ok(Outcome::Apply(stamp, entry, landed, BTreeSet::new())));
            } else {
                here.push((index, stamp, entry, landed));
            }
        }
        let together: Vec<(Stamp, &Entry)> = here
            .iter()
            .map(|(_, stamp, entry, _)| (*stamp, entry))
            .collect();
        let puts: Vec<Put> = match apply_entries(app, &together, self, &|_| {}) {
            Ok(Applied {
                skipped,
                unfinished,
            }) => together
                .iter()
                .map(|_| match &unfinished {
                    None => Put::Done(skipped.clone()),
                    Some(error) => Put::Unfinished(skipped.clone(), error.cause().to_owned()),
                })
                .collect(),
            // One by one, so only the one at fault (and any after it in
            // its roots) is stuck.
            Err(error) => {
                eprintln!(
                    "newgit serve: a batch of {} landings could not be put in place together ({}); putting them in place one by one",
                    together.len(),
                    error.cause()
                );
                let one_by_one: Vec<Stuck> = here
                    .iter()
                    .map(|(_, stamp, entry, _)| Stuck::new(*stamp, entry.clone()))
                    .collect();
                self.put_in_place(app, &one_by_one, &|_| {})
            }
        };
        for ((index, stamp, entry, mut landed), put) in here.into_iter().zip(puts) {
            let (answer, mark) = settle(Stuck::new(stamp, entry.clone()), put);
            outcomes[index] = Some(answer.map(|(skipped, pending)| {
                landed.behind.extend(behind_of(&entry, stamp, &skipped));
                landed.pending = pending;
                Outcome::Landed(landed)
            }));
            processed.marks.push(mark);
        }
        processed.outcomes = outcomes.into_iter().map(answered).collect();
        processed
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

    /// Put in place every landing written down and not yet in place: run at
    /// startup, before anything is served. A landing SQLite says is in place
    /// is never applied again; one in place whose old storage is left is
    /// finished. One that fails is said, with why, and its roots refuse new
    /// landings (503) until it is put in place or voided.
    pub fn replay(&self, app: &App) -> ApiResult<()> {
        let (applied, tip) = {
            let queue = self.lock();
            (queue.applied, queue.tip)
        };
        let entries: Vec<Stuck> = self
            .entries_after(applied, tip)?
            .into_iter()
            .map(|(stamp, entry)| Stuck::new(stamp, entry))
            .collect();
        // Every entry read, so a landing a later entry voids is known to be
        // voided before it would be put in place.
        let puts = self.put_in_place(app, &entries, &|_| {});
        let mut queue = self.lock();
        for (stuck, put) in entries.into_iter().zip(puts) {
            queue.record(settle(stuck, put).1);
        }
        Ok(())
    }

    /// Give up landing `stamp`, which is stuck and not in place, and every
    /// landing stuck after it in the projects it touches: one compensating
    /// entry, appended, whose homes are set to what SQLite has (which those
    /// landings never changed), whose refs they moved are moved back, and
    /// whose copies toward a store they never reached are removed. Returns
    /// the compensating entry's stamp and the stamps it voids.
    ///
    /// A landing in place (its rows written; only removing what it moved
    /// from where it was is left) has landed, and is refused: it is
    /// finished by trying it again. Once the void is written down, the
    /// landings it names are never put in place, even if putting the void
    /// itself in place fails or is interrupted: it is tried again, like any
    /// stuck landing, and it removes whatever of them is there.
    pub fn void(&self, app: &App, stamp: Stamp) -> ApiResult<(Stamp, Vec<Stamp>)> {
        let (roots, voided) = self.to_void(stamp)?;
        // Whether any of them is in place is SQLite's to say, read without
        // the queue's lock.
        for stuck in &voided {
            if placed_of(app, stuck.stamp)?.is_some() {
                return Err(HttpError::new(
                    409,
                    format!(
                        "landing {} (by {}, in {}) is in place: readers see it, and only removing what it moved from where it was is left ({}). It has landed, so it cannot be voided; it is finished by trying it again, which every landing there and every start of `newgit serve` does: fix the cause and restart",
                        stuck.stamp,
                        stuck.entry.by,
                        homes_of(&stuck.entry),
                        if stuck.cause.is_empty() {
                            "no cause recorded"
                        } else {
                            stuck.cause.as_str()
                        }
                    ),
                ));
            }
        }
        let mut queue = self.lock();
        while queue.leading || queue.blocker(&roots, 0).is_some() {
            queue = self
                .changed
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(queue);
        let (again, still) = self.to_void(stamp)?;
        let mut queue = self.lock();
        let same = again == roots
            && still
                .iter()
                .map(|stuck| stuck.stamp)
                .eq(voided.iter().map(|stuck| stuck.stamp));
        if !same || queue.leading || queue.blocker(&roots, 0).is_some() {
            return Err(HttpError::new(
                409,
                format!(
                    "the landings stuck in {} changed meanwhile: void again",
                    roots.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        queue.leading = true;
        let tip = queue.tip;
        drop(queue);
        let written = self.write_void(app, tip, &roots, &voided);
        let mut queue = self.lock();
        let answer = match written {
            Ok((void_stamp, commit, entry)) => {
                queue.tip = void_stamp;
                queue.tip_commit = commit;
                drop(queue);
                // Put in place with the landings it voids, so they are known
                // to be voided whatever happens to it.
                let mut entries = voided.clone();
                entries.push(Stuck::new(void_stamp, entry));
                let mut puts = self.put_in_place(app, &entries, &|_| {});
                let put = puts
                    .pop()
                    .unwrap_or(Put::Failed("nothing was put in place".to_owned()));
                let void = entries
                    .pop()
                    .unwrap_or_else(|| Stuck::new(void_stamp, Entry::default()));
                let (answer, mark) = settle(void, put);
                queue = self.lock();
                queue.record(mark);
                match answer {
                    Ok(_) => Ok((void_stamp, voided.iter().map(|stuck| stuck.stamp).collect())),
                    Err(error) => Err(HttpError::new(
                        500,
                        format!(
                            "the void was written down as landing {void_stamp} and could not be put in place: {}. The landings it voids are never put in place; it is tried again at each landing on their projects and when `newgit serve` starts",
                            error.cause()
                        ),
                    )),
                }
            }
            Err(error) => Err(error),
        };
        queue.leading = false;
        self.changed.notify_all();
        answer
    }

    /// What voiding `stamp` gives up: it, and every landing stuck after it
    /// in the projects they touch, with those projects. Refused for a
    /// landing not stuck, a void, or one a void already gives up.
    fn to_void(&self, stamp: Stamp) -> ApiResult<(BTreeSet<String>, Vec<Stuck>)> {
        let queue = self.lock();
        if !queue.stuck.iter().any(|stuck| stuck.stamp == stamp) {
            return Err(HttpError::new(
                409,
                if stamp <= queue.tip {
                    format!("landing {stamp} is not stuck: it is in place")
                } else {
                    format!("there is no landing {stamp}: the log ends at {}", queue.tip)
                },
            ));
        }
        let mut roots = BTreeSet::new();
        let mut voided: Vec<Stuck> = Vec::new();
        for stuck in &queue.stuck {
            if stuck.stamp == stamp
                || (stuck.stamp > stamp && !stuck.entry.roots().is_disjoint(&roots))
            {
                roots.extend(stuck.entry.roots());
                voided.push(stuck.clone());
            }
        }
        for stuck in &voided {
            if !stuck.entry.voids.is_empty() {
                return Err(HttpError::new(
                    409,
                    format!(
                        "landing {} is a void (of {}) that could not be put in place: {}. It is tried again at each landing there and when `newgit serve` starts; fix the cause and restart",
                        stuck.stamp,
                        stamps(&stuck.entry.voids),
                        stuck.cause
                    ),
                ));
            }
            if let Some(by) = queue
                .stuck
                .iter()
                .find(|void| void.entry.voids.contains(&stuck.stamp))
            {
                return Err(HttpError::new(
                    409,
                    format!(
                        "landing {} is voided already, by landing {}, which could not be put in place yet: {}. It is tried again at each landing there and when `newgit serve` starts",
                        stuck.stamp, by.stamp, by.cause
                    ),
                ));
            }
        }
        Ok((roots, voided))
    }

    fn write_void(
        &self,
        app: &App,
        tip: Stamp,
        roots: &BTreeSet<String>,
        voided: &[Stuck],
    ) -> ApiResult<(Stamp, String, Entry)> {
        let mut entry = Entry {
            by: "newgit serve (void)".to_owned(),
            voids: voided.iter().map(|stuck| stuck.stamp).collect(),
            ..Entry::default()
        };
        {
            let db = app.db.lock();
            for home in roots {
                let change = match db::project(&db, home)? {
                    Some(info) => {
                        let (branches, stores) = db::home_state(&db, home)?;
                        let mut change = Change {
                            incarnation: info.incarnation,
                            whole: true,
                            branches,
                            stores: stores
                                .into_iter()
                                .map(|(tracker, to)| (tracker, Move { from: None, to }))
                                .collect(),
                            ..Change::default()
                        };
                        // Refs they moved (a store's transaction committed
                        // before a later step failed) go back where they were.
                        for stuck in voided {
                            let Some(theirs) = stuck.entry.homes.get(home) else {
                                continue;
                            };
                            if theirs.incarnation != change.incarnation {
                                continue;
                            }
                            for (tracker, moved) in &theirs.refs {
                                for (reference, step) in moved {
                                    let back = change
                                        .refs
                                        .entry(tracker.clone())
                                        .or_default()
                                        .entry(reference.clone())
                                        .or_insert_with(|| RefMove {
                                            old: None,
                                            new: step.old.clone(),
                                            asked: false,
                                        });
                                    back.old.clone_from(&step.new);
                                }
                            }
                            for (tracker, step) in &theirs.stores {
                                if step.from.is_some()
                                    && db::tracker_store(&db, home, tracker)? == step.from
                                {
                                    change.unmoves.insert(tracker.clone(), step.clone());
                                }
                            }
                        }
                        change
                    }
                    None => Change {
                        removed: true,
                        ..Change::default()
                    },
                };
                entry.homes.insert(home.clone(), change);
            }
        }
        write_entries(&self.dir, tip, &[&entry])?;
        Ok((tip + 1, head(&self.dir)?, entry))
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

/// What one batch came to.
#[derive(Default)]
struct Processed {
    outcomes: Vec<ApiResult<Outcome>>,
    /// How many were written.
    written: Stamp,
    /// The log's commit after them.
    tip_commit: Option<String>,
    /// Each written one put in place here, or stuck.
    marks: Vec<Mark>,
}

fn busy(blocker: Option<String>) -> HttpError {
    HttpError::new(
        503,
        format!(
            "this project is busy ({}): try again in a moment",
            blocker.unwrap_or_else(|| "another landing holds it".to_owned())
        ),
    )
}

fn init_log(dir: &Utf8Path) -> anyhow::Result<()> {
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
fn count(dir: &Utf8Path) -> anyhow::Result<Stamp> {
    let output = repos::git(dir, &["rev-list", "--count", "refs/heads/main"])?;
    anyhow::ensure!(
        output.status.success(),
        "the landing log {dir} cannot be read ({}): refusing to start (restore it from a backup)",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().parse()?)
}

fn head(dir: &Utf8Path) -> anyhow::Result<String> {
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
fn newest_entry(dir: &Utf8Path) -> anyhow::Result<(Stamp, Entry)> {
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

fn parse_entry(message: &str) -> ApiResult<(Stamp, Entry)> {
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
fn write_entries(dir: &Utf8Path, tip: Stamp, entries: &[&Entry]) -> ApiResult<()> {
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

// ── The check ─────────────────────────────────────────────────────────────

/// What the landings checked before this one in the batch leave, so each is
/// checked against them as well as against what is there.
#[derive(Default)]
struct Seen {
    lists: BTreeMap<(String, String), Option<List>>,
    stores: BTreeMap<(String, String), Option<String>>,
    /// Each home's incarnation, `None` when there is no project.
    projects: BTreeMap<String, Option<String>>,
    /// Homes made or removed in the batch: nothing of SQLite's or the stores'
    /// is theirs.
    fresh: BTreeSet<String>,
    refs: BTreeMap<(String, String, String), Option<String>>,
    /// What each store answered for each name asked of it this batch
    /// (`None`: missing), so each store is read once per batch, not once per
    /// landing.
    resolved: BTreeMap<Utf8PathBuf, BTreeMap<String, Option<String>>>,
}

impl Seen {
    /// [`repos::resolve`] for `names` in the store at `dir`, reading only
    /// what this batch has not read there yet.
    fn resolve(&mut self, dir: &Utf8Path, names: &[String]) -> ApiResult<BTreeMap<String, String>> {
        let known = self.resolved.entry(dir.to_owned()).or_default();
        let missing: Vec<String> = names
            .iter()
            .filter(|name| !known.contains_key(*name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let found = repos::resolve(dir, &missing)?;
            for name in missing {
                let value = found.get(&name).cloned();
                known.insert(name, value);
            }
        }
        Ok(names
            .iter()
            .filter_map(|name| Some((name.clone(), known.get(name)?.clone()?)))
            .collect())
    }

    /// Read ahead, in one go per store, every ref and commit the landings
    /// in `landings` will ask of the stores their trackers are in now.
    fn read_ahead<'l>(
        &mut self,
        data: &Utf8Path,
        db: &Connection,
        landings: impl IntoIterator<Item = &'l Landing>,
    ) -> ApiResult<()> {
        let mut names: BTreeMap<Utf8PathBuf, BTreeSet<String>> = BTreeMap::new();
        let mut want =
            |home: &str, store: Option<&str>, tracker: &str, name: String| -> ApiResult<()> {
                let repo = repo_in(data, home, store, tracker)?;
                let name = if name.starts_with("refs/") {
                    repo.full(&name)
                } else {
                    name
                };
                names.entry(repo.dir()).or_default().insert(name);
                Ok(())
            };
        for landing in landings {
            for (home, change) in &landing.entry.homes {
                if home_dir(data, home).is_err() {
                    continue;
                }
                for (tracker, moved) in &change.refs {
                    let now = db::tracker_store(db, home, tracker)?;
                    let to = change.stores.get(tracker).map(|step| step.to.clone());
                    for (reference, step) in moved {
                        want(home, now.as_deref(), tracker, reference.clone())?;
                        if let Some(new) = &step.new {
                            want(home, now.as_deref(), tracker, new.clone())?;
                            if to.is_some() {
                                want(home, to.as_deref(), tracker, new.clone())?;
                            }
                        }
                    }
                }
            }
            for step in &landing.asked {
                if home_dir(data, &step.home).is_err() {
                    continue;
                }
                let now = db::tracker_store(db, &step.home, &step.tracker)?;
                want(
                    &step.home,
                    now.as_deref(),
                    &step.tracker,
                    step.reference.clone(),
                )?;
                want(&step.home, now.as_deref(), &step.tracker, step.new.clone())?;
            }
        }
        for (dir, names) in names {
            let names: Vec<String> = names.into_iter().collect();
            self.resolve(&dir, &names)?;
        }
        Ok(())
    }

    fn incarnation(&self, db: &Connection, home: &str) -> ApiResult<Option<String>> {
        match self.projects.get(home) {
            Some(known) => Ok(known.clone()),
            None => Ok(db::project(db, home)?.map(|info| info.incarnation)),
        }
    }

    fn list(&self, db: &Connection, home: &str, branch: &str) -> ApiResult<Option<List>> {
        match self.lists.get(&(home.to_owned(), branch.to_owned())) {
            Some(list) => Ok(list.clone()),
            None if self.fresh.contains(home) => Ok(None),
            None => db::list_of(db, home, branch),
        }
    }

    fn store(&self, db: &Connection, home: &str, tracker: &str) -> ApiResult<Option<String>> {
        match self.stores.get(&(home.to_owned(), tracker.to_owned())) {
            Some(store) => Ok(store.clone()),
            None if self.fresh.contains(home) => Ok(None),
            None => db::tracker_store(db, home, tracker),
        }
    }
}

fn home_dir(data: &Utf8Path, home: &str) -> ApiResult<Utf8PathBuf> {
    parse_project_key(home)
        .map(|id| id.dir(data))
        .ok_or_else(|| HttpError::new(400, format!("`{home}` is not a project")))
}

fn store_of(name: &str) -> ApiResult<Store> {
    Store::parse(name).ok_or_else(|| HttpError::new(500, format!("no store `{name}`")))
}

fn repo_in(data: &Utf8Path, home: &str, store: Option<&str>, tracker: &str) -> ApiResult<Repo> {
    let store = store.map(store_of).transpose()?;
    Ok(Repo::tracker(&home_dir(data, home)?, store, tracker))
}

/// The checks under the lock: every project the incarnation the landing was
/// written for, every branch still as it expects, every tracker still where
/// it expects, each asked ref still at its old commit (one moved meanwhile
/// is left, and said), every other ref's old value recorded, and every
/// commit a ref moves to in the store it lands in. What this batch lands
/// before it counts.
fn check(
    data: &Utf8Path,
    db: &Connection,
    landing: Landing,
    seen: &mut Seen,
) -> ApiResult<(Entry, Vec<Asked>)> {
    let Landing {
        mut entry,
        expect,
        placed,
        asked,
    } = landing;
    entry.format = FORMAT;
    for (home, change) in &entry.homes {
        home_dir(data, home)?;
        let now = seen.incarnation(db, home)?;
        if change.made.is_some() {
            if now.is_some() {
                return Err(HttpError::new(
                    409,
                    format!("project `{home}` already exists"),
                ));
            }
        } else if now.as_deref() != Some(change.incarnation.as_str()) {
            return Err(HttpError::new(
                409,
                match now {
                    None => format!("project `{home}` was deleted since you looked"),
                    Some(_) => format!(
                        "project `{home}` was deleted and made again since you looked: start again"
                    ),
                },
            ));
        }
        for step in change.stores.values() {
            store_of(&step.to)?;
            if let Some(from) = &step.from {
                store_of(from)?;
            }
        }
    }
    for want in &expect {
        let now = seen.list(db, &want.home, &want.branch)?;
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
        if seen.store(db, &want.home, &want.tracker)? != want.store {
            return Err(HttpError::new(
                409,
                format!(
                    "`{}` moved to another store since you looked: merge again",
                    want.tracker
                ),
            ));
        }
    }
    // Every ref it moves, as it is now: where the batch left it, or read
    // from the store the tracker is in before this landing, one read per
    // store.
    let mut wanted: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (home, change) in &entry.homes {
        for (tracker, moved) in &change.refs {
            for reference in moved.keys() {
                wanted.insert((home.clone(), tracker.clone(), reference.clone()));
            }
        }
    }
    for step in &asked {
        if !entry.homes.contains_key(&step.home) {
            return Err(HttpError::internal(format!(
                "a landing asks a ref in `{}`, which its entry does not name",
                step.home
            )));
        }
        wanted.insert((
            step.home.clone(),
            step.tracker.clone(),
            step.reference.clone(),
        ));
    }
    let now = read_refs(data, db, seen, &wanted)?;
    let mut behind = Vec::new();
    for (home, change) in &mut entry.homes {
        for (tracker, moved) in &mut change.refs {
            for (reference, step) in moved {
                step.old = now
                    .get(&(home.clone(), tracker.clone(), reference.clone()))
                    .cloned()
                    .flatten();
                step.asked = false;
            }
        }
    }
    for step in asked {
        let key = (
            step.home.clone(),
            step.tracker.clone(),
            step.reference.clone(),
        );
        let current = now.get(&key).cloned().flatten();
        if current.as_deref() == Some(step.new.as_str()) {
            continue;
        }
        if current != step.old {
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
            .insert(
                step.reference.clone(),
                RefMove {
                    old: step.old.clone(),
                    new: Some(step.new.clone()),
                    asked: true,
                },
            );
    }
    // Every commit a ref moves to is in the store it lands in (or comes with
    // the tracker's move, or waits in the lander's landing for it).
    let mut needs: BTreeMap<Utf8PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut whose: Vec<(Utf8PathBuf, Option<Utf8PathBuf>, String, String, bool)> = Vec::new();
    for (home, change) in &entry.homes {
        for (tracker, moved) in &change.refs {
            let step = change.stores.get(tracker);
            let to = match step {
                Some(step) => Some(step.to.clone()),
                None => seen.store(db, home, tracker)?,
            };
            let lands = repo_in(data, home, to.as_deref(), tracker)?.dir();
            let from = step
                .and_then(|step| step.from.as_deref())
                .map(|from| repo_in(data, home, Some(from), tracker).map(|repo| repo.dir()))
                .transpose()?;
            for step in moved.values() {
                let Some(new) = &step.new else {
                    continue;
                };
                let taken = change
                    .takes
                    .get(tracker)
                    .is_some_and(|takes| takes.contains(new));
                needs.entry(lands.clone()).or_default().insert(new.clone());
                if let Some(from) = &from {
                    needs.entry(from.clone()).or_default().insert(new.clone());
                }
                whose.push((
                    lands.clone(),
                    from.clone(),
                    tracker.clone(),
                    new.clone(),
                    taken,
                ));
            }
        }
    }
    let mut held: BTreeMap<Utf8PathBuf, BTreeMap<String, String>> = BTreeMap::new();
    for (dir, commits) in needs {
        let commits: Vec<String> = commits.into_iter().collect();
        held.insert(dir.clone(), seen.resolve(&dir, &commits)?);
    }
    for (lands, from, tracker, commit, taken) in whose {
        let has = |dir: &Utf8PathBuf| {
            held.get(dir)
                .is_some_and(|found| found.contains_key(&commit))
        };
        if !(taken || has(&lands) || from.as_ref().is_some_and(has)) {
            return Err(HttpError::new(
                400,
                format!(
                    "`{tracker}`'s commit {commit} is not on the remote: push it to its landing ref, then finish"
                ),
            ));
        }
    }
    // What it leaves, for the ones after it in the batch.
    for (home, change) in &entry.homes {
        if change.removed {
            seen.projects.insert(home.clone(), None);
            seen.fresh.insert(home.clone());
            continue;
        }
        if change.made.is_some() {
            seen.projects
                .insert(home.clone(), Some(change.incarnation.clone()));
            seen.fresh.insert(home.clone());
        }
        for (branch, list) in &change.branches {
            seen.lists
                .insert((home.clone(), branch.clone()), Some(list.clone()));
        }
        for (tracker, step) in &change.stores {
            seen.stores
                .insert((home.clone(), tracker.clone()), Some(step.to.clone()));
        }
        for (tracker, moved) in &change.refs {
            for (reference, step) in moved {
                seen.refs.insert(
                    (home.clone(), tracker.clone(), reference.clone()),
                    step.new.clone(),
                );
            }
        }
    }
    Ok((entry, behind))
}

/// A ref, as a landing names it: home, tracker, and ref as its readers name
/// it.
type RefKey = (String, String, String);

/// Each of `wanted` (home, tracker, ref as its readers name it) as it is:
/// where the batch left it, or read from the store its tracker is in.
fn read_refs(
    data: &Utf8Path,
    db: &Connection,
    seen: &mut Seen,
    wanted: &BTreeSet<(String, String, String)>,
) -> ApiResult<BTreeMap<(String, String, String), Option<String>>> {
    let mut out = BTreeMap::new();
    let mut by_store: BTreeMap<Utf8PathBuf, Vec<(RefKey, String)>> = BTreeMap::new();
    for key in wanted {
        if let Some(known) = seen.refs.get(key) {
            out.insert(key.clone(), known.clone());
            continue;
        }
        let (home, tracker, reference) = key;
        if seen.fresh.contains(home) {
            out.insert(key.clone(), None);
            continue;
        }
        let repo = repo_in(
            data,
            home,
            seen.store(db, home, tracker)?.as_deref(),
            tracker,
        )?;
        by_store
            .entry(repo.dir())
            .or_default()
            .push((key.clone(), repo.full(reference)));
    }
    for (dir, keys) in by_store {
        let names: Vec<String> = keys.iter().map(|(_, full)| full.clone()).collect();
        let found = seen.resolve(&dir, &names)?;
        for (key, full) in keys {
            out.insert(key, found.get(&full).cloned());
        }
    }
    Ok(out)
}

// ── Putting it in place ───────────────────────────────────────────────────

/// An asked ref a landing found moved when it was put in place: left.
#[derive(Debug, Clone)]
struct Skipped {
    stamp: Stamp,
    home: String,
    tracker: String,
    reference: String,
}

fn behind_of(entry: &Entry, stamp: Stamp, skipped: &[Skipped]) -> Vec<Asked> {
    skipped
        .iter()
        .filter(|skip| skip.stamp == stamp)
        .filter_map(|skip| {
            let step = entry
                .homes
                .get(&skip.home)?
                .refs
                .get(&skip.tracker)?
                .get(&skip.reference)?;
            step.asked.then(|| Asked {
                home: skip.home.clone(),
                tracker: skip.tracker.clone(),
                reference: skip.reference.clone(),
                old: step.old.clone(),
                new: step.new.clone().unwrap_or_default(),
            })
        })
        .collect()
}

fn placed_of(app: &App, stamp: Stamp) -> ApiResult<Option<bool>> {
    let db = app.db.lock();
    db::placed(&db, stamp)
}

/// Whether `change` acts on the project there now: the incarnation it was
/// written for (or, when it makes one, no project yet).
fn acts(current: Option<&String>, change: &Change) -> bool {
    match &change.made {
        Some(_) => current.is_none() || current == Some(&change.incarnation),
        None => current == Some(&change.incarnation),
    }
}

/// Put `entries` (in stamp order, none of them in place yet) in place
/// together:
///
/// 1. Each tracker moved between stores is copied into its new store, and
///    the commits it takes from the lander's landing follow. Nothing says it
///    is there yet, so nobody writes there yet, and running this again
///    (before step 2 is done) copies again. A project made has whatever a
///    project of its name left on disk removed.
/// 2. Under the db lock: every ref, by compare-and-swap, one transaction per
///    store, all prepared before any is committed; then every row and the
///    stamps marked in place, in one SQLite transaction. Refs are synced
///    before the rows are written, and the rows synced before this returns.
/// 3. What moved is removed where it was, once every push that began there
///    is done; a deleted project's home goes. Then the stamps are marked
///    finished.
///
/// A change written for another incarnation of its project than the one
/// there now does nothing. An error means the rows were not written: the
/// entries are not in place. Once they are, they have landed, and a failure
/// of step 3 is said in [`Applied::unfinished`].
fn apply_entries(
    app: &App,
    entries: &[(Stamp, &Entry)],
    log: &Log,
    note: &dyn Fn(String),
) -> ApiResult<Applied> {
    if entries.is_empty() {
        return Ok(Applied::default());
    }
    let data = &app.config.data;
    // Which changes act: the project there is the one each was written for,
    // as the entries before it leave it.
    let mut acting: Vec<BTreeSet<String>> = Vec::new();
    {
        let db = app.db.lock();
        let mut current: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (_, entry) in entries {
            let mut here = BTreeSet::new();
            for (home, change) in &entry.homes {
                if !current.contains_key(home) {
                    current.insert(
                        home.clone(),
                        db::project(&db, home)?.map(|info| info.incarnation),
                    );
                }
                let now = current.get(home).cloned().flatten();
                if acts(now.as_ref(), change) || (change.removed && now.is_none()) {
                    here.insert(home.clone());
                }
                if change.removed {
                    current.insert(home.clone(), None);
                } else if change.made.is_some() {
                    current.insert(home.clone(), Some(change.incarnation.clone()));
                }
            }
            acting.push(here);
        }
    }
    // 1. Copies, before anything says they are there.
    let mut moving: Vec<Moving> = Vec::new();
    for ((stamp, entry), here) in entries.iter().zip(&acting) {
        for (home, change) in &entry.homes {
            if !here.contains(home) || change.removed {
                continue;
            }
            if change.made.is_some() {
                let exists = db::project(&app.db.lock(), home)?.is_some();
                if !exists {
                    // A deleted project's name is free again: start from
                    // nothing on disk too.
                    repos::delete(&home_dir(data, home)?)?;
                }
            }
            for (tracker, step) in &change.unmoves {
                let Some(from) = &step.from else {
                    continue;
                };
                repo_in(data, home, Some(from), tracker)?.uncopy(&store_of(&step.to)?)?;
            }
            for (tracker, step) in &change.stores {
                let Some(from) = &step.from else {
                    // Made there: the repo was made before it landed.
                    continue;
                };
                note(format!(
                    "landing {stamp} is copying {tracker} into the {} store",
                    step.to
                ));
                let to = store_of(&step.to)?;
                let repo = repo_in(data, home, Some(from), tracker)?;
                let copied = repo.copy_to(&to)?;
                log.fault("during-copy");
                let borrow = if repos::store_exists(&repo.dir()) {
                    repo.clone()
                } else {
                    copied.to.clone()
                };
                for commit in change.takes.get(tracker).into_iter().flatten() {
                    copied.to.take_landing(&entry.by, commit, &borrow)?;
                }
                moving.push((*stamp, home.clone(), tracker.clone(), repo, copied));
            }
        }
    }
    // 2. Refs, then rows, under the db lock.
    let mut skipped = Vec::new();
    {
        let mut db = app.db.lock();
        // Store, then full ref name: its first old value, its last new one,
        // and every landing's move of it.
        let mut refs: BTreeMap<Utf8PathBuf, BTreeMap<String, Chain>> = BTreeMap::new();
        // Where each tracker is as the landings before leave it.
        let mut stores: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
        for ((stamp, entry), here) in entries.iter().zip(&acting) {
            for (home, change) in &entry.homes {
                if !here.contains(home) || change.removed {
                    continue;
                }
                for (tracker, step) in &change.stores {
                    stores.insert((home.clone(), tracker.clone()), Some(step.to.clone()));
                }
                for (tracker, moved) in &change.refs {
                    let key = (home.clone(), tracker.clone());
                    let store = match stores.get(&key) {
                        Some(store) => store.clone(),
                        None => db::tracker_store(&db, home, tracker)?,
                    };
                    let repo = repo_in(data, home, store.as_deref(), tracker)?;
                    let at = refs.entry(repo.dir()).or_default();
                    for (reference, step) in moved {
                        let chain = at.entry(repo.full(reference)).or_insert_with(|| Chain {
                            old: step.old.clone(),
                            new: step.new.clone(),
                            moves: Vec::new(),
                        });
                        chain.new.clone_from(&step.new);
                        chain.moves.push(Skipped {
                            stamp: *stamp,
                            home: home.clone(),
                            tracker: tracker.clone(),
                            reference: reference.clone(),
                        });
                    }
                }
            }
        }
        let zero = "0000000000000000000000000000000000000000";
        let mut prepared = Vec::new();
        for (dir, chains) in &refs {
            let names: Vec<String> = chains.keys().cloned().collect();
            let now = repos::resolve(dir, &names)?;
            let mut lines = Vec::new();
            for (reference, chain) in chains {
                let current = now.get(reference);
                if current == chain.new.as_ref() {
                    continue;
                }
                if current != chain.old.as_ref() {
                    // Moved by someone else since it was checked: left as
                    // they left it, never overwritten.
                    eprintln!(
                        "newgit serve: {reference} in {dir} is at {}, not {} as checked; left as it is",
                        current.map_or("nothing", String::as_str),
                        chain.old.as_deref().unwrap_or("nothing")
                    );
                    skipped.extend(chain.moves.iter().cloned());
                    continue;
                }
                let old = chain.old.as_deref().unwrap_or(zero);
                lines.push(match &chain.new {
                    Some(new) => format!("update {reference} {new} {old}"),
                    None => format!("delete {reference} {old}"),
                });
            }
            if !lines.is_empty() {
                prepared.push(repos::Prepared::new(dir, &lines)?);
            }
        }
        // Every store's transaction prepared: now committed, one by one.
        let several = prepared.len() > 1;
        for (index, transaction) in prepared.into_iter().enumerate() {
            transaction.commit()?;
            if several && index == 0 {
                log.fault("between-store-commits");
            }
        }
        log.fault("before-rows");
        db.execute_batch("PRAGMA synchronous = FULL")?;
        let written = write_rows(&mut db, entries, &acting);
        if let Err(error) = db.execute_batch("PRAGMA synchronous = NORMAL") {
            eprintln!("newgit serve: setting SQLite back to synchronous=NORMAL: {error}");
        }
        written?;
        app.trackers_moved();
    }
    // 3. What moved, removed where it was: the entries are in place, so
    // whatever fails here leaves them landed and not finished.
    let unfinished = remove_moved(app, entries, &acting, &moving, note).err();
    Ok(Applied {
        skipped,
        unfinished,
    })
}

/// What [`apply_entries`] put in place.
#[derive(Default)]
struct Applied {
    /// Asked refs found moved, and left.
    skipped: Vec<Skipped>,
    /// Why removing what moved from where it was failed: the entries are in
    /// place, and not finished.
    unfinished: Option<HttpError>,
}

/// A tracker being moved by landing `stamp`: home, tracker, where it was,
/// and what was copied.
type Moving = (Stamp, String, String, Repo, repos::Copied);

/// Step 3 of [`apply_entries`]: what moved, removed where it was, once
/// every push that began there is done; a deleted project's home removed;
/// the stamps marked finished.
fn remove_moved(
    app: &App,
    entries: &[(Stamp, &Entry)],
    acting: &[BTreeSet<String>],
    moving: &[Moving],
    note: &dyn Fn(String),
) -> ApiResult<()> {
    let mut done = BTreeSet::new();
    for (stamp, home, tracker, repo, copied) in moving {
        note(format!(
            "landing {stamp} is waiting for pushes into {tracker} in the {} store to end before removing it there",
            repo.store.name()
        ));
        app.pushes
            .drain(home, tracker, &repo.store.name(), PUSHES_DRAIN);
        repo.finish_move(copied)?;
        done.insert(*stamp);
    }
    for ((stamp, entry), here) in entries.iter().zip(acting) {
        for (home, change) in &entry.homes {
            if here.contains(home) && change.removed {
                remove_home(app, home)?;
            }
        }
        if entry.moves_storage() {
            done.insert(*stamp);
        }
    }
    if !done.is_empty() {
        let db = app.db.lock();
        for stamp in done {
            db::set_placed(&db, stamp, true)?;
        }
    }
    Ok(())
}

/// One ref's moves in a batch.
struct Chain {
    old: Option<String>,
    new: Option<String>,
    moves: Vec<Skipped>,
}

/// Every row `entries` change, and each stamp marked in place (finished,
/// unless storage is left to remove), in one SQLite transaction.
fn write_rows(
    db: &mut Connection,
    entries: &[(Stamp, &Entry)],
    acting: &[BTreeSet<String>],
) -> ApiResult<()> {
    let transaction = db.transaction()?;
    for ((stamp, entry), here) in entries.iter().zip(acting) {
        for (home, change) in &entry.homes {
            if !here.contains(home) {
                continue;
            }
            if change.removed {
                db::delete_project(&transaction, home)?;
                continue;
            }
            if let Some(made) = &change.made
                && db::project(&transaction, home)?.is_none()
                && !db::create_incarnation(
                    &transaction,
                    home,
                    &made.owner,
                    &made.name,
                    &change.incarnation,
                )?
            {
                return Err(HttpError::new(
                    500,
                    format!("project `{home}` could not be made"),
                ));
            }
            for (branch, list) in &change.branches {
                db::set_branch(&transaction, home, branch, list, &entry.by, *stamp)?;
            }
            for (tracker, step) in &change.stores {
                db::set_tracker_store(&transaction, home, tracker, &step.to)?;
            }
            if change.whole {
                // Its whole state: rows it does not name go (a void's, of
                // anything a voided landing could have left).
                let stores = change
                    .stores
                    .iter()
                    .map(|(tracker, step)| (tracker.clone(), step.to.clone()))
                    .collect();
                db::keep_only(&transaction, home, &(change.branches.clone(), stores))?;
            }
        }
        db::set_placed(&transaction, *stamp, !entry.moves_storage())?;
        for voided in &entry.voids {
            db::set_placed(&transaction, *voided, true)?;
            for home in entry.homes.keys() {
                db::set_voided(&transaction, *voided, *stamp, home)?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

/// A deleted project's home removed from disk, unless a project of its name
/// was made again since.
fn remove_home(app: &App, home: &str) -> ApiResult<()> {
    let again = db::project(&app.db.lock(), home)?.is_some();
    if !again {
        repos::delete(&home_dir(&app.config.data, home)?)?;
    }
    Ok(())
}

/// Step 3 of [`apply_entries`] for a landing put in place before a restart
/// and not finished: what is left where it moved from is carried forward
/// (never back) and removed there.
fn finish(app: &App, stamp: Stamp, entry: &Entry, note: &dyn Fn(String)) -> ApiResult<()> {
    let data = &app.config.data;
    for (home, change) in &entry.homes {
        if change.removed {
            remove_home(app, home)?;
            continue;
        }
        let current = db::project(&app.db.lock(), home)?.map(|info| info.incarnation);
        if current.as_deref() != Some(change.incarnation.as_str()) {
            continue;
        }
        for (tracker, step) in &change.stores {
            let Some(from) = &step.from else {
                continue;
            };
            note(format!(
                "landing {stamp} is removing {tracker} from the {from} store"
            ));
            let was = repo_in(data, home, Some(from), tracker)?;
            let now = repo_in(data, home, Some(&step.to), tracker)?;
            was.finish_move_after_restart(&now)?;
        }
    }
    db::set_placed(&app.db.lock(), stamp, true)?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn anyhow_of(error: HttpError) -> anyhow::Error {
    anyhow::anyhow!("{}", error.cause())
}

fn answered(outcome: Option<ApiResult<Outcome>>) -> ApiResult<Outcome> {
    outcome.unwrap_or_else(|| Err(HttpError::internal("a landing left unanswered")))
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
