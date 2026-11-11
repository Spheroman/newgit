//! The queue in front of the log: leader and followers, roots' locks and
//! holds, stuck landings and trying them again.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::App;
use crate::db;
use crate::error::{ApiResult, HttpError};

use super::apply::{Applied, Skipped, apply_entries, behind_of, finish, placed_of};
use super::check::{Seen, check};
use super::entry::{Asked, Entry, Landed, Landing, Stamp, homes_of, stamps};
use super::log::{count, head, write_entries};
use super::{Log, lock};

/// The most landings one batch writes.
pub(super) const BATCH: usize = 512;

/// How long a landing waits for a root another landing holds before it is
/// answered 503, by default.
pub(super) const ROOT_WAIT: Duration = Duration::from_secs(10);

/// What a batch's leader decided for one landing.
pub(super) enum Outcome {
    Landed(Landed),
    /// Written down; its own thread puts it in place, holding its roots
    /// (the leader took the ones named, for it).
    Apply(Stamp, Entry, Landed, BTreeSet<String>),
    /// Its roots have landings written down and not in place: its thread
    /// tries them again, then asks again.
    Stuck,
}

pub(super) struct Slot {
    pub(super) roots: BTreeSet<String>,
    pub(super) moves_storage: bool,
    /// Whose roots count as its own: its [`Hold`]'s, or its own.
    pub(super) token: u64,
    pub(super) landing: Mutex<Option<Landing>>,
    pub(super) outcome: Mutex<Option<ApiResult<Outcome>>>,
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
    pub(super) fn new(stamp: Stamp, entry: Entry) -> Self {
        Self {
            stamp,
            entry,
            cause: String::new(),
            placed: false,
        }
    }

    /// What a landing refused because of it is told.
    pub(super) fn refusal(&self) -> HttpError {
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

/// What putting one landing in place came to.
pub(super) enum Put {
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
pub(super) enum Mark {
    /// In place and finished, with every landing it voids.
    Done(Stamp, Vec<Stamp>),
    Stuck(Stuck),
    /// Nothing changes: a voided landing waits for its void.
    Nothing,
}

/// A landing in place, as its lander is told: the asked refs it found
/// moved, and why removing what it moved from where it was is pending.
pub(super) type Answered = (Vec<Skipped>, Option<String>);

/// What a landing put in place (or not) answers its lander, and what the
/// queue records for it. A landing in place has landed, finished or not.
pub(super) fn settle(stuck: Stuck, put: Put) -> (ApiResult<Answered>, Mark) {
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

pub(super) struct Queue {
    pub(super) waiting: VecDeque<Arc<Slot>>,
    pub(super) leading: bool,
    /// Roots held off the leader, by whom and why.
    pub(super) held: BTreeMap<String, (u64, String)>,
    /// Landings written down whose putting in place failed, oldest first.
    pub(super) stuck: Vec<Stuck>,
    /// The last stamp written, and its commit.
    pub(super) tip: Stamp,
    pub(super) tip_commit: String,
    /// Every landing up to here is in place, finished.
    pub(super) applied: Stamp,
    /// Landings in place after `applied`, with one before them not yet.
    pub(super) above: BTreeSet<Stamp>,
    pub(super) tokens: u64,
}

impl Queue {
    pub(super) fn stuck_roots(&self) -> BTreeSet<String> {
        self.stuck
            .iter()
            .flat_map(|stuck| stuck.entry.homes.keys().cloned())
            .collect()
    }

    /// Who else holds one of `roots`, and why: `None` when nobody but
    /// `token` does.
    pub(super) fn blocker(&self, roots: &BTreeSet<String>, token: u64) -> Option<String> {
        roots.iter().find_map(|root| match self.held.get(root) {
            Some((holder, why)) if *holder != token => Some(format!("{root}: {why}")),
            _ => None,
        })
    }

    pub(super) fn token(&mut self) -> u64 {
        self.tokens += 1;
        self.tokens
    }

    /// Hold each of `roots` nobody holds for `token`; returns those taken.
    pub(super) fn take(
        &mut self,
        roots: &BTreeSet<String>,
        token: u64,
        why: &str,
    ) -> BTreeSet<String> {
        let mut taken = BTreeSet::new();
        for root in roots {
            if !self.held.contains_key(root) {
                self.held.insert(root.clone(), (token, why.to_owned()));
                taken.insert(root.clone());
            }
        }
        taken
    }

    pub(super) fn release(&mut self, roots: &BTreeSet<String>, token: u64) {
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
    pub(super) fn mark(&mut self, stamp: Stamp) {
        self.stuck.retain(|stuck| stuck.stamp != stamp);
        if stamp <= self.applied {
            return;
        }
        self.above.insert(stamp);
        while self.above.remove(&(self.applied + 1)) {
            self.applied += 1;
        }
    }

    pub(super) fn stick(&mut self, stuck: Stuck) {
        self.stuck.retain(|known| known.stamp != stuck.stamp);
        self.stuck.push(stuck);
        self.stuck.sort_by_key(|stuck| stuck.stamp);
    }

    pub(super) fn record(&mut self, mark: Mark) {
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

/// Roots held ahead of a landing ([`Log::hold`]): no other landing touches
/// them until it is dropped.
pub struct Hold<'l> {
    pub(super) log: &'l Log,
    pub(super) token: u64,
    pub(super) roots: BTreeSet<String>,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let mut queue = self.log.lock();
        queue.release(&self.roots, self.token);
        self.log.changed.notify_all();
    }
}

/// What one batch came to.
#[derive(Default)]
pub(super) struct Processed {
    pub(super) outcomes: Vec<ApiResult<Outcome>>,
    /// How many were written.
    pub(super) written: Stamp,
    /// The log's commit after them.
    pub(super) tip_commit: Option<String>,
    /// Each written one put in place here, or stuck.
    pub(super) marks: Vec<Mark>,
}

/// How a landing that waited too long for a root is answered.
const BUSY: &str = "this project is busy";

/// Whether `error` is a root another landing held too long ([`busy`]), not a
/// stuck landing: waiting again is all it needs.
pub fn is_busy(error: &HttpError) -> bool {
    error.status == 503 && error.message.starts_with(BUSY)
}

pub(super) fn busy(blocker: Option<String>) -> HttpError {
    HttpError::new(
        503,
        format!(
            "{BUSY} ({}): try again in a moment",
            blocker.unwrap_or_else(|| "another landing holds it".to_owned())
        ),
    )
}

/// How a merge into `branch` that waited its whole turn budget is answered.
pub fn busy_merging(branch: &str) -> HttpError {
    busy(Some(format!(
        "merges into `{branch}` changing the same paths as this one kept landing first"
    )))
}

pub(super) fn answered(outcome: Option<ApiResult<Outcome>>) -> ApiResult<Outcome> {
    outcome.unwrap_or_else(|| Err(HttpError::internal("a landing left unanswered")))
}

impl Log {
    pub(super) fn lock(&self) -> MutexGuard<'_, Queue> {
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

    pub(super) fn root_wait(&self) -> Duration {
        Duration::from_millis(self.root_wait_ms.load(Ordering::SeqCst))
    }

    pub(super) fn armed(&self, at: &str) -> bool {
        self.faults.as_ref().is_some_and(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|faults| faults.lines().any(|fault| fault.trim() == at))
        })
    }

    /// Wait a moment here if the fault file says to: for the tests that
    /// land something between a merge and its recheck
    /// (`delay-merge-by-<login>`).
    pub fn pause(&self, at: &str) {
        if self.armed(at) {
            std::thread::sleep(Duration::from_millis(1500));
        }
    }

    /// Wait here while the fault file says to (at most a minute), with
    /// `<fault file>.at` naming where, for the tests that land something
    /// before a finish reads the branch (`hold-finish-by-<login>`).
    pub fn wait_while(&self, at: &str) {
        let Some(path) = self.faults.as_ref().filter(|_| self.armed(at)) else {
            return;
        };
        let marker = format!("{path}.at");
        let _ = std::fs::write(&marker, at);
        let until = std::time::Instant::now() + Duration::from_secs(60);
        while self.armed(at) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&marker);
    }

    /// Stop the process at `at` if the fault file says to: for crash tests
    /// outside the landing log (`rooted-checkpoint`).
    pub fn stop_at(&self, at: &str) {
        self.fault(at);
    }

    /// Stop the process here if the fault file says to.
    pub(super) fn fault(&self, at: &str) {
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

    pub(super) fn land_with(
        &self,
        app: &App,
        landing: Landing,
        token: Option<u64>,
    ) -> ApiResult<Landed> {
        let roots = landing.entry.roots();
        let landed = self.land_waiting(app, landing, token)?;
        // Derived data, after the landing and outside every lock it held:
        // its projects' stored rooted histories are extended in the
        // background (a read before that composes what it needs itself).
        app.rooted.touched(&roots);
        Ok(landed)
    }

    fn land_waiting(&self, app: &App, landing: Landing, token: Option<u64>) -> ApiResult<Landed> {
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
    pub(super) fn retry(&self, app: &App, roots: &BTreeSet<String>, token: u64) -> ApiResult<()> {
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
    pub(super) fn put_in_place(
        &self,
        app: &App,
        entries: &[Stuck],
        note: &dyn Fn(String),
    ) -> Vec<Put> {
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
    pub(super) fn lead<'q>(
        &'q self,
        app: &App,
        mut queue: MutexGuard<'q, Queue>,
    ) -> MutexGuard<'q, Queue> {
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
    pub(super) fn process(&self, app: &App, tip: Stamp, batch: &[Arc<Slot>]) -> Processed {
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
}
