//! The landing log's entry format: what one landing changes, as the log
//! keeps it, and what a lander asks for.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::db::List;

/// A landing's place in the log: 1 for the first, one more for each after.
pub type Stamp = u64;

/// The entry format this server writes and reads. An entry of a newer format
/// is refused, never guessed at; a field added later is defaulted when an
/// entry without it is read (#411's epochs and merge commit, #412's rooted
/// tips).
pub const FORMAT: u32 = 1;

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
    pub(super) fn roots(&self) -> BTreeSet<String> {
        self.homes.keys().cloned().collect()
    }

    /// Whether putting it in place copies or removes storage, which can be
    /// large: done by its own thread, holding its roots, not the log. Such a
    /// landing is in place before what it moved is removed where it was.
    pub(super) fn moves_storage(&self) -> bool {
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

pub(super) fn stamps(stamps: &[Stamp]) -> String {
    stamps
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn homes_of(entry: &Entry) -> String {
    entry.homes.keys().cloned().collect::<Vec<_>>().join(", ")
}
