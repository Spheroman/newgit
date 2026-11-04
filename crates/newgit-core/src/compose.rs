//! A workspace's Git is a view composed from pinned trackers
//! (docs/design/trackers.md, *Compose* and *Split*).
//!
//! Every tracker in Git is its own history, stored relative to its own root.
//! A parent records the commit of each child it mounts as a gitlink, a
//! *pin*, at `.newgit/pins/<name>`. The root tracker's history is the view's
//! shape: each root commit composes to exactly one view commit, and
//! splitting a view commit makes exactly one root commit.
//!
//! - [`compose`] walks a root's history and expands every readable pin into
//!   files under its mount. A child's changes split from the same view
//!   commit are *inline*; history from anywhere else is *brought in* by a
//!   `git subtree`-shaped merge. Rebuilt commits come back as the originals
//!   they were split from, signatures included, wherever their bytes allow.
//! - [`split`] takes view commits (an agent's work, a merge) and makes one
//!   root commit for each, with a commit in each child the work changed and
//!   every other pin reused.
//!
//! Which root commit a view commit stands for is kept in a note on it
//! (`refs/notes/newgit-composed`): `<tracker> <commit>`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::objects::{Commit, EMPTY_TREE, GITLINK_MODE, Objects, TREE_MODE, run};
use crate::tracker::{DEFINITIONS_DIR, TrackerDefinition, owner_of};

/// Where a parent keeps the commits of the children it mounts.
pub const PINS_DIR: &str = ".newgit/pins";

/// Where each view commit names the root commit it stands for.
pub const NOTES_REF: &str = "refs/notes/newgit-composed";

/// Where a rebuilt view commit names the commit it stands for — its
/// canonical id, the commit as its author made it — when the two differ (a
/// view without every tracker cannot rebuild the original). Commits given
/// back as their originals need no note: they are the original.
pub const CANONICAL_NOTES_REF: &str = "refs/notes/newgit-canonical";

const SPLIT_TRAILER: &[u8] = b"Newgit-Split: ";

/// One line of the headers of the commit split, kept verbatim: what Git
/// stores after the committer line — a signature (`gpgsig`), a merged tag
/// (`mergetag`), an `encoding`. One trailer per line, continuation lines
/// included: a PGP signature has a line holding only a space, which Git's
/// trailer parser takes for a blank line, so continuation lines left as
/// they are would hide every trailer from `git log --format=%(trailers)`.
const HEADER_TRAILER: &[u8] = b"Newgit-Split-Header: ";

/// `message` with the trailers a root commit split from view commit
/// `source` carries: the view commit's `headers`, and its id. It ends the
/// way `message` did — the same run of newlines, often none (GitHub's merge
/// commits have no final newline) — so composing it back gives the
/// original message byte for byte.
pub(crate) fn with_split_trailers(message: &[u8], source: &str, headers: &[u8]) -> Vec<u8> {
    let body_len = message.len()
        - message
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\n')
            .count();
    let (body, ending) = message.split_at(body_len);
    let mut out = body.to_vec();
    out.extend_from_slice(b"\n\n");
    for line in headers.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        out.extend_from_slice(HEADER_TRAILER);
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(SPLIT_TRAILER);
    out.extend_from_slice(source.as_bytes());
    out.extend_from_slice(ending);
    out
}

/// What a root commit's trailers say about the view commit it was split
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitFrom {
    pub source: String,
    pub message: Vec<u8>,
    pub headers: Vec<u8>,
}

/// The reverse of [`with_split_trailers`]: `None` when `message` is not a
/// split commit's. The trailers are the last paragraph: none of their lines
/// is blank.
pub(crate) fn parse_split(message: &[u8]) -> Option<SplitFrom> {
    let body_len = message.len()
        - message
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\n')
            .count();
    let (trimmed, ending) = message.split_at(body_len);
    let (body, block) = match trimmed.windows(2).rposition(|pair| pair == b"\n\n") {
        Some(at) => (&trimmed[..at], &trimmed[at + 2..]),
        None => (&trimmed[..0], trimmed),
    };
    let mut lines: Vec<&[u8]> = block.split(|&byte| byte == b'\n').collect();
    let source = lines.pop()?.strip_prefix(SPLIT_TRAILER)?;
    let source = std::str::from_utf8(source).ok()?.to_owned();
    if source.len() != 40 || !source.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let ours = lines.iter().all(|line| line.starts_with(HEADER_TRAILER));
    let mut restored = body.to_vec();
    let mut headers = Vec::new();
    if ours {
        for line in &lines {
            headers.extend_from_slice(&line[HEADER_TRAILER.len()..]);
            headers.push(b'\n');
        }
    } else {
        // Lines that are not ours belong to the message.
        restored.extend_from_slice(b"\n\n");
        restored.extend_from_slice(&lines.join(&b'\n'));
    }
    restored.extend_from_slice(ending);
    Some(SplitFrom {
        source,
        message: restored,
        headers,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The view's tip; `None` when the root composes to nothing.
    pub tip: Option<String>,
    /// How many view commits it has.
    pub commits: usize,
}

/// Tracker commits by tracker name.
pub type State = BTreeMap<String, String>;

/// Whether a commit may be expanded as a tracker's: `(tracker, commit)`.
pub type Admit<'a> = dyn Fn(&str, &str) -> bool + 'a;

/// Who is composing: which trackers they may read, and how much to trust a
/// pin.
pub struct ComposeOptions<'a> {
    /// The trackers this reader may read. A child they may not read is left
    /// out of every tree, with everything mounted inside it.
    pub readable: &'a dyn Fn(&str) -> bool,
    /// Whether `commit` may be expanded as tracker `name`'s: the endpoint
    /// checks every pin is in that tracker's own history, so a parent cannot
    /// pin another tracker's commit under a name its reader may read. `None`
    /// trusts every pin (a local store holds only your own trackers).
    pub admit: Option<&'a Admit<'a>>,
}

/// The pins in `tree`: child name → commit.
pub fn pins_in(objects: &mut Objects, tree: &str) -> Result<BTreeMap<String, String>> {
    let Some(pins) = objects.subtree(tree, PINS_DIR)? else {
        return Ok(BTreeMap::new());
    };
    Ok(objects
        .tree(&pins)?
        .into_iter()
        .filter(|entry| entry.mode == GITLINK_MODE)
        .map(|entry| (String::from_utf8_lossy(&entry.name).into_owned(), entry.id))
        .collect())
}

/// The tree holding the definitions of what `tree` mounts: its own
/// `.newgit/trackers/` before `access` is carved, or the root of the pinned
/// child that is mounted there (`access`, which says so in its own
/// `access.toml`).
fn definitions_tree(
    objects: &mut Objects,
    tree: &str,
    pins: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    if let Some(found) = objects.subtree(tree, DEFINITIONS_DIR)? {
        return Ok(Some(found));
    }
    for (name, pin) in pins {
        let Some(commit) = objects.try_commit(pin)? else {
            continue;
        };
        if let Some(entry) = objects.entry_at(&commit.tree, &format!("{name}.toml"))?
            && mount_in(&read_blob_text(objects, &entry.id)?).as_deref() == Some(DEFINITIONS_DIR)
        {
            return Ok(Some(commit.tree));
        }
    }
    Ok(None)
}

fn read_blob_text(objects: &mut Objects, id: &str) -> Result<String> {
    let out = run(
        Command::new("git")
            .arg("-C")
            .arg(objects.repo().as_str())
            .args(["cat-file", "blob", id]),
        "git cat-file blob",
        None,
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A definition's `mount`, read loosely: compose needs where, not what.
fn mount_in(text: &str) -> Option<String> {
    let value: toml::Table = toml::from_str(text).ok()?;
    let mount = value
        .get("mount")
        .and_then(|mount| mount.as_str())
        .unwrap_or(".");
    Some(crate::tracker::normalize_path(Utf8Path::new(mount)).to_string())
}

/// Where each pinned child of a commit with `tree` is mounted, as that
/// commit's own definitions say.
fn mounts_of(
    objects: &mut Objects,
    commit: &str,
    tree: &str,
    all_pins: &BTreeMap<String, String>,
    wanted: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut mounts = BTreeMap::new();
    if wanted.is_empty() {
        return Ok(mounts);
    }
    let definitions = definitions_tree(objects, tree, all_pins)?;
    for name in wanted.keys() {
        let entry = match &definitions {
            Some(definitions) => objects.entry_at(definitions, &format!("{name}.toml"))?,
            None => None,
        };
        let Some(entry) = entry else {
            return Err(NewgitError::Unsupported(format!(
                "commit {} pins `{name}` but defines no `{name}` to say where it is mounted",
                short(commit)
            )));
        };
        let text = read_blob_text(objects, &entry.id)?;
        let mount = mount_in(&text).ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "commit {}'s definition of `{name}` is not TOML",
                short(commit)
            ))
        })?;
        mounts.insert(name.clone(), mount);
    }
    Ok(mounts)
}

/// `tree` with `child` put at `mount`: the subtree there, or laid over the
/// whole tree for a tracker mounted at `.`.
fn graft(objects: &mut Objects, tree: &str, mount: &str, child: &str) -> Result<String> {
    if mount == "." {
        return objects.overlay(tree, child);
    }
    // Two trackers mounted at one path (each claiming its own files there)
    // share the directory.
    let merged = match objects.subtree(tree, mount)? {
        Some(existing) => objects.overlay(&existing, child)?,
        None => child.to_owned(),
    };
    let entry = (merged != EMPTY_TREE).then(|| (TREE_MODE.to_vec(), merged));
    objects.set_path(tree, mount, entry)
}

/// `tree` with whatever is at `mount` replaced by `child`: a replayed
/// commit's mount holds exactly that child commit's files.
fn replace(objects: &mut Objects, tree: &str, mount: &str, child: &str) -> Result<String> {
    if mount == "." {
        return objects.overlay(tree, child);
    }
    let entry = (child != EMPTY_TREE).then(|| (TREE_MODE.to_vec(), child.to_owned()));
    objects.set_path(tree, mount, entry)
}

fn depth(mount: &str) -> usize {
    if mount == "." {
        0
    } else {
        mount.split('/').count()
    }
}

#[derive(Debug, Clone)]
struct Composed {
    /// The view commit.
    id: String,
    tree: String,
}

/// Where a child view commit was taken: `(walk position, tracker commit,
/// view commit)`.
type TakenAt = (usize, String, String);

/// A tracker commit with its pins and where each readable one is mounted.
type Shape = (Commit, BTreeMap<String, String>, BTreeMap<String, String>);

/// Per tracker: which child view commits its view has taken, and where.
#[derive(Default)]
struct Taken {
    /// `(mount, child view commit)` → every `(position, tracker commit, view
    /// commit)` that took it inline.
    inline: HashMap<(String, String), Vec<TakenAt>>,
    /// The same for replays: where a brought-in child commit was replayed.
    replayed: HashMap<(String, String), Vec<TakenAt>>,
}

struct Composer<'a> {
    objects: Objects,
    options: &'a ComposeOptions<'a>,
    /// Tracker commit → its view commit.
    views: HashMap<String, Composed>,
    /// View commit → its parents and its tree: the view graph, every
    /// tracker's.
    graph: HashMap<String, (Vec<String>, String)>,
    /// Tracker commit → its walk position (ancestors first).
    position: HashMap<String, usize>,
    next_position: usize,
    taken: HashMap<String, Taken>,
    /// Tips already composed, per tracker: a later walk stops at them.
    done: HashMap<String, Vec<String>>,
    /// Trackers being composed right now, outermost first: a cycle guard.
    composing: Vec<String>,
    notes: Vec<(String, String)>,
    canonical: Vec<(String, String)>,
}

/// Compose the views rooted at tracker `root`, one per `(view ref, root
/// commit)` in `tips`, into `repo`, which must hold every tracker commit
/// the reader may read. Each view ref is set to its view's tip.
pub fn compose(
    repo: &Utf8Path,
    root: &str,
    tips: &[(String, String)],
    options: &ComposeOptions<'_>,
) -> Result<Vec<View>> {
    let mut composer = Composer {
        objects: Objects::open(repo)?,
        options,
        views: HashMap::new(),
        graph: HashMap::new(),
        position: HashMap::new(),
        next_position: 0,
        taken: HashMap::new(),
        done: HashMap::new(),
        composing: Vec::new(),
        notes: Vec::new(),
        canonical: Vec::new(),
    };
    if !(options.readable)(root) {
        return Ok(tips
            .iter()
            .map(|_| View {
                tip: None,
                commits: 0,
            })
            .collect());
    }
    let commits: Vec<String> = tips.iter().map(|(_, commit)| commit.clone()).collect();
    composer.compose_tracker(root, &commits)?;
    composer.objects.flush()?;
    let mut views = Vec::new();
    for (view_ref, commit) in tips {
        let tip = composer.views.get(commit).map(|view| view.id.clone());
        let commits = match &tip {
            Some(tip) => {
                git(repo, &["update-ref", view_ref, tip])?;
                git(repo, &["rev-list", "--count", tip])?
                    .trim()
                    .parse()
                    .unwrap_or(0)
            }
            None => 0,
        };
        views.push(View { tip, commits });
    }
    write_notes(repo, NOTES_REF, "compose", &composer.notes)?;
    let canonical: Vec<(String, String)> = composer
        .canonical
        .iter()
        .map(|(id, original)| (id.clone(), format!("canonical {original}\n")))
        .collect();
    write_notes(repo, CANONICAL_NOTES_REF, "canonical", &canonical)?;
    Ok(views)
}

impl Composer<'_> {
    fn compose_tracker(&mut self, name: &str, tips: &[String]) -> Result<()> {
        let tips: Vec<String> = tips
            .iter()
            .filter(|tip| !self.views.contains_key(*tip))
            .cloned()
            .collect();
        if tips.is_empty() {
            return Ok(());
        }
        if self.composing.iter().any(|outer| outer == name) {
            return Err(NewgitError::Unsupported(format!(
                "`{name}` is mounted inside itself ({} → {name})",
                self.composing.join(" → ")
            )));
        }
        self.composing.push(name.to_owned());
        let result = self.compose_walk(name, &tips);
        self.composing.pop();
        result
    }

    fn compose_walk(&mut self, name: &str, tips: &[String]) -> Result<()> {
        let mut args = vec![
            "rev-list".to_owned(),
            "--topo-order".to_owned(),
            "--reverse".to_owned(),
        ];
        args.extend(tips.iter().cloned());
        if let Some(done) = self.done.get(name) {
            args.push("--not".to_owned());
            args.extend(done.iter().cloned());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let walk: Vec<String> = git(self.objects.repo(), &args)?
            .lines()
            .map(ToOwned::to_owned)
            .filter(|commit| !self.views.contains_key(commit))
            .collect();
        self.done
            .entry(name.to_owned())
            .or_default()
            .extend(tips.iter().cloned());

        // First every child this history pins, so a parent always mounts a
        // view that is already composed.
        let mut shapes: Vec<Shape> = Vec::with_capacity(walk.len());
        let mut child_tips: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for id in &walk {
            let commit = self.objects.commit(id)?;
            let all_pins = pins_in(&mut self.objects, &commit.tree)?;
            let mut readable = BTreeMap::new();
            for (child, pin) in &all_pins {
                if !(self.options.readable)(child) {
                    continue;
                }
                if let Some(admit) = self.options.admit
                    && !admit(child, pin)
                {
                    return Err(NewgitError::Unsupported(format!(
                        "commit {} of `{name}` pins `{child}` at {}, which is not in `{child}`'s \
                         history",
                        short(id),
                        short(pin)
                    )));
                }
                readable.insert(child.clone(), pin.clone());
                child_tips
                    .entry(child.clone())
                    .or_default()
                    .insert(pin.clone());
            }
            let mounts = mounts_of(&mut self.objects, id, &commit.tree, &all_pins, &readable)?;
            shapes.push((commit, all_pins, mounts));
        }
        for (child, pins) in child_tips {
            let pins: Vec<String> = pins.into_iter().collect();
            self.compose_tracker(&child, &pins)?;
        }

        let mut own_trees: HashMap<String, String> = HashMap::new();
        for (commit, all_pins, mounts) in shapes {
            let position = self.next_position;
            self.next_position += 1;
            self.position.insert(commit.id.clone(), position);
            self.compose_one(name, &commit, &all_pins, &mounts, &mut own_trees)?;
        }
        Ok(())
    }

    fn own_tree(&mut self, commit: &Commit, cache: &mut HashMap<String, String>) -> Result<String> {
        if let Some(tree) = cache.get(&commit.id) {
            return Ok(tree.clone());
        }
        let tree = self.objects.set_path(&commit.tree, PINS_DIR, None)?;
        cache.insert(commit.id.clone(), tree.clone());
        Ok(tree)
    }

    fn compose_one(
        &mut self,
        name: &str,
        commit: &Commit,
        all_pins: &BTreeMap<String, String>,
        mounts: &BTreeMap<String, String>,
        own_trees: &mut HashMap<String, String>,
    ) -> Result<()> {
        let own = self.own_tree(commit, own_trees)?;
        let mut ordered: Vec<(&String, &String)> = mounts.iter().collect();
        ordered.sort_by(|(left_name, left), (right_name, right)| {
            depth(left)
                .cmp(&depth(right))
                .then_with(|| left.cmp(right))
                .then_with(|| left_name.cmp(right_name))
        });
        let mut tree = own.clone();
        for (child, mount) in &ordered {
            let pin = &all_pins[*child];
            let view = self.views.get(pin).cloned().ok_or_else(|| missing(pin))?;
            tree = graft(&mut self.objects, &tree, mount, &view.tree)?;
        }

        // A single-parent commit whose only change was to children this
        // reader may not read is dropped: the commit after it moves onto
        // its parent.
        let parent_commits: Vec<Commit> = commit
            .parents
            .iter()
            .map(|parent| self.objects.commit(parent))
            .collect::<Result<_>>()?;
        if let [parent] = parent_commits.as_slice() {
            let parent_pins = pins_in(&mut self.objects, &parent.tree)?;
            let parent_own = self.own_tree(parent, own_trees)?;
            let moved: BTreeSet<&String> = all_pins
                .keys()
                .chain(parent_pins.keys())
                .filter(|child| all_pins.get(*child) != parent_pins.get(*child))
                .collect();
            if parent_own == own
                && !moved.is_empty()
                && moved.iter().all(|child| !(self.options.readable)(child))
                && let Some(view) = self.views.get(&parent.id).cloned()
            {
                self.views.insert(commit.id.clone(), view);
                return Ok(());
            }
        }

        let mut parents: Vec<String> = Vec::new();
        for parent in &commit.parents {
            let view = self.views.get(parent).ok_or_else(|| missing(parent))?;
            if !parents.contains(&view.id) {
                parents.push(view.id.clone());
            }
        }

        let split = parse_split(&commit.message);
        // Inline, or brought in: per child, the child view commits new at
        // this commit are the other halves of the same work, or history from
        // elsewhere, which comes in as a merge.
        let mut inline: Vec<(String, String, Vec<String>)> = Vec::new();
        let mut brought: Vec<(String, String)> = Vec::new();
        for (child, mount) in &ordered {
            let tip = self.views[&all_pins[*child]].id.clone();
            let mut before: Vec<String> = Vec::new();
            for parent in &parent_commits {
                let parent_pins = pins_in(&mut self.objects, &parent.tree)?;
                if let Some(pin) = parent_pins.get(*child)
                    && let Some(view) = self.views.get(pin)
                    && !before.contains(&view.id)
                {
                    before.push(view.id.clone());
                }
            }
            if before.contains(&tip) {
                continue;
            }
            let new = self.new_commits(&tip, &before);
            if new.is_empty() {
                continue;
            }
            let same_work = new.len() == 1 && {
                let only = self.objects.commit(&new[0])?;
                only.parents == before
                    && (commit.parents.len() > 1 || only.committer == commit.committer)
            };
            if split.is_some() || same_work {
                inline.push(((*mount).clone(), tip, new));
            } else {
                brought.push(((*mount).clone(), tip));
            }
        }
        for (mount, tip) in &brought {
            let replay = self.replay(name, commit, mount, tip)?;
            if !parents.contains(&replay) {
                parents.push(replay);
            }
        }

        // The metadata: the commit's own, or for one split from a view
        // commit, that view commit's, put back from the trailers — taken
        // from the child commit that holds the work when this one holds
        // none of its own.
        let (author, committer, message, headers, expected) = match &split {
            Some(split) => {
                let own_unchanged = match parent_commits.first() {
                    Some(first) => self.own_tree(first, own_trees)? == own,
                    None => own == EMPTY_TREE,
                };
                match inline.first() {
                    Some((_, tip, _)) if own_unchanged => {
                        let holder = self.objects.commit(tip)?;
                        (
                            holder.author,
                            holder.committer,
                            holder.message,
                            split.headers.clone(),
                            split.source.clone(),
                        )
                    }
                    _ => (
                        commit.author.clone(),
                        commit.committer.clone(),
                        split.message.clone(),
                        split.headers.clone(),
                        split.source.clone(),
                    ),
                }
            }
            None => (
                commit.author.clone(),
                commit.committer.clone(),
                commit.message.clone(),
                commit.headers.clone(),
                commit.id.clone(),
            ),
        };
        let whole = Objects::commit_id(&tree, &parents, &author, &committer, &headers, &message);
        let id = if whole == expected {
            self.objects
                .make_commit(&tree, &parents, &author, &committer, &headers, &message)
        } else {
            let id = self.objects.make_commit(
                &tree,
                &parents,
                &author,
                &committer,
                &Commit::unsigned_headers(&headers),
                &message,
            );
            if split.is_some() && id != expected {
                self.canonical.push((id.clone(), expected));
            }
            id
        };
        self.graph
            .insert(id.clone(), (parents.clone(), tree.clone()));
        let position = self.position[&commit.id];
        let taken = self.taken.entry(name.to_owned()).or_default();
        for (mount, _, new) in inline {
            for child in new {
                taken
                    .inline
                    .entry((mount.clone(), child))
                    .or_default()
                    .push((position, commit.id.clone(), id.clone()));
            }
        }
        self.notes
            .push((id.clone(), format!("{name} {}\n", commit.id)));
        self.views.insert(commit.id.clone(), Composed { id, tree });
        Ok(())
    }

    /// The view commits reachable from `tip` and from none of `before`.
    fn new_commits(&self, tip: &str, before: &[String]) -> Vec<String> {
        if let Some((parents, _)) = self.graph.get(tip)
            && parents == before
        {
            return vec![tip.to_owned()];
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = before.to_vec();
        while let Some(current) = stack.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some((parents, _)) = self.graph.get(&current) {
                stack.extend(parents.iter().cloned());
            }
        }
        let mut new = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![tip.to_owned()];
        while let Some(current) = stack.pop() {
            if seen.contains(&current) || !visited.insert(current.clone()) {
                continue;
            }
            if let Some((parents, _)) = self.graph.get(&current) {
                stack.extend(parents.iter().cloned());
            }
            new.push(current);
        }
        new
    }

    /// Every commit of `name`'s history behind `commit` (not itself).
    fn ancestors(&mut self, commit: &Commit) -> Result<HashSet<String>> {
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = commit.parents.clone();
        while let Some(current) = stack.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            stack.extend(self.objects.commit(&current)?.parents);
        }
        Ok(seen)
    }

    /// Child view commit `tip`, brought in under `mount` at `commit` of
    /// `name`: replayed as `git subtree` would, each child commit with its
    /// tree under the mount and, beside it, the rest of the tree as its
    /// first parent holds it. A child commit this view already holds — one
    /// a commit behind this one took inline, or replayed — is represented by
    /// that commit, so nothing appears twice. A pure function of the child's
    /// history and of this commit's own history.
    fn replay(&mut self, name: &str, commit: &Commit, mount: &str, tip: &str) -> Result<String> {
        let behind = self.ancestors(commit)?;
        let position = self.position[&commit.id];
        let mut made: HashMap<String, String> = HashMap::new();
        let represented = |composer: &Composer<'_>, child: &str, made: &HashMap<String, String>| {
            if let Some(id) = made.get(child) {
                return Some(id.clone());
            }
            let taken = composer.taken.get(name)?;
            let key = (mount.to_owned(), child.to_owned());
            taken
                .inline
                .get(&key)
                .into_iter()
                .flatten()
                .chain(taken.replayed.get(&key).into_iter().flatten())
                .filter(|(_, at, _)| behind.contains(at))
                .min_by_key(|(at_position, _, _)| *at_position)
                .map(|(_, _, view)| view.clone())
        };
        // Depth-first over what is not represented yet, iteratively, so a
        // long history cannot overflow the stack.
        let mut stack = vec![(tip.to_owned(), false)];
        while let Some((current, ready)) = stack.pop() {
            if represented(self, &current, &made).is_some() {
                continue;
            }
            let (parents, child_tree) = self
                .graph
                .get(&current)
                .cloned()
                .ok_or_else(|| missing(&current))?;
            if !ready {
                stack.push((current, true));
                for parent in parents.iter().rev() {
                    stack.push((parent.clone(), false));
                }
                continue;
            }
            let mapped: Vec<String> = parents
                .iter()
                .map(|parent| represented(self, parent, &made).ok_or_else(|| missing(parent)))
                .collect::<Result<_>>()?;
            let base = match mapped.first() {
                Some(first) => self.graph.get(first).map(|(_, tree)| tree.clone()),
                None => None,
            }
            .unwrap_or_else(|| EMPTY_TREE.to_owned());
            let tree = replace(&mut self.objects, &base, mount, &child_tree)?;
            let original = self.objects.commit(&current)?;
            let mut deduped: Vec<String> = Vec::new();
            for parent in mapped {
                if !deduped.contains(&parent) {
                    deduped.push(parent);
                }
            }
            let id = self.objects.make_commit(
                &tree,
                &deduped,
                &original.author,
                &original.committer,
                &Commit::unsigned_headers(&original.headers),
                &original.message,
            );
            self.graph.insert(id.clone(), (deduped, tree));
            made.insert(current.clone(), id.clone());
            self.taken
                .entry(name.to_owned())
                .or_default()
                .replayed
                .entry((mount.to_owned(), current))
                .or_default()
                .push((position, commit.id.clone(), id));
        }
        represented(self, tip, &made).ok_or_else(|| missing(tip))
    }
}

/// The view tree of `commit` (a tracker commit) for a reader of everything
/// present: its own tree with every pin expanded under its mount. What split
/// compares a view's subtree against.
pub fn view_tree(objects: &mut Objects, commit: &str) -> Result<String> {
    let mut cache = HashMap::new();
    view_tree_cached(objects, commit, &mut cache, 0)
}

fn view_tree_cached(
    objects: &mut Objects,
    commit: &str,
    cache: &mut HashMap<String, String>,
    nesting: usize,
) -> Result<String> {
    if let Some(tree) = cache.get(commit) {
        return Ok(tree.clone());
    }
    if nesting > 32 {
        return Err(NewgitError::Unsupported(format!(
            "commit {} mounts trackers more than 32 deep: a tracker mounted inside itself?",
            short(commit)
        )));
    }
    let found = objects.commit(commit)?;
    let pins = pins_in(objects, &found.tree)?;
    let mut tree = objects.set_path(&found.tree, PINS_DIR, None)?;
    if !pins.is_empty() {
        let present: BTreeMap<String, String> = pins
            .iter()
            .filter(|(_, pin)| objects.has_commit(pin).unwrap_or(false))
            .map(|(name, pin)| (name.clone(), pin.clone()))
            .collect();
        let mounts = mounts_of(objects, commit, &found.tree, &pins, &present)?;
        let mut ordered: Vec<(String, String)> = mounts.into_iter().collect();
        ordered
            .sort_by(|(_, left), (_, right)| depth(left).cmp(&depth(right)).then(left.cmp(right)));
        for (child, mount) in ordered {
            let child_tree = view_tree_cached(objects, &present[&child], cache, nesting + 1)?;
            tree = graft(objects, &tree, &mount, &child_tree)?;
        }
    }
    cache.insert(commit.to_owned(), tree.clone());
    Ok(tree)
}

/// Whether commit `commit`'s own definitions mount a tracker in Git that it
/// does not pin: a view commit, which only a split makes a root commit of.
/// One that mounts nothing is its own root commit.
pub fn defines_mounts(repo: &Utf8Path, commit: &str) -> Result<bool> {
    let mut objects = Objects::open(repo)?;
    defines_mounts_in(&mut objects, commit)
}

fn defines_mounts_in(objects: &mut Objects, commit: &str) -> Result<bool> {
    let found = objects.commit(commit)?;
    let pins = pins_in(objects, &found.tree)?;
    let Some(directory) = objects.subtree(&found.tree, DEFINITIONS_DIR)? else {
        return Ok(false);
    };
    for entry in objects.tree(&directory)? {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        let Some(tracker) = name.strip_suffix(".toml") else {
            continue;
        };
        if entry.is_tree() || pins.contains_key(tracker) {
            continue;
        }
        let text = read_blob_text(objects, &entry.id)?;
        let Ok(table) = toml::from_str::<toml::Table>(&text) else {
            continue;
        };
        let git = table.get("git").and_then(toml::Value::as_bool) == Some(true);
        if git && mount_in(&text).is_some_and(|mount| mount != ".") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The commits a root commit holds, by tracker: itself, and every pin,
/// recursively — the list a branch or checkpoint is indexed by. The first
/// copy of a tracker mounted twice is the one listed.
pub fn index(repo: &Utf8Path, root: &str, commit: &str) -> Result<State> {
    let mut objects = Objects::open(repo)?;
    index_with(&mut objects, root, commit)
}

pub(crate) fn index_with(objects: &mut Objects, root: &str, commit: &str) -> Result<State> {
    let mut state = State::new();
    let mut stack = vec![(root.to_owned(), commit.to_owned())];
    while let Some((name, commit)) = stack.pop() {
        if state.contains_key(&name) {
            continue;
        }
        state.insert(name, commit.clone());
        let Some(found) = objects.try_commit(&commit)? else {
            continue;
        };
        for (child, pin) in pins_in(objects, &found.tree)?.into_iter().rev() {
            stack.push((child, pin));
        }
    }
    Ok(state)
}

/// The root commit view commit `view` stands for, by the note composing or
/// splitting left on it.
pub fn read_root(repo: &Utf8Path, view: &str) -> Result<Option<(String, String)>> {
    let note = Command::new("git")
        .arg("-C")
        .arg(repo.as_str())
        .args(["notes", "--ref", NOTES_REF, "show", view])
        .output()
        .map_err(|source| NewgitError::io(repo, source))?;
    if !note.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&note.stdout)
        .lines()
        .find_map(|line| line.split_once(' '))
        .map(|(tracker, commit)| (tracker.to_owned(), commit.to_owned())))
}

// ── Split ────────────────────────────────────────────────────────────────

/// Why a change cannot be split. Each names what to do instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path's owner is not bound in this view.
    Unbound { path: String, tracker: String },
    /// The path's owner is a tracker outside Git: its files never leave
    /// through Git.
    OutsideGit {
        commit: String,
        path: String,
        tracker: String,
    },
    /// Every claim excludes the path, so nothing owns it.
    Unowned { commit: String, path: String },
    /// A path under `.newgit/pins/`, which only newgit writes.
    Reserved { commit: String, path: String },
    /// One commit's message would be carried by trackers with different
    /// audiences.
    MixedAudience {
        commit: String,
        subject: String,
        /// Each tracker carrying it, with its audience.
        trackers: Vec<(String, String)>,
    },
    /// One commit edits both copies of a tracker mounted twice.
    MountedTwice { commit: String, tracker: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unbound { path, tracker } => write!(
                f,
                "`{path}` belongs to `{tracker}`, which this instance has not bound: \
                 `newgit tracker use {tracker}`"
            ),
            Refusal::OutsideGit {
                commit,
                path,
                tracker,
            } => write!(
                f,
                "commit {} changes `{path}`, which belongs to `{tracker}`, a tracker outside \
                 Git: its files (secrets among them) never leave this machine through Git. Take \
                 it out of that commit (`git rm --cached {path}` then `git commit --amend`, or \
                 an interactive rebase if the commit is older) and try again",
                short(commit)
            ),
            Refusal::Unowned { commit, path } => write!(
                f,
                "commit {} changes `{path}`, which no tracker owns: the claims there exclude it \
                 (a `!` in a definition's `paths`). Take it out of that commit (`git rm \
                 --cached {path}`), or claim it",
                short(commit)
            ),
            Refusal::Reserved { commit, path } => write!(
                f,
                "commit {} changes `{path}`: `.newgit/pins/` is where newgit records which \
                 commit of each tracker a commit holds, and only newgit writes it",
                short(commit)
            ),
            Refusal::MixedAudience {
                commit,
                subject,
                trackers,
            } => {
                let listed = trackers
                    .iter()
                    .map(|(tracker, audience)| format!("`{tracker}` ({audience})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "commit {} (\"{subject}\") changes trackers with different audiences: \
                     {listed}. A commit has one message, and every tracker it changes keeps a \
                     copy, so each audience would read a message about work it may not see. \
                     Make one commit per audience (for the last commit, `git reset HEAD~` and \
                     commit each tracker's files on their own), then try again",
                    short(commit)
                )
            }
            Refusal::MountedTwice { commit, tracker } => write!(
                f,
                "commit {} edits both copies of `{tracker}`: {tracker} is mounted twice; edit one",
                short(commit)
            ),
        }
    }
}

#[derive(Debug)]
pub enum SplitError {
    Refused(Refusal),
    Git(NewgitError),
}

impl From<NewgitError> for SplitError {
    fn from(error: NewgitError) -> Self {
        SplitError::Git(error)
    }
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitError::Refused(refusal) => refusal.fmt(f),
            SplitError::Git(error) => error.fmt(f),
        }
    }
}

impl From<SplitError> for NewgitError {
    fn from(error: SplitError) -> Self {
        match error {
            SplitError::Refused(refusal) => NewgitError::SplitRefused(refusal.to_string()),
            SplitError::Git(error) => error,
        }
    }
}

/// What splitting needs to know about the person splitting.
pub struct SplitOptions<'a> {
    /// The root tracker's name.
    pub root: &'a str,
    /// The trackers this instance works on. Work in any other is refused,
    /// naming `newgit tracker use`.
    pub bound: &'a dyn Fn(&str) -> bool,
    /// Each tracker's audience, from policy (`access`'s `main`), when known;
    /// otherwise the definition in the commit is used.
    pub audience: &'a dyn Fn(&str) -> Option<String>,
    /// A view commit newgit made itself whose message may be carried by
    /// trackers of different audiences: a carve, `Create tracker `x``, which
    /// says nothing the definition it adds does not.
    pub newgit_made: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitOutcome {
    /// The root commit the tip stands for.
    pub root: String,
    /// Every tracker commit the tip holds, by tracker.
    pub index: State,
    /// Tracker commits made, as `(tracker, commit)` in order.
    pub made: Vec<(String, String)>,
}

struct Splitter<'a> {
    objects: Objects,
    options: &'a SplitOptions<'a>,
    /// View commit → root commit.
    mapped: HashMap<String, String>,
    notes: HashMap<String, String>,
    notes_loaded: bool,
    root_history: Option<HashSet<String>>,
    by_trailer: Option<HashMap<String, String>>,
    view_trees: HashMap<String, String>,
    made: Vec<(String, String)>,
    noted: Vec<(String, String)>,
    sign: HashMap<Vec<u8>, bool>,
}

/// One tracker's part of a view commit, split.
struct Piece {
    commit: String,
    /// Whether a new commit was made for it (rather than a pin reused).
    new: bool,
}

/// Split view commit `tip`, and every view commit behind it that is not
/// split yet, into root commits — one each — and commits in the children
/// they changed. Notes each view commit with the root commit it became.
pub fn split(
    repo: &Utf8Path,
    tip: &str,
    options: &SplitOptions<'_>,
) -> std::result::Result<SplitOutcome, SplitError> {
    let mut splitter = Splitter {
        objects: Objects::open(repo)?,
        options,
        mapped: HashMap::new(),
        notes: HashMap::new(),
        notes_loaded: false,
        root_history: None,
        by_trailer: None,
        view_trees: HashMap::new(),
        made: Vec::new(),
        noted: Vec::new(),
        sign: HashMap::new(),
    };
    // What is not split yet, oldest first: every parent, not only the first.
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack = vec![(tip.to_owned(), false)];
    while let Some((current, ready)) = stack.pop() {
        if ready {
            order.push(current);
            continue;
        }
        if !seen.insert(current.clone()) || splitter.root_of(&current)?.is_some() {
            continue;
        }
        stack.push((current.clone(), true));
        let commit = splitter.objects.commit(&current)?;
        for parent in commit.parents.iter().rev() {
            stack.push((parent.clone(), false));
        }
    }
    for view in &order {
        let root = splitter.split_view(view)?;
        splitter.mapped.insert(view.clone(), root.clone());
        splitter
            .noted
            .push((view.clone(), format!("{} {root}\n", options.root)));
    }
    splitter.objects.flush()?;
    write_notes(repo, NOTES_REF, "split", &splitter.noted)?;
    let root = splitter.root_of(tip)?.ok_or_else(|| {
        SplitError::Git(NewgitError::Unsupported(format!(
            "{} was not split",
            short(tip)
        )))
    })?;
    let index = index_with(&mut splitter.objects, options.root, &root)?;
    Ok(SplitOutcome {
        root,
        index,
        made: splitter.made,
    })
}

impl Splitter<'_> {
    /// The root commit view commit `view` already stands for: by its note,
    /// by being the root's own history (a project's history before any pin,
    /// which is its own view), or by a root commit naming it.
    fn root_of(&mut self, view: &str) -> Result<Option<String>> {
        if let Some(root) = self.mapped.get(view) {
            return Ok(Some(root.clone()));
        }
        if !self.notes_loaded {
            self.notes_loaded = true;
            let repo = self.objects.repo().to_path_buf();
            for line in git(&repo, &["notes", "--ref", NOTES_REF, "list"])
                .unwrap_or_default()
                .lines()
            {
                if let Some((blob, commit)) = line.split_once(' ') {
                    self.notes.insert(commit.to_owned(), blob.to_owned());
                }
            }
        }
        if let Some(blob) = self.notes.get(view).cloned() {
            let text = read_blob_text(&mut self.objects, &blob)?;
            if let Some((tracker, root)) = text.lines().find_map(|line| line.split_once(' '))
                && tracker == self.options.root
                && self.objects.has_commit(root)?
            {
                self.mapped.insert(view.to_owned(), root.to_owned());
                return Ok(Some(root.to_owned()));
            }
        }
        if self.root_history.is_none() {
            let repo = self.objects.repo().to_path_buf();
            let glob = format!("--glob=refs/newgit/trackers/{}/*", self.options.root);
            let commits: HashSet<String> = git(&repo, &["rev-list", &glob])
                .unwrap_or_default()
                .lines()
                .map(ToOwned::to_owned)
                .collect();
            let mut by_trailer = HashMap::new();
            for commit in &commits {
                let found = self.objects.commit(commit)?;
                if let Some(split) = parse_split(&found.message) {
                    by_trailer.insert(split.source, commit.clone());
                }
            }
            self.root_history = Some(commits);
            self.by_trailer = Some(by_trailer);
        }
        if let Some(root) = self
            .by_trailer
            .as_ref()
            .and_then(|map| map.get(view))
            .cloned()
        {
            self.mapped.insert(view.to_owned(), root.clone());
            return Ok(Some(root));
        }
        if (self
            .root_history
            .as_ref()
            .is_some_and(|history| history.contains(view))
            || self.made_before_mounts(view)?)
            && !defines_mounts_in(&mut self.objects, view)?
        {
            self.mapped.insert(view.to_owned(), view.to_owned());
            return Ok(Some(view.to_owned()));
        }
        Ok(None)
    }

    /// A view commit from before roots and mounts — `project` holds
    /// `.newgit/`, or `access` is not mounted — pins nothing, so it is its
    /// own root commit: the migration's "root's history is the view's
    /// history, commit for commit", for commits it did not see (work still
    /// in a workspace).
    fn made_before_mounts(&mut self, view: &str) -> Result<bool> {
        let commit = self.objects.commit(view)?;
        let project = format!("{DEFINITIONS_DIR}/{}.toml", crate::tracker::PROJECT_TRACKER);
        if self.objects.entry_at(&commit.tree, &project)?.is_some() {
            return Ok(true);
        }
        let access = format!("{DEFINITIONS_DIR}/{}.toml", crate::tracker::ACCESS_TRACKER);
        if let Some(entry) = self.objects.entry_at(&commit.tree, &access)? {
            let text = read_blob_text(&mut self.objects, &entry.id)?;
            return Ok(mount_in(&text).as_deref() == Some("."));
        }
        Ok(false)
    }

    fn view_tree_of(&mut self, commit: &str) -> Result<String> {
        if let Some(tree) = self.view_trees.get(commit) {
            return Ok(tree.clone());
        }
        let tree = view_tree(&mut self.objects, commit)?;
        self.view_trees.insert(commit.to_owned(), tree.clone());
        Ok(tree)
    }

    fn split_view(&mut self, view: &str) -> std::result::Result<String, SplitError> {
        let commit = self.objects.commit(view)?;
        let mut parents: Vec<String> = Vec::new();
        for parent in &commit.parents {
            let root = self.root_of(parent)?.ok_or_else(|| {
                NewgitError::Unsupported(format!("{} was not split", short(parent)))
            })?;
            if !parents.contains(&root) {
                parents.push(root);
            }
        }
        let definitions = self.flattened(&commit.tree, Utf8Path::new("."))?;
        self.refuse(&commit, &definitions)?;

        let mut made_here: Vec<(String, String)> = Vec::new();
        let root = self.options.root.to_owned();
        let (own, pins) =
            self.split_part(&commit, &root, &commit.tree, &parents, &mut made_here)?;
        let tree = self.with_pins(&own, &pins)?;

        // The message goes on the commit that holds the work: the root when
        // it changed its own files or nothing else holds the work, the
        // child commits otherwise.
        let own_changed = match parents.first() {
            Some(first) => {
                let first = self.objects.commit(first)?;
                self.objects.set_path(&first.tree, PINS_DIR, None)? != own
            }
            None => own != EMPTY_TREE,
        };
        let on_root = own_changed || made_here.is_empty();
        let mut carriers: Vec<String> = made_here.iter().map(|(name, _)| name.clone()).collect();
        if on_root {
            carriers.insert(0, root.clone());
        }
        carriers.dedup();
        if commit.parents.len() <= 1 && self.options.newgit_made != Some(view) {
            let audiences: Vec<(String, String)> = carriers
                .iter()
                .map(|name| {
                    let audience = (self.options.audience)(name)
                        .or_else(|| {
                            definitions
                                .iter()
                                .find(|definition| &definition.name == name)
                                .map(|definition| definition.audience.clone())
                        })
                        .unwrap_or_else(|| "project-devs".to_owned());
                    (name.clone(), audience)
                })
                .collect();
            let distinct: BTreeSet<&String> =
                audiences.iter().map(|(_, audience)| audience).collect();
            if distinct.len() > 1 {
                return Err(SplitError::Refused(Refusal::MixedAudience {
                    commit: view.to_owned(),
                    subject: commit.subject(),
                    trackers: audiences,
                }));
            }
        }
        let message = if on_root {
            with_split_trailers(&commit.message, view, &commit.headers)
        } else {
            let names: Vec<&str> = made_here
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            with_split_trailers(
                format!("Update {}\n", names.join(", ")).as_bytes(),
                view,
                &commit.headers,
            )
        };
        let made = self.make(&tree, &parents, &commit, &message)?;
        self.made.push((root, made.clone()));
        self.made.extend(made_here);
        Ok(made)
    }

    /// Refuse the paths this commit changes that cannot be split: what
    /// nothing owns, what a tracker outside Git owns, and `.newgit/pins/`.
    /// A path counts as changed when it differs from every parent: what a
    /// merge takes whole from one side was checked when that side was.
    fn refuse(
        &mut self,
        commit: &Commit,
        definitions: &[TrackerDefinition],
    ) -> std::result::Result<(), SplitError> {
        let mut changed: Option<BTreeSet<String>> = None;
        if commit.parents.is_empty() {
            changed = Some(self.objects.files(&commit.tree)?.into_keys().collect());
        }
        for parent in &commit.parents {
            let parent_tree = self.objects.commit(parent)?.tree;
            let paths: BTreeSet<String> = self
                .objects
                .changed_paths(Some(&parent_tree), Some(&commit.tree))?
                .into_iter()
                .collect();
            changed = Some(match changed {
                None => paths,
                Some(before) => before.intersection(&paths).cloned().collect(),
            });
        }
        let changed = changed.unwrap_or_default();
        if changed.is_empty() {
            return Ok(());
        }
        for path in changed {
            if path == PINS_DIR || path.starts_with(&format!("{PINS_DIR}/")) {
                return Err(SplitError::Refused(Refusal::Reserved {
                    commit: commit.id.clone(),
                    path,
                }));
            }
            // Deleting a file is how it comes out: never refused.
            if self.objects.entry_at(&commit.tree, &path)?.is_none() {
                continue;
            }
            match owner_of(definitions, Utf8Path::new(&path)) {
                None => {
                    return Err(SplitError::Refused(Refusal::Unowned {
                        commit: commit.id.clone(),
                        path,
                    }));
                }
                Some(owner) if !owner.git => {
                    return Err(SplitError::Refused(Refusal::OutsideGit {
                        commit: commit.id.clone(),
                        path,
                        tracker: owner.name.clone(),
                    }));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Every definition in the view `tree` (the root's), with each mounted
    /// tracker's own definitions brought in under its mount: what decides
    /// who owns a path. The root claims `.` when it has no definition.
    fn flattened(&mut self, tree: &str, at: &Utf8Path) -> Result<Vec<TrackerDefinition>> {
        let mut out = Vec::new();
        let mut stack = vec![(
            tree.to_owned(),
            at.to_path_buf(),
            self.options.root.to_owned(),
        )];
        let mut guard = 0;
        while let Some((tree, at, owner)) = stack.pop() {
            guard += 1;
            if guard > 64 {
                break;
            }
            let definitions = self.definitions_in(&tree)?;
            if at.as_str() == "."
                && !definitions
                    .iter()
                    .any(|definition| definition.name == owner)
            {
                let mut root = TrackerDefinition::root();
                root.name = owner.clone();
                out.push(root);
            }
            for mut definition in definitions {
                if at.as_str() != "." {
                    if definition.name == owner {
                        continue;
                    }
                    let place = |path: &Utf8PathBuf| {
                        if path.as_str() == "." {
                            at.clone()
                        } else {
                            at.join(path)
                        }
                    };
                    definition.paths = definition.paths.iter().map(place).collect();
                    definition.exclusions = definition.exclusions.iter().map(place).collect();
                    definition.mount = place(&definition.mount);
                }
                if definition.git && definition.mounted() && definition.name != owner {
                    let relative = definition
                        .mount
                        .strip_prefix(&at)
                        .map(Utf8Path::to_path_buf)
                        .unwrap_or_else(|_| definition.mount.clone());
                    if let Some(subtree) = self.objects.subtree(&tree, relative.as_str())? {
                        stack.push((subtree, definition.mount.clone(), definition.name.clone()));
                    }
                }
                out.push(definition);
            }
        }
        Ok(out)
    }

    /// The definitions a tree holds in its own `.newgit/trackers/`.
    fn definitions_in(&mut self, tree: &str) -> Result<Vec<TrackerDefinition>> {
        let Some(directory) = self.objects.subtree(tree, DEFINITIONS_DIR)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in self.objects.tree(&directory)? {
            let name = String::from_utf8_lossy(&entry.name).into_owned();
            let Some(tracker) = name.strip_suffix(".toml") else {
                continue;
            };
            if entry.is_tree() {
                continue;
            }
            let text = read_blob_text(&mut self.objects, &entry.id)?;
            let path = Utf8PathBuf::from(format!("{DEFINITIONS_DIR}/{name}"));
            out.push(TrackerDefinition::from_text(tracker, &path, &text)?);
        }
        Ok(out)
    }

    /// Split `tracker`'s part of a view commit: `tree` is its view subtree
    /// (its own files, and its children's under their mounts), `parents`
    /// its commits at the view commit's parents. Returns its own tree
    /// (children taken out) and the pin each child gets.
    fn split_part(
        &mut self,
        commit: &Commit,
        tracker: &str,
        tree: &str,
        parents: &[String],
        made: &mut Vec<(String, String)>,
    ) -> std::result::Result<(String, BTreeMap<String, String>), SplitError> {
        let definitions = self.definitions_in(tree)?;
        let mut children: Vec<TrackerDefinition> = definitions
            .iter()
            .filter(|definition| definition.git && definition.name != tracker)
            .cloned()
            .collect();
        children.sort_by(|left, right| {
            depth(right.mount.as_str())
                .cmp(&depth(left.mount.as_str()))
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut parent_pins: Vec<BTreeMap<String, String>> = Vec::new();
        for parent in parents {
            let found = self.objects.commit(parent)?;
            parent_pins.push(pins_in(&mut self.objects, &found.tree)?);
        }
        let mut own = self.objects.set_path(tree, PINS_DIR, None)?;
        let mut pins = BTreeMap::new();
        for child in &children {
            let mount = child.mount.as_str();
            let shares_mount = definitions.iter().any(|other| {
                other.name != child.name && other.name != tracker && other.mount == child.mount
            });
            let whole = mount != "."
                && !shares_mount
                && child.exclusions.is_empty()
                && child.paths == [child.mount.clone()];
            let part = if whole {
                let part = self.objects.subtree(tree, mount)?;
                own = self.objects.set_path(&own, mount, None)?;
                // A sibling mounted deeper inside this mount is its own.
                let mut part = part;
                for other in &children {
                    if other.name != child.name
                        && let Ok(inner) = other.mount.strip_prefix(&child.mount)
                        && !inner.as_str().is_empty()
                        && let Some(current) = part.clone()
                    {
                        let trimmed = self.objects.set_path(&current, inner.as_str(), None)?;
                        part = (trimmed != EMPTY_TREE).then_some(trimmed);
                    }
                }
                part
            } else {
                // What it claims at its mount, beside whatever else is
                // mounted there: the longest claim wins, and its
                // exclusions are nobody's.
                let relative = |path: &Utf8Path| -> String {
                    if mount == "." {
                        return path.to_string();
                    }
                    match path.strip_prefix(&child.mount) {
                        Ok(inner) if inner.as_str().is_empty() => ".".to_owned(),
                        Ok(inner) => inner.to_string(),
                        Err(_) => path.to_string(),
                    }
                };
                let base = if mount == "." {
                    Some(tree.to_owned())
                } else {
                    self.objects.subtree(tree, mount)?
                };
                let mut part = EMPTY_TREE.to_owned();
                if let Some(base) = &base {
                    for claim in &child.paths {
                        let at = relative(claim);
                        if at == "." {
                            part = self.objects.overlay(&part, base)?;
                        } else if let Some(entry) = self.objects.entry_at(base, &at)? {
                            part =
                                self.objects
                                    .set_path(&part, &at, Some((entry.mode, entry.id)))?;
                        }
                    }
                }
                let mut out: Vec<String> =
                    child.exclusions.iter().map(|path| relative(path)).collect();
                for other in definitions.iter().filter(|other| {
                    other.name != child.name && other.name != tracker && other.mount == child.mount
                }) {
                    for claim in &other.paths {
                        if other.claim_on(claim) > child.claim_on(claim) {
                            out.push(relative(claim));
                        }
                    }
                }
                for other in &children {
                    if other.name != child.name
                        && other.mount != child.mount
                        && (mount == "." || other.mount.starts_with(&child.mount))
                    {
                        out.push(relative(&other.mount));
                    }
                }
                for path in out {
                    if path != "." {
                        part = self.objects.set_path(&part, &path, None)?;
                    }
                }
                for file in self.objects.files(&part)?.into_keys() {
                    let at = if mount == "." {
                        file
                    } else {
                        format!("{mount}/{file}")
                    };
                    own = self.objects.set_path(&own, &at, None)?;
                }
                (part != EMPTY_TREE).then_some(part)
            };
            let mut before: Vec<String> = Vec::new();
            for pins_then in &parent_pins {
                if let Some(pin) = pins_then.get(&child.name)
                    && !before.contains(pin)
                {
                    before.push(pin.clone());
                }
            }
            if part.is_none() && before.is_empty() {
                continue;
            }
            // A child whose history is not here (a reader who may not read
            // it): its files are not in the view, so its pin carries over.
            let present = {
                let mut all = true;
                for pin in &before {
                    all &= self.objects.has_commit(pin)?;
                }
                all
            };
            if !present {
                let pin = self.descendant(&before)?.ok_or_else(|| {
                    NewgitError::Unsupported(format!(
                        "commit {} merges two lines of `{}`, which is not here to merge",
                        short(&commit.id),
                        child.name
                    ))
                })?;
                pins.insert(child.name.clone(), pin);
                continue;
            }
            let part_tree = part.clone().unwrap_or_else(|| EMPTY_TREE.to_owned());
            let mut reused = None;
            for pin in &before {
                if self.view_tree_of(pin)? != part_tree {
                    continue;
                }
                let mut descends = true;
                for other in &before {
                    if other != pin && !self.is_ancestor(other, pin)? {
                        descends = false;
                    }
                }
                if descends {
                    reused = Some(pin.clone());
                    break;
                }
            }
            if let Some(pin) = reused {
                pins.insert(child.name.clone(), pin);
                continue;
            }
            if !(self.options.bound)(&child.name) {
                let changed = self
                    .first_change(&before, &part_tree, mount)?
                    .unwrap_or_else(|| mount.to_owned());
                return Err(SplitError::Refused(Refusal::Unbound {
                    path: changed,
                    tracker: child.name.clone(),
                }));
            }
            if made.iter().any(|(name, _)| name == &child.name) {
                return Err(SplitError::Refused(Refusal::MountedTwice {
                    commit: commit.id.clone(),
                    tracker: child.name.clone(),
                }));
            }
            let piece = self.split_child(commit, &child.name, &part_tree, &before, made)?;
            if piece.new {
                made.push((child.name.clone(), piece.commit.clone()));
            }
            pins.insert(child.name.clone(), piece.commit);
        }
        Ok((own, pins))
    }

    fn split_child(
        &mut self,
        commit: &Commit,
        tracker: &str,
        tree: &str,
        parents: &[String],
        made: &mut Vec<(String, String)>,
    ) -> std::result::Result<Piece, SplitError> {
        let (own, pins) = self.split_part(commit, tracker, tree, parents, made)?;
        let tree = self.with_pins(&own, &pins)?;
        // Every child commit carries the view commit's metadata, without
        // trailers: a tracker's own history stays clean.
        let message = commit.message.clone();
        let made_commit = self.make(&tree, parents, commit, &message)?;
        Ok(Piece {
            commit: made_commit,
            new: true,
        })
    }

    fn with_pins(&mut self, own: &str, pins: &BTreeMap<String, String>) -> Result<String> {
        let mut tree = own.to_owned();
        for (child, pin) in pins {
            tree = self.objects.set_path(
                &tree,
                &format!("{PINS_DIR}/{child}"),
                Some((GITLINK_MODE.to_vec(), pin.clone())),
            )?;
        }
        Ok(tree)
    }

    /// A commit of tree `tree` on `parents` with `commit`'s author,
    /// committer, and encoding, and `message`; signed as its author when
    /// this machine signs that author's commits.
    fn make(
        &mut self,
        tree: &str,
        parents: &[String],
        commit: &Commit,
        message: &[u8],
    ) -> Result<String> {
        let encoding = Commit::encoding_header(&commit.headers);
        if self.signs(&commit.author)? {
            self.objects.flush()?;
            return sign_commit(self.objects.repo(), tree, parents, commit, message);
        }
        Ok(self.objects.make_commit(
            tree,
            parents,
            &commit.author,
            &commit.committer,
            &encoding,
            message,
        ))
    }

    fn signs(&mut self, author: &[u8]) -> Result<bool> {
        if let Some(answer) = self.sign.get(author) {
            return Ok(*answer);
        }
        let answer = signs_for(self.objects.repo(), &String::from_utf8_lossy(author));
        self.sign.insert(author.to_vec(), answer);
        Ok(answer)
    }

    fn is_ancestor(&mut self, ancestor: &str, descendant: &str) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        // Git answers for what it has: commits made in this split first.
        self.objects.flush()?;
        let output = Command::new("git")
            .arg("-C")
            .arg(self.objects.repo().as_str())
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|source| NewgitError::io(self.objects.repo(), source))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(NewgitError::SourceCommand {
                command: format!("git merge-base --is-ancestor {ancestor} {descendant}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }

    /// The one of `commits` every other is an ancestor of.
    fn descendant(&mut self, commits: &[String]) -> Result<Option<String>> {
        'candidates: for candidate in commits {
            for other in commits {
                if other != candidate && !self.is_ancestor(other, candidate)? {
                    continue 'candidates;
                }
            }
            return Ok(Some(candidate.clone()));
        }
        Ok(None)
    }

    /// A path under `mount` that differs from the first of `before`'s views.
    fn first_change(
        &mut self,
        before: &[String],
        part: &str,
        mount: &str,
    ) -> Result<Option<String>> {
        let base = match before.first() {
            Some(pin) => Some(self.view_tree_of(pin)?),
            None => None,
        };
        let changed = self.objects.changed_paths(base.as_deref(), Some(part))?;
        Ok(changed.into_iter().next().map(|path| {
            if mount == "." {
                path
            } else {
                format!("{mount}/{path}")
            }
        }))
    }
}

/// A commit made by `git commit-tree -S`, signed with this machine's key.
fn sign_commit(
    repo: &Utf8Path,
    tree: &str,
    parents: &[String],
    commit: &Commit,
    message: &[u8],
) -> Result<String> {
    let mut args = vec!["commit-tree".to_owned(), tree.to_owned(), "-S".to_owned()];
    for parent in parents {
        args.extend(["-p".to_owned(), parent.clone()]);
    }
    let mut env = Vec::new();
    for (role, line) in [("AUTHOR", &commit.author), ("COMMITTER", &commit.committer)] {
        let line = String::from_utf8_lossy(line);
        if let Some((name, rest)) = line.split_once(" <")
            && let Some((email, date)) = rest.split_once("> ")
        {
            env.push((format!("GIT_{role}_NAME"), name.to_owned()));
            env.push((format!("GIT_{role}_EMAIL"), email.to_owned()));
            env.push((format!("GIT_{role}_DATE"), format!("@{date}")));
        }
    }
    let out = run(
        Command::new("git")
            .arg("-C")
            .arg(repo.as_str())
            .args(&args)
            .envs(env),
        "git commit-tree -S",
        Some(message),
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// Whether a commit by `author` (`Name <email> …`) is signed when newgit
/// makes it here: only when this machine signs its commits
/// (`commit.gpgSign`) and the author is this machine's user — newgit signs
/// with your key, never on someone else's behalf.
fn signs_for(repo: &Utf8Path, author: &str) -> bool {
    let config = |key: &str| {
        git(repo, &["config", "--get", key])
            .ok()
            .map(|value| value.trim().to_owned())
    };
    let signing = git(repo, &["config", "--bool", "--get", "commit.gpgSign"])
        .is_ok_and(|value| value.trim() == "true");
    let email = author
        .split_once(" <")
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(email, _)| email);
    signing && email.is_some() && config("user.email").as_deref() == email
}

/// The first commit among `rev_list` (arguments to `git log`, so a caller
/// says what is new: `<tip> --not --all`) that adds or changes a path a
/// tracker outside Git owns, with that path and tracker. Every parent of a
/// merge is diffed, so a secret cannot ride in on one; deleting a secret
/// file is what removing it looks like, so it is never refused. `env` lets
/// the caller see objects Git is still holding in quarantine.
pub fn find_outside_git(
    repo: &Utf8Path,
    rev_list: &[&str],
    env: &[(String, String)],
    all: &[TrackerDefinition],
) -> Result<Option<(String, String, String)>> {
    // Each commit starts with \x01 — no path holds that byte, so a file
    // named like a commit id cannot be mistaken for one.
    let mut args = vec![
        "log",
        "-m",
        "--no-renames",
        "--name-only",
        "--diff-filter=d",
        "-z",
        "--format=%x01%H",
    ];
    args.extend_from_slice(rev_list);
    let output = run(
        Command::new("git")
            .arg("-C")
            .arg(repo.as_str())
            .args(&args)
            .envs(env.iter().map(|(key, value)| (key, value))),
        "git log",
        None,
    )?;
    let output = String::from_utf8_lossy(&output);
    let mut commit = String::new();
    for field in output.split('\0') {
        let field = field.trim_start_matches('\n');
        if let Some(id) = field.strip_prefix('\x01') {
            commit = id.to_owned();
            continue;
        }
        if field.is_empty() {
            continue;
        }
        if let Some(owner) = owner_of(all, Utf8Path::new(field)).filter(|owner| !owner.git) {
            return Ok(Some((commit, field.to_owned(), owner.name.clone())));
        }
    }
    Ok(None)
}

/// Note every `(commit, note)` in one notes commit on `notes_ref` — one
/// `git fast-import`, however many commits. (`git notes add` once per
/// commit rewrites the whole notes tree each time.) Fixed identity and
/// date, so the notes are deterministic too.
pub(crate) fn write_notes(
    repo: &Utf8Path,
    notes_ref: &str,
    label: &str,
    notes: &[(String, String)],
) -> Result<()> {
    if notes.is_empty() {
        return Ok(());
    }
    let parent = git(repo, &["rev-parse", "-q", "--verify", notes_ref])
        .ok()
        .map(|parent| parent.trim().to_owned())
        .filter(|parent| !parent.is_empty());
    let mut stream = Vec::new();
    writeln!(stream, "commit {notes_ref}").ok();
    writeln!(
        stream,
        "committer newgit <newgit@localhost> 946684800 +0000"
    )
    .ok();
    writeln!(stream, "data {}\n{label}", label.len()).ok();
    if let Some(parent) = parent {
        writeln!(stream, "from {parent}").ok();
    }
    for (commit, note) in notes {
        writeln!(stream, "N inline {commit}").ok();
        writeln!(stream, "data {}", note.len()).ok();
        stream.extend_from_slice(note.as_bytes());
        stream.push(b'\n');
    }
    stream.push(b'\n');
    run(
        Command::new("git").arg("-C").arg(repo.as_str()).args([
            "fast-import",
            "--quiet",
            "--date-format=raw",
        ]),
        "git fast-import",
        Some(&stream),
    )
    .map(drop)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn missing(id: &str) -> NewgitError {
    NewgitError::SourceCommand {
        command: "compose".to_owned(),
        stderr: format!("commit {id} is not composed"),
    }
}

fn git(repo: &Utf8Path, args: &[&str]) -> Result<String> {
    let out = run(
        Command::new("git").arg("-C").arg(repo.as_str()).args(args),
        &format!("git {}", args.join(" ")),
        None,
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: &[u8], headers: &[u8]) {
        let source = "a".repeat(40);
        let split = with_split_trailers(message, &source, headers);
        let back = parse_split(&split).expect("a split commit's message");
        assert_eq!(back.source, source);
        assert_eq!(back.message, message, "message of {split:?}");
        assert_eq!(back.headers, headers, "headers of {split:?}");
    }

    #[test]
    fn split_trailers_give_back_the_exact_message_and_headers() {
        for message in [
            &b""[..],
            b"\n",
            b"subject",
            b"subject\n",
            b"subject\n\n",
            b"subject\n\nbody\n\nCo-authored-by: Bo <bo@example.com>",
            b"subject\r\n\r\nbody\r\n",
            b"subject\n\n  indented last paragraph\n",
            b"subject\n\nNewgit-Split: in the body\n",
            b"caf\xe9 in Latin-1\n",
        ] {
            round_trip(message, b"");
            round_trip(
                message,
                b"gpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFc\r\n -----END PGP SIGNATURE-----\n",
            );
            round_trip(
                message,
                b"encoding ISO-8859-1\nmergetag object abc\n type commit\n",
            );
        }
    }

    #[test]
    fn a_message_without_our_trailer_is_not_a_split() {
        assert!(parse_split(b"subject\n\nCo-authored-by: Bo\n").is_none());
        assert!(parse_split(b"subject\n\nNewgit-Split: not-an-id\n").is_none());
    }
}
