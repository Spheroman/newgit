//! Stored rooted histories (#412; docs/design/landing.md, *Performance*, and
//! docs/design/serve.md, decision 10).
//!
//! A view rooted at a tracker (`/<owner>/<project>/<path>.git`) is that
//! tracker's history with each child it mounts expanded at every commit
//! (`newgit_core::compose`). Composed per fetch it costs the whole history
//! every time anything lands, so each tracker's is stored, in its own store,
//! and extended by composing only the commits that are new.
//!
//! **What is stored, and where.** For tracker `T` in store `S`, the view of
//! `T` composed from exactly the trackers mounted within it whose content
//! every reader of `S` may read: those in `S` itself and those in `public`
//! (`Store::includes`). That is *the store's view*: what anyone who reaches
//! `S` may read already, so writing it into `S` makes nothing readable that
//! was not. A tracker mounted within `T` in a narrower store (`user:jack`, a
//! private instance's) is never composed into it, nor is a linked tracker,
//! whose home is another directory. A reader whose readable set within `T`
//! is exactly that set is served the stored history; one who reads more (a
//! private tracker inside) is composed for per fetch, as before, since a
//! per-reader view of k private audiences can be 2^k histories (landing.md,
//! *Performance*), and composing those from cached subtrees is #440.
//!
//! **Layout in `S`.** Served refs live in the namespace `r/<T>`
//! (`refs/heads/*`, `refs/tags/*`, `refs/notes/commits` for canonical ids,
//! and `HEAD`), served with `git --namespace` exactly as a tracker's repo is.
//! What it was composed from is one ref outside every namespace,
//! `refs/newgit/rooted/<T>`, at a state commit no request can name a ref to:
//! its tree has `served` (each branch and tag: the commit of `T` it was
//! composed from, the readable set's key, and the view commit) and, for each
//! readable set, `lines/<key>/map/<ab>/<cd>/<rest>` (what each commit of `T`
//! composed to: [`Composition`]) and `lines/<key>/notes/…` (the canonical
//! notes, in Git's own notes layout). The state ref, every served ref, and
//! `HEAD` move in one ref transaction: a compare-and-swap against the state
//! its writer read, which also checks the tracker's own `HEAD` is still in
//! `S`, so a tracker that moved out meanwhile gets nothing written back.
//!
//! **Never back.** The state records when its lists were read ([`At`]: the
//! project's revision, which SQLite moves with every change to what views
//! are made of). A refresh from older lists than what is stored leaves it
//! alone, so once a fetch has brought it up to the lists it read, its refs
//! only move forward. "Tracker `T`'s rooted history as of landing `N`" is
//! [`as_of`].
//!
//! **When.** Derived data, never a state anyone sees (landing.md, *How we got
//! here*): nothing about it is in a landing's entry. Every read of a rooted
//! view compares what is stored with the lists it reads under the db lock,
//! and composes what is missing before it serves, so no reader ever sees a
//! rooted view behind what has landed. After each landing the trackers it
//! touched, and those they are mounted within, are extended in the
//! background ([`Worker`]), outside every lock a landing holds and with no
//! Git process under the db lock, so the read usually finds it done. A landing voided or
//! never put in place changed no list, so nothing was composed for it; a
//! crash mid-extension leaves objects nothing names and a state ref where it
//! was.
//!
//! **Existing data.** A data directory from before this has no stored
//! histories. They are composed on first request (the read above does that
//! anyway) and by a background pass at start, which never blocks serving:
//! startup on a home of 22k trackers must not wait for 22k histories. Long
//! histories are composed in chunks, each recorded in the state ref, so a
//! pass stopped part way resumes where it was.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Condvar, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions, Composition};
use newgit_core::endpoint::TagAnnotation;
use newgit_core::objects::{Entry, Objects, TREE_MODE, hash};
use rusqlite::Connection;

use crate::App;
use crate::api::ProjectContext;
use crate::db;
use crate::error::{ApiResult, HttpError};
use crate::landing::Stamp;
use crate::names::{ProjectId, RepoRef, namespace_prefix, parse_project_key, within};
use crate::policy::Facts;
use crate::repos::{self, Repo, Store};

/// Where a tracker's stored rooted history is served from, in its store.
pub fn namespace(tracker: &str) -> String {
    format!("r/{tracker}")
}

/// What a tracker's stored rooted history was composed from.
pub fn state_ref(tracker: &str) -> String {
    format!("refs/newgit/rooted/{tracker}")
}

const BLOB_MODE: &[u8] = b"100644";
const NOBODY: &[u8] = b"newgit <newgit@localhost> 946684800 +0000";

/// How many commits are composed before what they composed to is recorded:
/// a backfill stopped part way resumes from the last chunk.
fn chunk() -> usize {
    std::env::var("NEWGIT_SERVE_ROOTED_CHUNK")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(5000)
}

/// One branch or tag of the view: the commit of the rooted tracker it is,
/// and the trackers composed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub source: String,
    pub set: BTreeSet<String>,
    /// An annotated tag's annotation; `None` for a branch or a light tag.
    pub annotation: Option<TagAnnotation>,
}

/// When the lists a stored rooted history was composed from were read: which
/// project of its name (`db::ProjectInfo::incarnation`), the revision of
/// what its views are made of (`db::views_at`: every list, tag, link, store
/// row, and default branch change moves it), and the stamp of the last
/// landing in it. Within one incarnation a higher revision is newer; another
/// incarnation is another project. A refresh never replaces a stored history
/// with one read at an older revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct At {
    pub incarnation: String,
    pub revision: u64,
    pub landed: Stamp,
}

impl At {
    fn line(&self) -> String {
        let incarnation = if self.incarnation.is_empty() {
            "-"
        } else {
            &self.incarnation
        };
        format!("at {incarnation} {} {}\n", self.revision, self.landed)
    }

    /// What a state's `served` says it was read at (nothing, for a state
    /// from before this was recorded: older than anything).
    fn of(served: &str) -> Self {
        served
            .lines()
            .find_map(|line| {
                let mut fields = line.strip_prefix("at ")?.split(' ');
                let incarnation = fields.next()?;
                Some(Self {
                    incarnation: if incarnation == "-" {
                        String::new()
                    } else {
                        incarnation.to_owned()
                    },
                    revision: fields.next()?.parse().ok()?,
                    landed: fields.next()?.parse().ok()?,
                })
            })
            .unwrap_or_default()
    }

    /// Whether lists read at `self` are older than ones read at `stored`.
    pub fn behind(&self, stored: &Self) -> bool {
        self.incarnation == stored.incarnation && self.revision < stored.revision
    }
}

/// A tracker's stored rooted history as the lists have it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub home: Utf8PathBuf,
    pub tracker: String,
    pub store: Store,
    pub default_branch: String,
    pub heads: BTreeMap<String, Part>,
    pub tags: BTreeMap<String, Part>,
    /// The store of each tracker composed into it: its own, or `public`.
    pub stores: BTreeMap<String, Store>,
    /// When the lists it was read from were read.
    pub at: At,
    /// What the tracker's own `HEAD` in `store` names, as [`located`] found
    /// it: every write of the stored history checks it is still there in the
    /// same transaction, so a tracker whose move to another store lands
    /// meanwhile gets nothing written back into the store it left. `None`
    /// writes without that check (the tests that make a store by hand).
    pub own_head: Option<String>,
}

impl Want {
    pub fn dir(&self) -> Utf8PathBuf {
        self.store.dir(&self.home)
    }

    /// The repo readers are served: the namespace in `S`.
    pub fn repo(&self) -> Repo {
        Repo::new(&self.home, self.store.clone(), namespace(&self.tracker))
    }

    /// The tracker's own repo, in the same store.
    fn own(&self) -> Repo {
        Repo::tracker(&self.home, Some(self.store.clone()), &self.tracker)
    }

    /// Whether `other` is the same history, whenever its lists were read.
    fn same_as(&self, other: &Self) -> bool {
        self.home == other.home
            && self.tracker == other.tracker
            && self.store == other.store
            && self.default_branch == other.default_branch
            && self.heads == other.heads
            && self.tags == other.tags
            && self.stores == other.stores
    }

    /// What the state ref's `served` says when the history is this.
    fn served_text(&self, views: &BTreeMap<String, String>) -> String {
        let mut text = format!("default {}\n", self.default_branch);
        text.push_str(&self.at.line());
        for (kind, parts) in [("head", &self.heads), ("tag", &self.tags)] {
            for (name, part) in parts {
                let key = format!("{kind} {name}");
                text.push_str(&format!(
                    "{key} {} {} {} {}\n",
                    part.source,
                    set_key(&part.set),
                    annotation_key(part.annotation.as_ref()),
                    views.get(&key).map_or("-", String::as_str),
                ));
            }
        }
        text
    }
}

/// The trackers a readable set names, as a short key.
pub fn set_key(set: &BTreeSet<String>) -> String {
    let text: String = set.iter().map(|name| format!("{name}\n")).collect();
    hash("blob", text.as_bytes())[..16].to_owned()
}

fn annotation_key(annotation: Option<&TagAnnotation>) -> String {
    annotation.map_or_else(
        || "-".to_owned(),
        |annotation| {
            hash(
                "blob",
                format!("{}\n{}", annotation.tagger, annotation.message).as_bytes(),
            )[..16]
                .to_owned()
        },
    )
}

/// What one project's stored rooted histories are made of, read once under
/// the db lock and nothing else: its lists, tags, definitions, and rows.
/// Every [`Want`] is made from it with no process and no lock, and every Git
/// check ([`located`]) and composition runs after the lock is let go of.
pub struct Snapshot {
    pub id: ProjectId,
    pub home: Utf8PathBuf,
    pub at: At,
    default_branch: String,
    lists: Vec<(String, db::List)>,
    tags: Vec<(String, db::List, Option<TagAnnotation>)>,
    facts: BTreeMap<String, Facts>,
    links: BTreeSet<String>,
    /// The store each named tracker's row puts it in.
    rows: BTreeMap<String, Store>,
    /// For each tracker on the default branch: where it is, and the store
    /// its facts call for (`ProjectContext::placed_for`).
    places: BTreeMap<String, (Repo, Store)>,
}

impl Snapshot {
    /// Read under the db lock `db`, which `project` was opened under.
    pub fn read(project: &ProjectContext<'_>, db: &Connection) -> ApiResult<Self> {
        let lists: Vec<(String, db::List)> = db::branches(db, &project.key)?
            .iter()
            .map(|branch| {
                (
                    branch["name"].as_str().unwrap_or_default().to_owned(),
                    serde_json::from_value(branch["list"].clone()).unwrap_or_default(),
                )
            })
            .collect();
        let tags: Vec<(String, db::List, Option<TagAnnotation>)> = db::tags(db, &project.key)?
            .iter()
            .map(|tag| {
                (
                    tag["name"].as_str().unwrap_or_default().to_owned(),
                    serde_json::from_value(tag["list"].clone()).unwrap_or_default(),
                    serde_json::from_value(tag["annotation"].clone())
                        .ok()
                        .flatten(),
                )
            })
            .collect();
        let named: BTreeSet<String> = lists
            .iter()
            .flat_map(|(_, list)| list.keys())
            .chain(tags.iter().flat_map(|(_, list, _)| list.keys()))
            .cloned()
            .collect();
        let facts = project.definitions(db, named.iter().map(String::as_str))?;
        let rows = named
            .iter()
            .map(|name| (name.clone(), project.own(name).store))
            .collect();
        let default_branch = project.info.default_branch.clone();
        let mut places = BTreeMap::new();
        if let Some((_, list)) = lists.iter().find(|(name, _)| *name == default_branch) {
            for tracker in list.keys() {
                // On a branch, so decided when it has facts at all.
                let decided = facts.contains_key(tracker);
                places.insert(
                    tracker.clone(),
                    project.placed_for(
                        &RepoRef::tracker(&project.id, tracker),
                        facts.get(tracker),
                        false,
                        decided,
                    ),
                );
            }
        }
        let (revision, landed) = db::views_at(db, &project.key)?;
        Ok(Self {
            id: project.id.clone(),
            home: project.home(),
            at: At {
                incarnation: project.info.incarnation.clone(),
                revision,
                landed,
            },
            default_branch,
            lists,
            tags,
            facts,
            links: project.links.keys().cloned().collect(),
            rows,
            places,
        })
    }

    /// Every tracker that may have a stored rooted history: those on the
    /// default branch.
    pub fn trackers(&self) -> impl Iterator<Item = &String> {
        self.places.keys()
    }

    /// The store tracker `name`'s row puts it in, for a tracker named on a
    /// list or tag.
    pub fn row(&self, name: &str) -> Option<&Store> {
        self.rows.get(name)
    }

    /// Tracker `tracker`'s stored rooted history as these lists, tags,
    /// definitions, and rows have it. `None` when it has none: a link (its
    /// home is elsewhere), a tracker outside Git, one not on the default
    /// branch, or one not in the store its audience calls for (refused by
    /// `served`, whose row half this is; [`located`] is the rest).
    pub fn want(&self, tracker: &str) -> Option<Want> {
        if self.links.contains(tracker) {
            return None;
        }
        // A view's `HEAD` is the default branch, so one not on it is not
        // served (the composer refuses it too).
        let (location, expected) = self.places.get(tracker)?;
        if !self.facts.get(tracker).is_some_and(|fact| fact.git) || location.store != *expected {
            return None;
        }
        let store = location.store.clone();
        let mut stores = BTreeMap::new();
        let mut member = |name: &str| -> bool {
            if !within(name, tracker)
                || self.links.contains(name)
                || !self.facts.get(name).is_some_and(|fact| fact.git)
            {
                return false;
            }
            let there = self
                .rows
                .get(name)
                .cloned()
                .unwrap_or_else(|| Store::Tracker(name.to_owned()));
            if there == store || there == Store::Public {
                stores.insert(name.to_owned(), there);
                true
            } else {
                false
            }
        };
        let mut cut = |list: &db::List| -> BTreeSet<String> {
            list.keys()
                .filter(|name| member(name))
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let mut heads = BTreeMap::new();
        for (name, list) in &self.lists {
            if let Some(source) = list.get(tracker) {
                heads.insert(
                    name.clone(),
                    Part {
                        source: source.clone(),
                        set: cut(list),
                        annotation: None,
                    },
                );
            }
        }
        let mut tags = BTreeMap::new();
        for (name, list, annotation) in &self.tags {
            let Some(source) = list.get(tracker) else {
                continue;
            };
            tags.insert(
                name.clone(),
                Part {
                    source: source.clone(),
                    set: cut(list),
                    annotation: annotation.clone(),
                },
            );
        }
        Some(Want {
            home: self.home.clone(),
            tracker: tracker.to_owned(),
            store,
            default_branch: self.default_branch.clone(),
            heads,
            tags,
            stores,
            at: self.at.clone(),
            own_head: None,
        })
    }
}

/// Tracker `tracker` of `project`'s stored rooted history, as its lists,
/// tags, definitions, and store rows have it under the db lock `db`: no
/// process runs. [`located`] then checks it, outside the lock.
pub fn wanted(
    project: &ProjectContext<'_>,
    db: &Connection,
    tracker: &str,
) -> ApiResult<Option<Want>> {
    Ok(Snapshot::read(project, db)?.want(tracker))
}

/// `served`'s Git half for `want`'s tracker, run outside the db lock: its
/// store is made, for its audience, and the tracker's repo is there. Records
/// what the tracker's `HEAD` there names ([`Want::own_head`]). `false` when
/// it is not: nothing is served from it.
pub fn located(want: &mut Want, id: &ProjectId) -> ApiResult<bool> {
    let own = want.own();
    let Some(head) = symbolic(&want.dir(), &own.full("HEAD"))? else {
        return Ok(false);
    };
    let repo = RepoRef::tracker(id, &want.tracker);
    if crate::api::served_from(&repo, own, &want.store, true).is_err() {
        return Ok(false);
    }
    want.own_head = Some(head);
    Ok(true)
}

/// What symbolic ref `name` in the store at `dir` names, if it is one.
fn symbolic(dir: &Utf8Path, name: &str) -> ApiResult<Option<String>> {
    if !repos::store_exists(dir) {
        return Ok(None);
    }
    let output = repos::git(dir, &["symbolic-ref", "--quiet", name])?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

/// What [`refresh`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refreshed {
    /// Nothing was missing.
    pub fresh: bool,
    /// Commits of the tracker composed now.
    pub composed: usize,
    /// What is stored was composed from lists read after these, and differs:
    /// left as it is, never moved back to them. What it serves is at least
    /// as new as what was asked.
    pub newer: bool,
    /// The tracker is no longer in this store (its move landed meanwhile):
    /// nothing was written here.
    pub gone: bool,
}

/// The state ref's commit, its `served` text, and the tree of each line.
struct State {
    commit: Option<String>,
    served: String,
    lines: BTreeMap<String, String>,
}

fn read_state(objects: &mut Objects, dir: &Utf8Path, tracker: &str) -> ApiResult<State> {
    let reference = format!("{}^{{commit}}", state_ref(tracker));
    let output = repos::git(dir, &["rev-parse", "--verify", "--quiet", &reference])?;
    let commit = output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let mut state = State {
        commit: commit.clone(),
        served: String::new(),
        lines: BTreeMap::new(),
    };
    let Some(commit) = commit else {
        return Ok(state);
    };
    let tree = objects.commit(&commit).map_err(core)?.tree;
    if let Some(entry) = objects.entry_at(&tree, "served").map_err(core)? {
        state.served = String::from_utf8_lossy(&objects.blob(&entry.id).map_err(core)?).into();
    }
    if let Some(lines) = objects.subtree(&tree, "lines").map_err(core)? {
        for entry in objects.tree(&lines).map_err(core)? {
            if entry.is_tree() {
                state
                    .lines
                    .insert(String::from_utf8_lossy(&entry.name).into_owned(), entry.id);
            }
        }
    }
    Ok(state)
}

fn core(error: newgit_core::error::NewgitError) -> HttpError {
    HttpError::internal(error)
}

/// `served` text without the view commits: what the history was composed
/// from, to compare with what is wanted.
fn sources_of(served: &str) -> Vec<String> {
    served
        .lines()
        .filter(|line| !line.starts_with("at "))
        .map(|line| {
            let mut fields: Vec<&str> = line.split(' ').collect();
            if fields.first().is_some_and(|kind| *kind != "default") {
                fields.pop();
            }
            fields.join(" ")
        })
        .collect()
}

/// Each branch and tag of `served`: `(kind name, source, set key, view)`.
fn served_parts(served: &str) -> Vec<(String, String, String, String)> {
    served
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            match fields.as_slice() {
                [kind, name, source, key, _annotation, view] if *kind != "default" => Some((
                    format!("{kind} {name}"),
                    (*source).to_owned(),
                    (*key).to_owned(),
                    (*view).to_owned(),
                )),
                _ => None,
            }
        })
        .collect()
}

/// A commit id's path in a fanned-out tree: `ab/cd/<rest>`.
fn fanout(id: &str) -> String {
    format!("{}/{}/{}", &id[..2], &id[2..4], &id[4..])
}

/// `tree` (or nothing) with each of `adds` (a commit id → a blob) put at its
/// fanned-out path, every touched subtree written once.
fn insert_fanout(
    objects: &mut Objects,
    tree: Option<&str>,
    adds: &BTreeMap<String, String>,
) -> ApiResult<Option<String>> {
    if adds.is_empty() {
        return Ok(tree.map(ToOwned::to_owned));
    }
    // first → second → (rest, blob)
    type Leaves<'a> = Vec<(&'a str, &'a String)>;
    let mut grouped: BTreeMap<&str, BTreeMap<&str, Leaves<'_>>> = BTreeMap::new();
    for (id, blob) in adds {
        grouped
            .entry(&id[..2])
            .or_default()
            .entry(&id[2..4])
            .or_default()
            .push((&id[4..], blob));
    }
    let entries_of = |objects: &mut Objects, tree: Option<&str>| -> ApiResult<Vec<Entry>> {
        match tree {
            Some(tree) => objects.tree(tree).map_err(core),
            None => Ok(Vec::new()),
        }
    };
    let mut top = entries_of(objects, tree)?;
    for (first, seconds) in grouped {
        let first_tree = top
            .iter()
            .find(|entry| entry.name == first.as_bytes() && entry.is_tree())
            .map(|entry| entry.id.clone());
        let mut middle = entries_of(objects, first_tree.as_deref())?;
        for (second, leaves) in seconds {
            let second_tree = middle
                .iter()
                .find(|entry| entry.name == second.as_bytes() && entry.is_tree())
                .map(|entry| entry.id.clone());
            let mut bottom = entries_of(objects, second_tree.as_deref())?;
            for (rest, blob) in leaves {
                bottom.retain(|entry| entry.name != rest.as_bytes());
                bottom.push(Entry {
                    mode: BLOB_MODE.to_vec(),
                    name: rest.as_bytes().to_vec(),
                    id: blob.clone(),
                });
            }
            let made = objects.make_tree(bottom);
            middle.retain(|entry| entry.name != second.as_bytes());
            middle.push(Entry {
                mode: TREE_MODE.to_vec(),
                name: second.as_bytes().to_vec(),
                id: made,
            });
        }
        let made = objects.make_tree(middle);
        top.retain(|entry| entry.name != first.as_bytes());
        top.push(Entry {
            mode: TREE_MODE.to_vec(),
            name: first.as_bytes().to_vec(),
            id: made,
        });
    }
    Ok(Some(objects.make_tree(top)))
}

fn encode(found: &Composition) -> Vec<u8> {
    let mut text = format!("view {}\nexpanded", found.view);
    for child in &found.expanded {
        text.push(' ');
        text.push_str(child);
    }
    text.push('\n');
    if let Some(original) = &found.canonical {
        text.push_str(&format!("canonical {original}\n"));
    }
    text.into_bytes()
}

fn decode(body: &[u8]) -> Option<Composition> {
    let text = std::str::from_utf8(body).ok()?;
    let mut found = Composition {
        view: String::new(),
        expanded: BTreeSet::new(),
        canonical: None,
    };
    for line in text.lines() {
        if let Some(view) = line.strip_prefix("view ") {
            found.view = view.to_owned();
        } else if let Some(children) = line.strip_prefix("expanded") {
            found.expanded = children.split_whitespace().map(str::to_owned).collect();
        } else if let Some(original) = line.strip_prefix("canonical ") {
            found.canonical = Some(original.to_owned());
        }
    }
    (found.view.len() == 40).then_some(found)
}

/// One readable set's history while it is extended.
struct Line {
    set: BTreeSet<String>,
    /// Its tree in the state, before this extension.
    tree: Option<String>,
    /// What this extension composed, not yet in a tree.
    fresh: HashMap<String, Composition>,
    /// Map and notes as last recorded.
    map: Option<String>,
    notes: Option<String>,
}

impl Line {
    fn lookup(&mut self, objects: &mut Objects, commit: &str) -> ApiResult<Option<Composition>> {
        if let Some(found) = self.fresh.get(commit) {
            return Ok(Some(found.clone()));
        }
        let Some(map) = &self.map else {
            return Ok(None);
        };
        let Some(entry) = objects.entry_at(map, fanout(commit)).map_err(core)? else {
            return Ok(None);
        };
        Ok(decode(&objects.blob(&entry.id).map_err(core)?))
    }

    /// Record what was composed into the map and notes trees.
    fn record(&mut self, objects: &mut Objects) -> ApiResult<()> {
        let mut map_adds = BTreeMap::new();
        let mut note_adds = BTreeMap::new();
        for (commit, found) in self.fresh.drain() {
            map_adds.insert(commit, objects.make_blob(&encode(&found)));
            if let Some(original) = &found.canonical {
                note_adds.insert(
                    found.view.clone(),
                    objects.make_blob(format!("canonical {original}\n").as_bytes()),
                );
            }
        }
        self.map = insert_fanout(objects, self.map.as_deref(), &map_adds)?;
        self.notes = insert_fanout(objects, self.notes.as_deref(), &note_adds)?;
        Ok(())
    }

    fn tree(&self, objects: &mut Objects) -> String {
        let text: String = self.set.iter().map(|name| format!("{name}\n")).collect();
        let set = objects.make_blob(text.as_bytes());
        let mut entries = vec![Entry {
            mode: BLOB_MODE.to_vec(),
            name: b"set".to_vec(),
            id: set,
        }];
        for (name, tree) in [("map", &self.map), ("notes", &self.notes)] {
            if let Some(tree) = tree {
                entries.push(Entry {
                    mode: TREE_MODE.to_vec(),
                    name: name.as_bytes().to_vec(),
                    id: tree.clone(),
                });
            }
        }
        objects.make_tree(entries)
    }
}

fn state_commit(objects: &mut Objects, served: &str, lines: &BTreeMap<String, String>) -> String {
    let mut line_entries = Vec::new();
    for (key, tree) in lines {
        line_entries.push(Entry {
            mode: TREE_MODE.to_vec(),
            name: key.as_bytes().to_vec(),
            id: tree.clone(),
        });
    }
    let lines_tree = objects.make_tree(line_entries);
    let served = objects.make_blob(served.as_bytes());
    let tree = objects.make_tree(vec![
        Entry {
            mode: BLOB_MODE.to_vec(),
            name: b"served".to_vec(),
            id: served,
        },
        Entry {
            mode: TREE_MODE.to_vec(),
            name: b"lines".to_vec(),
            id: lines_tree,
        },
    ]);
    objects.make_commit(&tree, &[], NOBODY, NOBODY, b"", b"rooted history\n")
}

/// Move the state ref from `old` (the state this writer read: `None`, none)
/// to `new`, and every served ref of `want`'s namespace in `refs` (a full
/// name → its object, `None` deleted), and point the namespace's `HEAD` at
/// the default branch, in one transaction: a compare-and-swap on the state
/// ref, which also checks the tracker's own `HEAD` is still in this store
/// ([`Want::own_head`]). `false` when another writer moved the state first,
/// or the tracker left: nothing moved.
fn write_refs(
    want: &Want,
    old: Option<&str>,
    new: &str,
    refs: &BTreeMap<String, Option<String>>,
) -> ApiResult<bool> {
    let zero = "0".repeat(40);
    let repo = want.repo();
    let mut script = String::from("start\n");
    script.push_str(&format!(
        "update {} {new} {}\n",
        state_ref(&want.tracker),
        old.unwrap_or(&zero)
    ));
    if let Some(head) = &want.own_head {
        script.push_str(&format!(
            "symref-verify {} {head}\n",
            want.own().full("HEAD")
        ));
    }
    for (name, object) in refs {
        match object {
            Some(object) => script.push_str(&format!("update {name} {object}\n")),
            None => script.push_str(&format!("delete {name}\n")),
        }
    }
    script.push_str(&format!(
        "symref-update {} {}\n",
        repo.full("HEAD"),
        repo.full(&format!("refs/heads/{}", want.default_branch))
    ));
    script.push_str("prepare\ncommit\n");
    let dir = want.dir();
    if repos::run_stdin(&dir, &["update-ref", "--no-deref", "--stdin"], &script)? {
        return Ok(true);
    }
    // Lost to another writer, or the tracker left: either way the state or
    // the tracker's `HEAD` is not what this read. Anything else is a fault.
    let mut objects = Objects::open(&dir).map_err(core)?;
    let now = read_state(&mut objects, &dir, &want.tracker)?.commit;
    let moved = match &want.own_head {
        Some(head) => symbolic(&dir, &want.own().full("HEAD"))?.as_deref() != Some(head.as_str()),
        None => false,
    };
    if now.as_deref() != old || moved {
        return Ok(false);
    }
    Err(HttpError::internal(format!(
        "moving `{}`'s stored rooted history in {dir}",
        want.tracker
    )))
}

/// After a lost [`write_refs`]: the tracker left (gone), or another writer
/// moved the state first (try again: [`refresh_with`] reads it anew).
fn lost(want: &Want) -> ApiResult<Option<Refreshed>> {
    let gone = match &want.own_head {
        Some(head) => {
            symbolic(&want.dir(), &want.own().full("HEAD"))?.as_deref() != Some(head.as_str())
        }
        None => false,
    };
    Ok(gone.then(|| Refreshed {
        gone: true,
        ..Refreshed::default()
    }))
}

/// How many times a refresh that lost its swap to another writer reads the
/// state again and tries again.
const ATTEMPTS: usize = 4;

/// Bring `want`'s stored rooted history up to the lists: compose only the
/// commits not composed yet, and move its refs. Never moves it back: when
/// what is stored was composed from newer lists, it is left as it is. Run
/// holding the project's compose lock, never the db lock or a landing's.
pub fn refresh(want: &Want) -> ApiResult<Refreshed> {
    refresh_with(want, chunk(), &|| {}, &|| {})
}

/// [`refresh`], recording what is composed every `chunk` commits and
/// calling `checkpoint` after each (the crash test stops there) and
/// `between` (the worker lets go of the compose lock there, and takes it
/// again).
pub fn refresh_with(
    want: &Want,
    chunk: usize,
    checkpoint: &dyn Fn(),
    between: &dyn Fn(),
) -> ApiResult<Refreshed> {
    for _ in 0..ATTEMPTS {
        if let Some(done) = refresh_once(want, chunk, checkpoint, between)? {
            return Ok(done);
        }
    }
    Err(HttpError::internal(format!(
        "`{}`'s stored rooted history kept moving while it was extended",
        want.tracker
    )))
}

/// One try of [`refresh_with`]: `None` when another writer moved the state
/// first.
fn refresh_once(
    want: &Want,
    chunk: usize,
    checkpoint: &dyn Fn(),
    between: &dyn Fn(),
) -> ApiResult<Option<Refreshed>> {
    let chunk = chunk.max(1);
    let dir = want.dir();
    let mut objects = Objects::open(&dir).map_err(core)?;
    let state = read_state(&mut objects, &dir, &want.tracker)?;
    let stored_at = At::of(&state.served);
    let same = !state.served.is_empty()
        && sources_of(&state.served) == sources_of(&want.served_text(&BTreeMap::new()));
    if state.commit.is_some() && want.at.behind(&stored_at) {
        // Composed from newer lists than these: never moved back.
        return Ok(Some(Refreshed {
            fresh: same,
            newer: !same,
            ..Refreshed::default()
        }));
    }
    if same {
        if stored_at != want.at {
            // The same history, read later: say so, so that a writer that
            // read the lists between the two cannot move it back.
            let served = replace_at(&state.served, &want.at);
            let commit = state_commit(&mut objects, &served, &state.lines);
            objects.flush().map_err(core)?;
            if !write_refs(want, state.commit.as_deref(), &commit, &BTreeMap::new())? {
                return lost(want);
            }
        }
        return Ok(Some(Refreshed {
            fresh: true,
            ..Refreshed::default()
        }));
    }
    let mut current = state.commit.clone();
    let mut recorded = state.lines.clone();
    if !want.heads.contains_key(&want.default_branch) {
        return Err(HttpError::internal(format!(
            "`{}` is not on the default branch `{}`",
            want.tracker, want.default_branch
        )));
    }
    let old_parts = served_parts(&state.served);

    // Every readable set the branches and tags compose, with what each
    // composed last time.
    let mut by_key: BTreeMap<String, Line> = BTreeMap::new();
    for part in want.heads.values().chain(want.tags.values()) {
        let key = set_key(&part.set);
        if by_key.contains_key(&key) {
            continue;
        }
        let tree = state.lines.get(&key).cloned();
        let (map, notes) = match &tree {
            Some(tree) => (
                objects.subtree(tree, "map").map_err(core)?,
                objects.subtree(tree, "notes").map_err(core)?,
            ),
            None => (None, None),
        };
        by_key.insert(
            key,
            Line {
                set: part.set.clone(),
                tree,
                fresh: HashMap::new(),
                map,
                notes,
            },
        );
    }

    let public = Store::Public.dir(&want.home);
    let mut composed = 0;
    let keys: Vec<String> = by_key.keys().cloned().collect();
    for key in &keys {
        let tips: Vec<String> = want
            .heads
            .values()
            .chain(want.tags.values())
            .filter(|part| set_key(&part.set) == *key)
            .map(|part| part.source.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // What this set composed last time: all of it is in the map.
        let stops: Vec<String> = if by_key[key].tree.is_some() {
            old_parts
                .iter()
                .filter(|(_, _, old_key, _)| old_key == key)
                .map(|(_, source, _, _)| source.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let walk = compose::walk_of(&dir, &tips, &stops).map_err(core)?;
        let set = by_key[key].set.clone();
        let publics: BTreeSet<String> = set
            .iter()
            .filter(|name| want.stores.get(*name) == Some(&Store::Public))
            .cloned()
            .collect();
        let from_public = want.store != Store::Public && !publics.is_empty();
        for part in walk.chunks(chunk) {
            // Commits the map has already (a chunk recorded before a stop,
            // or a branch since deleted) are not composed again.
            let mut unknown = Vec::new();
            for commit in part {
                let line = by_key.get_mut(key).expect("made above");
                if line.lookup(&mut objects, commit)?.is_none() {
                    unknown.push(commit.clone());
                }
            }
            if from_public && !unknown.is_empty() {
                bring_public(&dir, &public, &want.tracker, &publics, &set, &unknown)?;
            }
            // A pin is expanded only if its commit is in that tracker's own
            // store: the stored history's own, or `public` for a public one.
            let public_objects = if from_public {
                Some(std::cell::RefCell::new(
                    Objects::open(&public).map_err(core)?,
                ))
            } else {
                None
            };
            let admit = |tracker: &str, commit: &str| -> bool {
                match (&public_objects, publics.contains(tracker)) {
                    (Some(objects), true) => {
                        objects.borrow_mut().has_commit(commit).unwrap_or(false)
                    }
                    _ => true,
                }
            };
            let readable = |name: &str| set.contains(name);
            let options = ComposeOptions {
                readable: &readable,
                admit: Some(&admit),
            };
            let line = by_key.get_mut(key).expect("made above");
            let mut reader = Objects::open(&dir).map_err(core)?;
            let mut known = |commit: &str| {
                line.lookup(&mut reader, commit)
                    .map_err(|error| newgit_core::error::NewgitError::Unsupported(error.message))
            };
            let made =
                compose::extend(&dir, &want.tracker, part, &mut known, &options).map_err(core)?;
            for (commit, found) in made {
                if unknown.contains(&commit) {
                    composed += 1;
                    line.fresh.insert(commit, found);
                }
            }
            if part.len() == chunk {
                // A long history: record this chunk, so a stop resumes here.
                line.record(&mut objects)?;
                let mut lines = recorded.clone();
                lines.insert(key.clone(), line.tree(&mut objects));
                recorded = lines.clone();
                let commit = state_commit(&mut objects, &state.served, &lines);
                objects.flush().map_err(core)?;
                if !write_refs(want, current.as_deref(), &commit, &BTreeMap::new())? {
                    return lost(want);
                }
                current = Some(commit);
                checkpoint();
                // The project's compose lock is let go of between chunks,
                // so a fetch of another view there waits for one chunk, not
                // for the whole of a long history.
                between();
            }
        }
    }

    // Every branch's and tag's view commit.
    let mut views = BTreeMap::new();
    let mut refs = BTreeMap::new();
    let repo = want.repo();
    for (kind, parts) in [("head", &want.heads), ("tag", &want.tags)] {
        for (name, part) in parts {
            let key = set_key(&part.set);
            let line = by_key.get_mut(&key).expect("every set has a line");
            let found = line.lookup(&mut objects, &part.source)?.ok_or_else(|| {
                HttpError::internal(format!(
                    "{} of `{}` was not composed",
                    &part.source, want.tracker
                ))
            })?;
            let label = format!("{kind} {name}");
            let object = match (&part.annotation, kind) {
                (Some(annotation), "tag") => {
                    objects.flush().map_err(core)?;
                    newgit_core::endpoint::make_tag_object(&dir, &found.view, name, annotation)
                        .map_err(core)?
                }
                _ => found.view.clone(),
            };
            let reference = if kind == "head" {
                repo.full(&format!("refs/heads/{name}"))
            } else {
                repo.full(&format!("refs/tags/{name}"))
            };
            views.insert(label, object.clone());
            refs.insert(reference, Some(object));
        }
    }
    // Branches and tags gone since.
    for (label, ..) in &old_parts {
        if views.contains_key(label) {
            continue;
        }
        if let Some((kind, name)) = label.split_once(' ') {
            let reference = if kind == "head" {
                repo.full(&format!("refs/heads/{name}"))
            } else {
                repo.full(&format!("refs/tags/{name}"))
            };
            refs.insert(reference, None);
        }
    }

    // The canonical notes of every set a branch composes.
    let mut lines = BTreeMap::new();
    let mut notes: Option<String> = None;
    let head_keys: BTreeSet<String> = want.heads.values().map(|part| set_key(&part.set)).collect();
    for (key, line) in &mut by_key {
        line.record(&mut objects)?;
        if head_keys.contains(key)
            && let Some(tree) = &line.notes
        {
            notes = Some(match notes {
                Some(so_far) => objects.overlay(&so_far, tree).map_err(core)?,
                None => tree.clone(),
            });
        }
        lines.insert(key.clone(), line.tree(&mut objects));
    }
    let notes_ref = repo.full("refs/notes/commits");
    match notes {
        Some(tree) => {
            let commit = objects.make_commit(&tree, &[], NOBODY, NOBODY, b"", b"canonical\n");
            refs.insert(notes_ref, Some(commit));
        }
        None => {
            refs.insert(notes_ref, None);
        }
    }
    let served = want.served_text(&views);
    let commit = state_commit(&mut objects, &served, &lines);
    objects.flush().map_err(core)?;
    // Deleting a ref that is not there fails the transaction.
    let present: BTreeSet<String> = repo
        .list_refs("refs/")?
        .into_iter()
        .map(|(reference, _)| repo.full(&reference))
        .collect();
    refs.retain(|reference, object| object.is_some() || present.contains(reference));
    if !write_refs(want, current.as_deref(), &commit, &refs)? {
        return lost(want);
    }
    Ok(Some(Refreshed {
        composed,
        ..Refreshed::default()
    }))
}

/// `served` with its `at` line saying `at`.
fn replace_at(served: &str, at: &At) -> String {
    let mut text = String::new();
    let mut placed = false;
    for line in served.lines() {
        if line.starts_with("at ") {
            continue;
        }
        text.push_str(line);
        text.push('\n');
        if !placed && line.starts_with("default ") {
            text.push_str(&at.line());
            placed = true;
        }
    }
    if !placed {
        text.insert_str(0, &at.line());
    }
    text
}

/// Fetch into the store at `dir`, from `public`, every commit of a public
/// tracker in `set` that `commits` (the rooted tracker's) pin, and what
/// those pin in turn, that `dir` does not hold: their content is public, so
/// any store may hold it.
fn bring_public(
    dir: &Utf8Path,
    public: &Utf8Path,
    root: &str,
    publics: &BTreeSet<String>,
    set: &BTreeSet<String>,
    commits: &[String],
) -> ApiResult<()> {
    let mut level: Vec<(String, String)> = commits
        .iter()
        .map(|commit| (root.to_owned(), commit.clone()))
        .collect();
    let mut seen = BTreeSet::new();
    let mut objects = Objects::open(dir).map_err(core)?;
    while !level.is_empty() {
        let mut next = Vec::new();
        let mut missing = Vec::new();
        for (tracker, commit) in &level {
            let Some(found) = objects.try_commit(commit).map_err(core)? else {
                continue;
            };
            for (child, pin) in compose::pins_in(&mut objects, &found.tree).map_err(core)? {
                let child = newgit_core::tracker::child_id(tracker, &child);
                if !set.contains(&child) || !seen.insert((child.clone(), pin.clone())) {
                    continue;
                }
                if publics.contains(&child) && !objects.has_commit(&pin).map_err(core)? {
                    missing.push(pin.clone());
                }
                next.push((child, pin));
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            let present: Vec<String> = missing
                .into_iter()
                .filter(|commit| repos::has_commit(public, commit))
                .collect();
            if !present.is_empty() {
                let mut args = vec![
                    "-c",
                    "protocol.version=2",
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    public.as_str(),
                ];
                args.extend(present.iter().map(String::as_str));
                let output = repos::git(dir, &args)?;
                if !output.status.success() {
                    return Err(HttpError::internal(format!(
                        "copying public commits into {dir}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
            // A fresh batch sees what was just fetched.
            objects = Objects::open(dir).map_err(core)?;
        }
        level = next;
    }
    Ok(())
}

/// What `commit` of `want`'s tracker composed to in its stored history, as
/// its state records it for the readable set `set`: for the tests that
/// compare every commit with a composition from scratch.
pub fn stored_composition(
    want: &Want,
    set: &BTreeSet<String>,
    commit: &str,
) -> ApiResult<Option<Composition>> {
    let dir = want.dir();
    let mut objects = Objects::open(&dir).map_err(core)?;
    let state = read_state(&mut objects, &dir, &want.tracker)?;
    let Some(tree) = state.lines.get(&set_key(set)) else {
        return Ok(None);
    };
    let mut line = Line {
        set: set.clone(),
        tree: Some(tree.clone()),
        fresh: HashMap::new(),
        map: objects.subtree(tree, "map").map_err(core)?,
        notes: None,
    };
    line.lookup(&mut objects, commit)
}

// ── Keeping them up to date ───────────────────────────────────────────────

/// What a landing touched in one project: the trackers whose stored rooted
/// histories it may change are these and every tracker they are mounted
/// within.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touched {
    /// Anything: definitions changed (every readable set may have), the
    /// project was made, a branch's old list is not known, or a pass over
    /// everything (the backfill).
    All,
    Some(BTreeSet<String>),
}

impl Touched {
    /// Whether tracker `tracker`'s stored rooted history may have changed.
    pub fn reaches(&self, tracker: &str) -> bool {
        match self {
            Self::All => true,
            Self::Some(names) => names.iter().any(|name| within(name, tracker)),
        }
    }

    /// Both: what two queued entries for one project coalesce to.
    pub fn and(&mut self, other: Self) {
        match (&mut *self, other) {
            (Self::All, _) => {}
            (_, Self::All) => *self = Self::All,
            (Self::Some(mine), Self::Some(theirs)) => mine.extend(theirs),
        }
    }
}

/// What `landing` touches, by project, worked out from the landing alone
/// (no process, no lock): for each branch it sets, the trackers whose pins
/// differ from the list it expects there; every tracker it moves between
/// stores or whose refs it moves. Anything it cannot say that of (the
/// project made or removed, definitions changed, a void, a branch whose old
/// list it does not name) touches everything there.
pub fn touched_by(landing: &crate::landing::Landing) -> BTreeMap<String, Touched> {
    let entry = &landing.entry;
    let mut out = BTreeMap::new();
    for (home, change) in &entry.homes {
        let all = !entry.voids.is_empty()
            || change.made.is_some()
            || change.removed
            || change.whole
            || change.epoch.is_some()
            || !change.unmoves.is_empty();
        if all {
            out.insert(home.clone(), Touched::All);
            continue;
        }
        let mut names: BTreeSet<String> = change
            .stores
            .keys()
            .chain(change.refs.keys())
            .chain(change.takes.keys())
            .cloned()
            .collect();
        let mut known = true;
        for (branch, list) in &change.branches {
            let Some(expect) = landing
                .expect
                .iter()
                .find(|expect| expect.home == *home && expect.branch == *branch)
            else {
                known = false;
                break;
            };
            let before = expect.list.clone().unwrap_or_default();
            for (name, pin) in list {
                if before.get(name) != Some(pin) {
                    names.insert(name.clone());
                }
            }
            names.extend(
                before
                    .keys()
                    .filter(|name| !list.contains_key(*name))
                    .cloned(),
            );
        }
        out.insert(
            home.clone(),
            if known {
                Touched::Some(names)
            } else {
                Touched::All
            },
        );
    }
    out
}

/// A refresh that failed, waiting to be tried again.
#[derive(Debug, Clone)]
pub struct Failing {
    pub home: String,
    /// What is tried again.
    pub touched: Touched,
    /// Why it failed last, as the server's log says it.
    pub error: String,
    pub attempts: u32,
    due: std::time::Instant,
}

#[derive(Default)]
struct Queue {
    /// Projects to refresh, in the order they were touched, each once.
    order: VecDeque<String>,
    /// What each queued project's landings touched, coalesced.
    touched: HashMap<String, Touched>,
    /// Projects whose last refresh failed, tried again when due.
    failing: BTreeMap<String, Failing>,
}

/// The longest a failed refresh waits to be tried again.
const MOST_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

fn first_backoff() -> std::time::Duration {
    std::env::var("NEWGIT_SERVE_ROOTED_RETRY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis,
        )
}

/// Extends stored rooted histories in the background: after each landing,
/// for the trackers it touched and those they are mounted within, and once
/// at start for every project (the backfill of a data directory from before
/// them). One thread. Each project's lists are read once per pass, under the
/// db lock and with no process; every Git process runs after it is let go
/// of, each tracker under its project's compose lock, let go of between
/// chunks of a long history, so a fetch that needs it waits for at most one
/// chunk. A refresh that fails (or panics) is logged, kept in
/// [`Worker::failing`], and tried again with backoff without waiting for
/// another landing; the thread keeps going.
#[derive(Default)]
pub struct Worker {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// What each tracker was last brought up to here, by `(home, tracker)`:
    /// one unchanged since is not refreshed again.
    done: Mutex<HashMap<(String, String), Want>>,
    /// Trackers refreshed (failures not counted), for the tests that wait
    /// on the backfill.
    pub refreshed: std::sync::atomic::AtomicU64,
    /// Refreshes that failed or panicked.
    pub failed: std::sync::atomic::AtomicU64,
    /// Passes over a project done, failed or not.
    pub passes: std::sync::atomic::AtomicU64,
    /// The background pass at start has looked at every project.
    pub backfilled: std::sync::atomic::AtomicBool,
}

impl Worker {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Landings touched `touched`, by project: their stored rooted histories
    /// may need extending. Entries for a project already queued coalesce.
    pub fn touched(&self, touched: BTreeMap<String, Touched>) {
        let mut queue = self.queue();
        for (home, what) in touched {
            match queue.touched.get_mut(&home) {
                Some(queued) => queued.and(what),
                None => {
                    queue.order.push_back(home.clone());
                    queue.touched.insert(home, what);
                }
            }
        }
        self.wake.notify_all();
    }

    /// The next project to refresh and what to refresh there: a queued one,
    /// or a failing one whose retry is due, waiting until there is one.
    fn next(&self) -> (String, Touched) {
        let mut queue = self.queue();
        loop {
            if let Some(home) = queue.order.pop_front() {
                let mut what = queue.touched.remove(&home).unwrap_or(Touched::All);
                // A failing retry there rides along.
                if let Some(failing) = queue.failing.get(&home) {
                    what.and(failing.touched.clone());
                }
                return (home, what);
            }
            let now = std::time::Instant::now();
            let due = queue
                .failing
                .values()
                .min_by_key(|failing| failing.due)
                .map(|failing| (failing.home.clone(), failing.due));
            match due {
                Some((home, at)) if at <= now => {
                    let what = queue.failing[&home].touched.clone();
                    return (home, what);
                }
                Some((_, at)) => {
                    queue = self
                        .wake
                        .wait_timeout(queue, at - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
                None => {
                    queue = self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
    }

    /// A pass over `home` ended: `failed` is what failed there, and why.
    fn ended(&self, home: &str, failed: Option<(Touched, String)>) {
        self.passes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut queue = self.queue();
        let Some((touched, error)) = failed else {
            queue.failing.remove(home);
            return;
        };
        let attempts = queue
            .failing
            .get(home)
            .map_or(0, |failing| failing.attempts)
            + 1;
        let backoff = first_backoff()
            .saturating_mul(1 << (attempts - 1).min(16))
            .min(MOST_BACKOFF);
        eprintln!(
            "newgit serve: stored rooted histories of {home} failed ({attempts} in a row; trying again in {} s): {error}",
            backoff.as_secs_f64()
        );
        queue.failing.insert(
            home.to_owned(),
            Failing {
                home: home.to_owned(),
                touched,
                error,
                attempts,
                due: std::time::Instant::now() + backoff,
            },
        );
    }

    /// Every project whose stored rooted histories failed to refresh last
    /// time, and why: what an operator looks at when a view is composed per
    /// fetch for longer than it should be.
    pub fn failing(&self) -> Vec<Failing> {
        self.queue().failing.values().cloned().collect()
    }

    fn is_done(&self, want: &Want) -> bool {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(want.home.to_string(), want.tracker.clone()))
            .is_some_and(|done| done.same_as(want))
    }

    fn did(&self, want: &Want) {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((want.home.to_string(), want.tracker.clone()), want.clone());
    }

    /// `tracker` of `home` has no stored rooted history any more.
    fn forget(&self, home: &Utf8Path, tracker: &str) {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(home.to_string(), tracker.to_owned()));
    }
}

/// Start the background thread: first every project, then whatever lands.
pub fn start(app: &std::sync::Arc<App>) {
    if std::env::var("NEWGIT_SERVE_ROOTED_BACKGROUND").is_ok_and(|value| value == "off") {
        app.rooted
            .backfilled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("rooted".to_owned())
        .spawn(move || {
            let homes = {
                let db = app.db.lock();
                db::project_keys(&db).unwrap_or_default()
            };
            for home in &homes {
                pass(&app, home, &Touched::All);
            }
            app.rooted
                .backfilled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            loop {
                let (home, touched) = app.rooted.next();
                pass(&app, &home, &touched);
            }
        })
        .expect("the rooted thread starts");
}

/// One pass over `home`, a panic in it caught: said, counted, and tried
/// again like any failure, and the thread goes on.
fn pass(app: &App, home: &str, touched: &Touched) {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        refresh_home(app, home, touched)
    }));
    let failed = match ran {
        Ok(failed) => failed,
        Err(panic) => {
            app.rooted
                .failed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let why = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".to_owned());
            Some((touched.clone(), format!("panicked: {why}")))
        }
    };
    app.rooted.ended(home, failed);
}

/// Bring the stored rooted histories of project `home` that `touched`
/// reaches up to its lists, and remove those of trackers it no longer has.
/// Returns what failed, and why.
fn refresh_home(app: &App, home: &str, touched: &Touched) -> Option<(Touched, String)> {
    let id = parse_project_key(home)?;
    // Everything that must be consistent, once, under the db lock; nothing
    // else is done holding it.
    let snapshot = {
        let db = app.db.lock();
        match ProjectContext::open(app, &db, &id).and_then(|project| Snapshot::read(&project, &db))
        {
            Ok(snapshot) => snapshot,
            // Removed since: nothing to keep up.
            Err(error) if error.status == 404 => return None,
            Err(error) => {
                app.rooted
                    .failed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Some((touched.clone(), error.cause().to_owned()));
            }
        }
    };
    app.log.panic_at("rooted-panic");
    // A worker whose lists are stale by the time it writes (the race test).
    app.log.pause("rooted-after-snapshot");
    let mut failed = BTreeSet::new();
    let mut why = String::new();
    let candidates: Vec<String> = snapshot
        .trackers()
        .filter(|tracker| touched.reaches(tracker))
        .cloned()
        .collect();
    let lock = app.compose_lock(home);
    for tracker in candidates {
        let Some(mut want) = snapshot.want(&tracker) else {
            continue;
        };
        if app.rooted.is_done(&want) {
            continue;
        }
        let checkpoint = || app.log.stop_at("rooted-checkpoint");
        let held = std::cell::RefCell::new(Some(lock.blocking_lock()));
        let between = || {
            drop(held.borrow_mut().take());
            *held.borrow_mut() = Some(lock.blocking_lock());
        };
        let result = located(&mut want, &id).and_then(|here| {
            if here {
                refresh_with(&want, chunk(), &checkpoint, &between).map(Some)
            } else {
                Ok(None)
            }
        });
        drop(held.borrow_mut().take());
        match result {
            Ok(Some(done)) => {
                if !done.gone && !done.newer {
                    app.rooted.did(&want);
                }
                app.rooted
                    .refreshed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(None) => {}
            Err(error) => {
                app.rooted
                    .failed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                why = format!("`{tracker}`: {}", error.cause());
                failed.insert(tracker);
            }
        }
    }
    if let Err(error) = remove_unwanted(app, &snapshot, touched) {
        app.rooted
            .failed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        why = format!("removing stored rooted histories: {}", error.cause());
        return Some((touched.clone(), why));
    }
    (!failed.is_empty()).then_some((Touched::Some(failed), why))
}

/// Remove the stored rooted history of every tracker `touched` reaches that
/// `snapshot` gives none (it left every list or the default branch, or it is
/// not where its audience says): derived data nobody is served. One composed
/// from newer lists than `snapshot`'s is left alone.
fn remove_unwanted(app: &App, snapshot: &Snapshot, touched: &Touched) -> ApiResult<()> {
    // Where a stored history that should not be could be: for everything,
    // every store's; for some trackers, the stores their rows name.
    let mut found: Vec<(Store, String)> = Vec::new();
    match touched {
        Touched::All => {
            let mut stores: BTreeSet<Store> = snapshot.rows.values().cloned().collect();
            stores.extend([Store::Public, Store::ProjectDevs]);
            for store in stores {
                let dir = store.dir(&snapshot.home);
                if !repos::store_exists(&dir) {
                    continue;
                }
                let listed = repos::git(
                    &dir,
                    &["for-each-ref", "--format=%(refname)", "refs/newgit/rooted/"],
                )?;
                for line in String::from_utf8_lossy(&listed.stdout).lines() {
                    if let Some(tracker) = line.strip_prefix("refs/newgit/rooted/") {
                        found.push((store.clone(), tracker.to_owned()));
                    }
                }
            }
        }
        Touched::Some(names) => {
            for name in names {
                if snapshot.want(name).is_none()
                    && let Some(store) = snapshot.row(name)
                {
                    found.push((store.clone(), name.clone()));
                }
            }
        }
    }
    let lock = app.compose_lock(&snapshot.id.key());
    for (store, tracker) in found {
        if snapshot
            .want(&tracker)
            .is_some_and(|want| want.store == store)
        {
            continue;
        }
        let _held = lock.blocking_lock();
        if remove(&store.dir(&snapshot.home), &tracker, &snapshot.at)? {
            app.rooted.forget(&snapshot.home, &tracker);
        }
    }
    Ok(())
}

/// Remove `tracker`'s stored rooted history from the store at `dir` (its
/// served namespace and its state ref, in one transaction, swapped against
/// the state read here), unless it was composed from lists newer than `at`.
/// Whether it was removed.
fn remove(dir: &Utf8Path, tracker: &str, at: &At) -> ApiResult<bool> {
    if !repos::store_exists(dir) {
        return Ok(false);
    }
    let mut objects = Objects::open(dir).map_err(core)?;
    let state = read_state(&mut objects, dir, tracker)?;
    let Some(commit) = state.commit else {
        return Ok(false);
    };
    if at.behind(&At::of(&state.served)) {
        return Ok(false);
    }
    let listed = repos::git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &prefix(tracker),
        ],
    )?;
    let mut script = format!("start\ndelete {} {commit}\n", state_ref(tracker));
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        let Some((reference, object)) = line.split_once(' ') else {
            continue;
        };
        // `HEAD` there is symbolic: removed itself, not what it names.
        if reference.ends_with("/HEAD") {
            script.push_str(&format!("delete {reference}\n"));
        } else {
            script.push_str(&format!("delete {reference} {object}\n"));
        }
    }
    let head = format!("{}HEAD", prefix(tracker));
    if symbolic(dir, &head)?.is_some() && !script.contains(&format!("delete {head}\n")) {
        script.push_str(&format!("delete {head}\n"));
    }
    script.push_str("prepare\ncommit\n");
    repos::run_stdin(dir, &["update-ref", "--no-deref", "--stdin"], &script)
}

/// Tracker `tracker` of project `id`'s stored rooted history as of landing
/// `stamp`: what the module answers for anything outside it that must name
/// a tracker's rooted history at a point in the log (a mirror export, a
/// webhook). The lists are read under the db lock; once landing `stamp` is
/// in place they hold it and everything before it, so the history is
/// brought up to them (composing what is missing, under the project's
/// compose lock) and its refs are read. Since a stored history never moves
/// back, they stay at least as new as `stamp` from then on. `Ok(None)` when
/// the tracker has no stored rooted history (a link, outside Git, not on the
/// default branch, not where its audience says). Refused (409) when `stamp`
/// is not in place yet. Blocking: never called from an async task, nor with
/// the db lock held.
pub fn as_of(app: &App, id: &ProjectId, tracker: &str, stamp: Stamp) -> ApiResult<Option<History>> {
    let snapshot = {
        let db = app.db.lock();
        let project = ProjectContext::open(app, &db, id)?;
        Snapshot::read(&project, &db)?
    };
    if snapshot.at.landed < stamp && app.log.applied() < stamp {
        return Err(HttpError::new(
            409,
            format!("landing {stamp} is not in place in {} yet", id.key()),
        ));
    }
    let Some(mut want) = snapshot.want(tracker) else {
        return Ok(None);
    };
    let lock = app.compose_lock(&id.key());
    let _held = lock.blocking_lock();
    if !located(&mut want, id)? {
        return Ok(None);
    }
    let done = refresh(&want)?;
    if done.gone {
        return Ok(None);
    }
    let dir = want.dir();
    let mut objects = Objects::open(&dir).map_err(core)?;
    let at = At::of(&read_state(&mut objects, &dir, tracker)?.served);
    let repo = want.repo();
    let refs = repo.list_refs("refs/")?;
    Ok(Some(History { at, repo, refs }))
}

/// A stored rooted history as [`as_of`] answers it.
#[derive(Debug, Clone)]
pub struct History {
    /// When the lists it was composed from were read: at or after the
    /// stamp asked for.
    pub at: At,
    /// Where it is served from: the namespace `r/<tracker>` in its store.
    pub repo: Repo,
    /// Its refs, as its readers name them (`refs/heads/main`), with objects.
    pub refs: Vec<(String, String)>,
}

/// Whether `prefix` is the namespace prefix of a stored rooted history (the
/// count of repos leaves them out: they are views, not repos).
pub fn is_rooted_namespace(kind: &str) -> bool {
    kind == "r"
}

/// The namespace `r/<tracker>`'s refs, as the store names them.
pub fn prefix(tracker: &str) -> String {
    namespace_prefix(&namespace(tracker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_composition_round_trips() {
        let found = Composition {
            view: "a".repeat(40),
            expanded: ["lib".to_owned(), "lib__vendor".to_owned()].into(),
            canonical: Some("b".repeat(40)),
        };
        assert_eq!(decode(&encode(&found)), Some(found));
        let bare = Composition {
            view: "c".repeat(40),
            expanded: BTreeSet::new(),
            canonical: None,
        };
        assert_eq!(decode(&encode(&bare)), Some(bare));
    }

    #[test]
    fn sources_leave_out_the_views() {
        let served = "default main\nhead main abc k1 - v1\ntag v1 def k1 a1 v2\n";
        assert_eq!(
            sources_of(served),
            vec!["default main", "head main abc k1 -", "tag v1 def k1 a1"]
        );
    }
}
