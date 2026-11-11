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
//!
//! Where each part is: [`entry`] the format; [`log`] the log's Git I/O;
//! [`queue`] the leader, roots, holds and retries; [`check`] the check;
//! [`apply`] putting entries in place, and pushes a move waits for; [`void`]
//! voiding.

mod apply;
mod check;
mod entry;
mod log;
mod queue;
mod void;

use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex, MutexGuard};

use camino::Utf8PathBuf;

use crate::error::HttpError;

pub use apply::{Pushes, Pushing};
pub use entry::{
    Asked, Change, Entry, Expect, FORMAT, Landed, Landing, Made, Move, Placed, RefMove, Stamp,
};
use queue::Queue;
pub use queue::{Hold, Stuck};

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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn anyhow_of(error: HttpError) -> anyhow::Error {
    anyhow::anyhow!("{}", error.cause())
}
