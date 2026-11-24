//! Voiding a stuck landing: one compensating entry, appended.

use std::collections::BTreeSet;

use crate::App;
use crate::db;
use crate::error::{ApiResult, HttpError};

use super::Log;
use super::apply::placed_of;
use super::entry::{Change, Entry, Move, RefMove, Stamp, homes_of, stamps};
use super::log::{head, write_entries};
use super::queue::{Put, Stuck, settle};

impl Log {
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
    pub(super) fn to_void(&self, stamp: Stamp) -> ApiResult<(BTreeSet<String>, Vec<Stuck>)> {
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

    pub(super) fn write_void(
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
}
