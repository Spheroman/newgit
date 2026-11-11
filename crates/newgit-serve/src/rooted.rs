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
//! notes, in Git's own notes layout). The state ref and every served ref
//! move in one ref transaction.
//!
//! **When.** Derived data, never a state anyone sees (landing.md, *How we got
//! here*): nothing about it is in a landing's entry. Every read of a rooted
//! view compares what is stored with the lists it reads under the db lock,
//! and composes what is missing before it serves, so no reader ever sees a
//! rooted view behind what has landed. After each landing the touched
//! trackers are extended in the background ([`Worker`]), outside every lock a
//! landing holds, so the read usually finds it done. A landing voided or
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
use crate::names::{ProjectId, RepoRef, namespace_prefix, parse_project_key, within};
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
}

impl Want {
    pub fn dir(&self) -> Utf8PathBuf {
        self.store.dir(&self.home)
    }

    /// The repo readers are served: the namespace in `S`.
    pub fn repo(&self) -> Repo {
        Repo::new(&self.home, self.store.clone(), namespace(&self.tracker))
    }

    /// What the state ref's `served` says when the history is this.
    fn served_text(&self, views: &BTreeMap<String, String>) -> String {
        let mut text = format!("default {}\n", self.default_branch);
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

/// Tracker `tracker` of `project`'s stored rooted history, as its lists,
/// tags, definitions, and store rows have it under the db lock. `None` when
/// it has none: a link (its home is elsewhere), a tracker outside Git, one
/// not on the default branch, or one not in the store its audience calls for (refused by
/// `served`, as every repo handed out is).
pub fn wanted(
    project: &ProjectContext<'_>,
    db: &Connection,
    tracker: &str,
) -> ApiResult<Option<Want>> {
    if project.links.contains_key(tracker) {
        return Ok(None);
    }
    let branches = db::branches(db, &project.key)?;
    let lists: Vec<(String, db::List)> = branches
        .iter()
        .map(|branch| {
            (
                branch["name"].as_str().unwrap_or_default().to_owned(),
                serde_json::from_value(branch["list"].clone()).unwrap_or_default(),
            )
        })
        .collect();
    // A view's `HEAD` is the default branch, so one not on it is not served
    // (the composer refuses it too).
    let default = &project.info.default_branch;
    if !lists
        .iter()
        .any(|(name, list)| name == default && list.contains_key(tracker))
    {
        return Ok(None);
    }
    let named: Vec<&str> = lists
        .iter()
        .flat_map(|(_, list)| list.keys().map(String::as_str))
        .collect();
    let facts = project.definitions(db, named)?;
    if !facts.get(tracker).is_some_and(|fact| fact.git) {
        return Ok(None);
    }
    let Ok(location) = project.served(
        db,
        &RepoRef::tracker(&project.id, tracker),
        facts.get(tracker),
        false,
    ) else {
        return Ok(None);
    };
    if !location.exists() {
        return Ok(None);
    }
    let store = location.store.clone();
    let mut stores = BTreeMap::new();
    let mut member = |name: &str| -> bool {
        if !within(name, tracker)
            || project.links.contains_key(name)
            || !facts.get(name).is_some_and(|fact| fact.git)
        {
            return false;
        }
        let there = project.own(name).store;
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
    for (name, list) in &lists {
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
    for tag in db::tags(db, &project.key)? {
        let list: db::List = serde_json::from_value(tag["list"].clone()).unwrap_or_default();
        let Some(source) = list.get(tracker) else {
            continue;
        };
        tags.insert(
            tag["name"].as_str().unwrap_or_default().to_owned(),
            Part {
                source: source.clone(),
                set: cut(&list),
                annotation: serde_json::from_value(tag["annotation"].clone())
                    .ok()
                    .flatten(),
            },
        );
    }
    Ok(Some(Want {
        home: project.home(),
        tracker: tracker.to_owned(),
        store,
        default_branch: project.info.default_branch.clone(),
        heads,
        tags,
        stores,
    }))
}

/// What [`refresh`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refreshed {
    /// Nothing was missing.
    pub fresh: bool,
    /// Commits of the tracker composed now.
    pub composed: usize,
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

/// Move the state ref from `old` to `new`, and every served ref of `want`'s
/// namespace in `refs` (a full name → its object, `None` deleted), in one
/// transaction.
fn write_refs(
    want: &Want,
    old: Option<&str>,
    new: &str,
    refs: &BTreeMap<String, Option<String>>,
) -> ApiResult<()> {
    let zero = "0".repeat(40);
    let mut lines = vec![format!(
        "update {} {new} {}",
        state_ref(&want.tracker),
        old.unwrap_or(&zero)
    )];
    for (name, object) in refs {
        match object {
            Some(object) => lines.push(format!("update {name} {object}")),
            None => lines.push(format!("delete {name}")),
        }
    }
    repos::Prepared::new(&want.dir(), &lines)?.commit()
}

/// Bring `want`'s stored rooted history up to the lists: compose only the
/// commits not composed yet, and move its refs. Run holding the project's
/// compose lock, never the db lock or a landing's.
pub fn refresh(want: &Want) -> ApiResult<Refreshed> {
    refresh_with(want, chunk(), &|| {})
}

/// [`refresh`], recording what is composed every `chunk` commits and
/// calling `checkpoint` after each (the crash test stops there).
pub fn refresh_with(want: &Want, chunk: usize, checkpoint: &dyn Fn()) -> ApiResult<Refreshed> {
    let chunk = chunk.max(1);
    let dir = want.dir();
    let mut objects = Objects::open(&dir).map_err(core)?;
    let state = read_state(&mut objects, &dir, &want.tracker)?;
    if !state.served.is_empty()
        && sources_of(&state.served) == sources_of(&want.served_text(&BTreeMap::new()))
    {
        return Ok(Refreshed {
            fresh: true,
            composed: 0,
        });
    }
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
                let mut lines = state.lines.clone();
                lines.insert(key.clone(), line.tree(&mut objects));
                let commit = state_commit(&mut objects, &state.served, &lines);
                objects.flush().map_err(core)?;
                let current = read_state(&mut objects, &dir, &want.tracker)?.commit;
                write_refs(want, current.as_deref(), &commit, &BTreeMap::new())?;
                checkpoint();
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
    let current = read_state(&mut objects, &dir, &want.tracker)?.commit;
    write_refs(want, current.as_deref(), &commit, &refs)?;
    let head = repo.full("HEAD");
    let target = repo.full(&format!("refs/heads/{}", want.default_branch));
    let pointed = repos::git(&dir, &["symbolic-ref", "--quiet", &head])?;
    if String::from_utf8_lossy(&pointed.stdout).trim() != target {
        let output = repos::git(&dir, &["symbolic-ref", &head, &target])?;
        if !output.status.success() {
            return Err(HttpError::internal(format!(
                "pointing {head} at {target}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(Refreshed {
        fresh: false,
        composed,
    })
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

/// Extends stored rooted histories in the background: after each landing,
/// for the projects it touched, and once at start for every project (the
/// backfill of a data directory from before them). One thread; each
/// tracker is refreshed under its project's compose lock, so a read that
/// needs it waits for at most that tracker.
#[derive(Default)]
pub struct Worker {
    queue: Mutex<VecDeque<String>>,
    wake: Condvar,
    /// What each tracker was last brought up to here, by `(home, tracker)`:
    /// one unchanged since is not read again.
    done: Mutex<HashMap<(String, String), Want>>,
    /// Trackers refreshed, for the tests that wait on the backfill.
    pub refreshed: std::sync::atomic::AtomicU64,
    /// The background pass at start has looked at every project.
    pub backfilled: std::sync::atomic::AtomicBool,
}

impl Worker {
    /// Something landed in `homes`: their trackers may need extending.
    pub fn touched<'h>(&self, homes: impl IntoIterator<Item = &'h String>) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for home in homes {
            if !queue.contains(home) {
                queue.push_back(home.clone());
            }
        }
        self.wake.notify_all();
    }

    fn next(&self) -> String {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(home) = queue.pop_front() {
                return home;
            }
            queue = self
                .wake
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub fn forget(&self, home: &str, tracker: &str) {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(home.to_owned(), tracker.to_owned()));
    }

    fn is_done(&self, want: &Want, home: &str) -> bool {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(home.to_owned(), want.tracker.clone()))
            == Some(want)
    }

    fn did(&self, want: &Want, home: &str) {
        self.done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((home.to_owned(), want.tracker.clone()), want.clone());
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
                refresh_home(&app, home);
            }
            app.rooted
                .backfilled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            loop {
                let home = app.rooted.next();
                refresh_home(&app, &home);
            }
        })
        .expect("the rooted thread starts");
}

/// Bring every stored rooted history of project `home` up to its lists.
fn refresh_home(app: &App, home: &str) {
    let Some(id) = parse_project_key(home) else {
        return;
    };
    let wants = {
        let db = app.db.lock();
        match wants_of(app, &db, &id) {
            Ok(wants) => wants,
            Err(error) => {
                eprintln!(
                    "newgit serve: rooted histories of {home}: {}",
                    error.cause()
                );
                return;
            }
        }
    };
    for want in wants {
        if app.rooted.is_done(&want, home) {
            continue;
        }
        let lock = app.compose_lock(home);
        let held = lock.blocking_lock();
        let checkpoint = || app.log.stop_at("rooted-checkpoint");
        match refresh_with(&want, chunk(), &checkpoint) {
            Ok(_) => app.rooted.did(&want, home),
            Err(error) => eprintln!(
                "newgit serve: extending `{}`'s rooted history in {home}: {}",
                want.tracker,
                error.cause()
            ),
        }
        drop(held);
        app.rooted
            .refreshed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Every tracker of `id` with a stored rooted history, as wanted now.
fn wants_of(app: &App, db: &Connection, id: &ProjectId) -> ApiResult<Vec<Want>> {
    let project = ProjectContext::open(app, db, id)?;
    let mut wants = Vec::new();
    for tracker in db::listed_trackers(db, &project.key)? {
        if let Some(want) = wanted(&project, db, &tracker)? {
            wants.push(want);
        }
    }
    Ok(wants)
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
