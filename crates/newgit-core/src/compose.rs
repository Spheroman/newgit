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
//!   files under its mount. That is all a child contributes: its tree at the
//!   pin, and the author, dates, and message of that one commit when it holds
//!   the work. A child's parents are never read, so its own history stays in
//!   its own repo, no view reaches it, and a store holding root's history and
//!   each pinned commit alone composes the very same ids. A commit that adds
//!   a mount or moves a pin, from whatever history, is one ordinary view
//!   commit. Rebuilt commits come back as the originals they were split from,
//!   signatures included, wherever their bytes allow.
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
use crate::tracker::{
    DEFINITIONS_DIR, TrackerDefinition, child_id, leaf_name, owner_of, tracker_path,
};

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

fn depth(mount: &str) -> usize {
    if mount == "." {
        0
    } else {
        mount.split('/').count()
    }
}

/// `mounts` in the order they are grafted: shallowest first.
fn by_depth(mounts: &BTreeMap<String, String>) -> Vec<(&String, &String)> {
    let mut ordered: Vec<(&String, &String)> = mounts.iter().collect();
    ordered.sort_by(|(left_name, left), (right_name, right)| {
        depth(left)
            .cmp(&depth(right))
            .then_with(|| left.cmp(right))
            .then_with(|| left_name.cmp(right_name))
    });
    ordered
}

#[derive(Debug, Clone)]
struct Composed {
    /// The view commit.
    id: String,
    tree: String,
}

/// A root commit with its pins and where each readable one is mounted.
type Shape = (Commit, BTreeMap<String, String>, BTreeMap<String, String>);

struct Composer<'a> {
    objects: Objects,
    options: &'a ComposeOptions<'a>,
    /// Root commit → its view commit.
    views: HashMap<String, Composed>,
    /// Pinned child commit → its tree as this reader sees it.
    children: HashMap<String, String>,
    /// Root commit → the children its view expanded.
    expanded: HashMap<String, BTreeSet<String>>,
    /// Trackers being expanded right now, outermost first: a cycle guard.
    composing: Vec<String>,
    notes: Vec<(String, String)>,
    canonical: Vec<(String, String)>,
}

/// Compose the views rooted at tracker `root`, one per `(view ref, root
/// commit)` in `tips`, into `repo`, which must hold root's history and, of
/// every child the reader may read, each commit that history pins (and what
/// those pin): no child's parents are read. Each view ref is set to its
/// view's tip.
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
        children: HashMap::new(),
        expanded: HashMap::new(),
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
    composer.composing.push(root.to_owned());
    composer.compose_root(root, &commits)?;
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
    fn compose_root(&mut self, name: &str, tips: &[String]) -> Result<()> {
        let mut args = vec![
            "rev-list".to_owned(),
            "--topo-order".to_owned(),
            "--reverse".to_owned(),
        ];
        args.extend(tips.iter().cloned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let walk: Vec<String> = git(self.objects.repo(), &args)?
            .lines()
            .map(ToOwned::to_owned)
            .collect();

        // First every child this history pins, so a commit always mounts a
        // tree that is already expanded.
        let mut shapes: Vec<Shape> = Vec::with_capacity(walk.len());
        for id in &walk {
            let commit = self.objects.commit(id)?;
            let all_pins = pins_in(&mut self.objects, &commit.tree)?;
            let mounts = self.expand_children(name, &commit, &all_pins)?;
            shapes.push((commit, all_pins, mounts));
        }

        let mut own_trees: HashMap<String, String> = HashMap::new();
        for (commit, all_pins, mounts) in shapes {
            self.compose_one(name, &commit, &all_pins, &mounts, &mut own_trees)?;
        }
        Ok(())
    }

    /// Expand each child `commit` pins that this reader gets, and say where
    /// each of them is mounted.
    fn expand_children(
        &mut self,
        owner: &str,
        commit: &Commit,
        all_pins: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let mut readable = BTreeMap::new();
        for (child, pin) in all_pins {
            let id = child_id(owner, child);
            if !(self.options.readable)(&id) {
                continue;
            }
            // A pin its tracker's own history does not hold (another
            // tracker's commit under this name, or one never pushed) is left
            // out at this commit, as for a reader who may not read it: what
            // the reader gets is never someone else's content.
            let admitted = self.options.admit.is_none_or(|admit| admit(&id, pin));
            if admitted && self.objects.has_commit(pin)? {
                readable.insert(child.clone(), pin.clone());
            }
        }
        let mounts = mounts_of(
            &mut self.objects,
            &commit.id,
            &commit.tree,
            all_pins,
            &readable,
        )?;
        for (child, pin) in &readable {
            self.expand_child(&child_id(owner, child), pin)?;
        }
        Ok(mounts)
    }

    /// `pin`'s tree as this reader sees it, kept in `children`: this one
    /// commit and what it pins, never its parents.
    fn expand_child(&mut self, name: &str, pin: &str) -> Result<()> {
        if self.children.contains_key(pin) {
            return Ok(());
        }
        if self.composing.iter().any(|outer| outer == name) {
            return Err(NewgitError::Unsupported(format!(
                "`{name}` is mounted inside itself ({} → {name})",
                self.composing.join(" → ")
            )));
        }
        self.composing.push(name.to_owned());
        let result = self.child_tree(name, pin);
        self.composing.pop();
        self.children.insert(pin.to_owned(), result?);
        Ok(())
    }

    fn child_tree(&mut self, name: &str, pin: &str) -> Result<String> {
        let commit = self.objects.commit(pin)?;
        let all_pins = pins_in(&mut self.objects, &commit.tree)?;
        let mounts = self.expand_children(name, &commit, &all_pins)?;
        let own = self.objects.set_path(&commit.tree, PINS_DIR, None)?;
        self.with_children(&own, &by_depth(&mounts), &all_pins)
    }

    /// `own` with each child in `ordered` grafted at its mount.
    fn with_children(
        &mut self,
        own: &str,
        ordered: &[(&String, &String)],
        all_pins: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut tree = own.to_owned();
        for (child, mount) in ordered {
            let pin = &all_pins[*child];
            let view = self
                .children
                .get(pin)
                .cloned()
                .ok_or_else(|| missing(pin))?;
            tree = graft(&mut self.objects, &tree, mount, &view)?;
        }
        Ok(tree)
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
        let ordered = by_depth(mounts);
        let tree = self.with_children(&own, &ordered, all_pins)?;

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
            let parent_expanded = self.expanded.get(&parent.id).cloned().unwrap_or_default();
            if parent_own == own
                && !moved.is_empty()
                && moved
                    .iter()
                    .all(|child| !mounts.contains_key(*child) && !parent_expanded.contains(*child))
                && let Some(view) = self.views.get(&parent.id).cloned()
            {
                self.expanded.insert(commit.id.clone(), parent_expanded);
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

        // A merge left with nothing of its own by what this reader may not
        // read: its sides differ in a withheld pin, and the side that only
        // moved it was dropped, so what is left is one parent, or an
        // ancestor of it, with this tree.
        if commit.parents.len() > 1
            && self.joins_withheld(all_pins, mounts, &parent_commits)?
            && let Some(parent) = self.empty_merge(&commit.parents, &tree)?
        {
            let view = self.views[&parent].clone();
            let expanded = self.expanded.get(&parent).cloned().unwrap_or_default();
            self.expanded.insert(commit.id.clone(), expanded);
            self.views.insert(commit.id.clone(), view);
            return Ok(());
        }

        let split = parse_split(&commit.message);

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
                let holder = if own_unchanged {
                    self.work_holder(&ordered, all_pins, &parent_commits)?
                } else {
                    None
                };
                match holder {
                    Some(holder) => {
                        let holder = self.objects.commit(&holder)?;
                        let message = parse_split(&holder.message)
                            .map_or(holder.message.clone(), |carried| carried.message);
                        (
                            holder.author,
                            holder.committer,
                            message,
                            split.headers.clone(),
                            split.source.clone(),
                        )
                    }
                    None => (
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
        self.notes
            .push((id.clone(), format!("{name} {}\n", commit.id)));
        self.expanded
            .insert(commit.id.clone(), mounts.keys().cloned().collect());
        self.views.insert(commit.id.clone(), Composed { id, tree });
        Ok(())
    }

    /// The child commit holding the work of a split commit that changed
    /// none of its own files: the first child, by mount, whose pin moved at
    /// this commit. It carries the work's metadata (*Which commit carries the
    /// message* in docs/design/trackers.md), so it is read as it stands.
    fn work_holder(
        &mut self,
        ordered: &[(&String, &String)],
        all_pins: &BTreeMap<String, String>,
        parent_commits: &[Commit],
    ) -> Result<Option<String>> {
        for (child, _) in ordered {
            let tip = &all_pins[*child];
            let mut moved = true;
            for parent in parent_commits {
                let parent_pins = pins_in(&mut self.objects, &parent.tree)?;
                if parent_pins.get(*child) == Some(tip)
                    && self
                        .expanded
                        .get(&parent.id)
                        .is_some_and(|expanded| expanded.contains(*child))
                {
                    moved = false;
                }
            }
            if moved {
                return Ok(Some(tip.clone()));
            }
        }
        Ok(None)
    }

    /// Whether a merge's parents differ in the pin of a child not expanded
    /// here.
    fn joins_withheld(
        &mut self,
        all_pins: &BTreeMap<String, String>,
        mounts: &BTreeMap<String, String>,
        parent_commits: &[Commit],
    ) -> Result<bool> {
        let mut sides = Vec::with_capacity(parent_commits.len());
        for parent in parent_commits {
            sides.push(pins_in(&mut self.objects, &parent.tree)?);
        }
        Ok(all_pins
            .keys()
            .filter(|child| !mounts.contains_key(*child))
            .any(|child| {
                sides
                    .iter()
                    .any(|pins| pins.get(child) != sides[0].get(child))
            }))
    }

    /// The tracker commit among `parents` whose view has `tree` and is ahead
    /// of every other parent's, if there is one: a merge adding nothing to it.
    fn empty_merge(&mut self, parents: &[String], tree: &str) -> Result<Option<String>> {
        self.objects.flush()?;
        for candidate in parents {
            let view = self.views[candidate].clone();
            if view.tree != tree {
                continue;
            }
            let mut ahead = true;
            for other in parents {
                let other = &self.views[other].id;
                ahead &= other == &view.id || is_ancestor(self.objects.repo(), other, &view.id)?;
            }
            if ahead {
                return Ok(Some(candidate.clone()));
            }
        }
        Ok(None)
    }
}

/// Whether commit `ancestor` is behind commit `descendant`.
fn is_ancestor(repo: &Utf8Path, ancestor: &str, descendant: &str) -> Result<bool> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(repo.as_str())
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .map_err(|source| NewgitError::io(repo, source))?
        .status
        .success())
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
        state.insert(name.clone(), commit.clone());
        let Some(found) = objects.try_commit(&commit)? else {
            continue;
        };
        for (child, pin) in pins_in(objects, &found.tree)?.into_iter().rev() {
            stack.push((child_id(&name, &child), pin));
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

/// Every definition in view tree `tree` of a project rooted at `root`: what
/// decides who owns a path.
pub fn view_definitions(repo: &Utf8Path, root: &str, tree: &str) -> Result<Vec<TrackerDefinition>> {
    let bound = |_: &str| true;
    let audience = |_: &str| None;
    let options = SplitOptions {
        root,
        bound: &bound,
        audience: &audience,
        newgit_made: None,
        need_history: &|_| Ok(()),
    };
    Splitter::new(Objects::open(repo)?, &options).flattened(tree, Utf8Path::new("."))
}

/// Bring commits of an export target that did not come from here into this
/// project's view: `walk`, oldest first, is what the target has that this
/// project does not, and `known` says which view commit a commit that came
/// from here stands for. Each comes back as a view commit on those parents,
/// with the files of every `withheld` tracker put back as its parents have
/// them, and a note naming the original, so a split records it and composing
/// for the target gives that very commit back. Parents that differ there
/// give the one ahead: a merge of work already here must not undo it. Two
/// that diverged cannot be told apart, and are refused.
///
/// A commit touching a withheld tracker's mount, or sharing no history with
/// the project, is refused: the mirror holds nothing of a withheld tracker,
/// so there is no telling what it meant.
pub fn bring_in(
    repo: &Utf8Path,
    target: &str,
    walk: &[String],
    withheld: &[TrackerDefinition],
    known: &mut dyn FnMut(&str) -> Result<Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut objects = Objects::open(repo)?;
    let mut mapped: BTreeMap<String, String> = BTreeMap::new();
    let mut rebuilt: Vec<(String, String)> = Vec::new();
    for id in walk {
        if let Some(ours) = known(id)? {
            mapped.insert(id.clone(), ours);
            continue;
        }
        let commit = objects.commit(id)?;
        if commit.parents.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "commit {} on `{target}` shares no history with the project, so it cannot be \
                 brought in as work",
                short(id),
            )));
        }
        let mut parents = Vec::new();
        for parent in &commit.parents {
            let ours = match mapped.get(parent) {
                Some(ours) => ours.clone(),
                None => known(parent)?.unwrap_or_else(|| parent.clone()),
            };
            if !parents.contains(&ours) {
                parents.push(ours);
            }
        }
        let mut tree = commit.tree.clone();
        for definition in withheld {
            let mount = definition.mount.as_str();
            if mount == "." {
                return Err(NewgitError::Unsupported(format!(
                    "`{target}` withholds `{}`, which is mounted at `.`, so its files cannot be \
                     put back into the commits that come from there",
                    definition.name
                )));
            }
            if objects.subtree(&commit.tree, mount)?.is_some()
                || objects.entry_at(&commit.tree, mount)?.is_some()
            {
                return Err(NewgitError::SplitRefused(format!(
                    "commit {} on `{target}` changes `{mount}`, which belongs to `{}`, a tracker \
                     that target does not ship, so nothing was pulled",
                    short(id),
                    definition.name
                )));
            }
            let mut ahead = parents[0].clone();
            let ahead_tree = objects.commit(&ahead)?.tree;
            let mut there = objects.subtree(&ahead_tree, mount)?;
            for other in &parents[1..] {
                let other_tree = objects.commit(other)?.tree;
                let other_there = objects.subtree(&other_tree, mount)?;
                if other_there == there {
                    continue;
                }
                objects.flush()?;
                if is_ancestor(repo, &ahead, other)? {
                    ahead = other.clone();
                    there = other_there;
                } else if !is_ancestor(repo, other, &ahead)? {
                    return Err(NewgitError::SplitRefused(format!(
                        "commit {} on `{target}` merges two lines of `{}`, which that target \
                         does not ship, so nothing was pulled: `newgit merge` the work there",
                        short(id),
                        definition.name
                    )));
                }
            }
            let there = there.map(|tree| (TREE_MODE.to_vec(), tree));
            tree = objects.set_path(&tree, mount, there)?;
        }
        let made = if tree == commit.tree && parents == commit.parents {
            id.clone()
        } else {
            let made = objects.make_commit(
                &tree,
                &parents,
                &commit.author,
                &commit.committer,
                &Commit::unsigned_headers(&commit.headers),
                &commit.message,
            );
            rebuilt.push((made.clone(), format!("canonical {id}\n")));
            made
        };
        mapped.insert(id.clone(), made);
    }
    objects.flush()?;
    write_notes(repo, CANONICAL_NOTES_REF, "canonical", &rebuilt)?;
    Ok(mapped)
}

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
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unbound { path, tracker } => write!(
                f,
                "`{path}` belongs to `{tracker}`, which this instance has not bound: \
                 `newgit tracker use {tracker}`",
                tracker = tracker_path(tracker)
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
    /// Called before a question about a tracker's ancestry: its history
    /// must be here for the answer to mean anything.
    pub need_history: &'a dyn Fn(&str) -> Result<()>,
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
    /// View commit → the blob of its canonical note; read once.
    canonical: Option<HashMap<String, String>>,
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
    let mut splitter = Splitter::new(Objects::open(repo)?, options);
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

impl<'a> Splitter<'a> {
    fn new(objects: Objects, options: &'a SplitOptions<'a>) -> Self {
        Splitter {
            objects,
            options,
            mapped: HashMap::new(),
            notes: HashMap::new(),
            notes_loaded: false,
            root_history: None,
            by_trailer: None,
            view_trees: HashMap::new(),
            canonical: None,
            made: Vec::new(),
            noted: Vec::new(),
            sign: HashMap::new(),
        }
    }

    /// The commit a view commit's trailers should name, and its headers:
    /// itself, or for a view commit rebuilt from another commit ([`bring_in`]
    /// notes it) that original, so composing the same trackers gives the
    /// original back, signature included.
    fn origin(&mut self, commit: &Commit) -> Result<(String, Vec<u8>)> {
        if self.canonical.is_none() {
            let repo = self.objects.repo().to_path_buf();
            let listing =
                git(&repo, &["notes", "--ref", CANONICAL_NOTES_REF, "list"]).unwrap_or_default();
            self.canonical = Some(
                listing
                    .lines()
                    .filter_map(|line| line.split_once(' '))
                    .map(|(blob, view)| (view.to_owned(), blob.to_owned()))
                    .collect(),
            );
        }
        let note = self
            .canonical
            .as_ref()
            .and_then(|notes| notes.get(&commit.id));
        if let Some(blob) = note.cloned() {
            let text = read_blob_text(&mut self.objects, &blob)?;
            if let Some(original) = text.trim().strip_prefix("canonical ")
                && let Some(found) = self.objects.try_commit(original)?
            {
                return Ok((found.id, found.headers));
            }
        }
        Ok((commit.id.clone(), commit.headers.clone()))
    }

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
        let stock_merge = commit.parents.len() > 1 && stock_merge_message(&commit.message);
        if !stock_merge && self.options.newgit_made != Some(view) {
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
        let (source, headers) = self.origin(&commit)?;
        let message = if on_root {
            with_split_trailers(&commit.message, &source, &headers)
        } else {
            let names: Vec<&str> = made_here
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            with_split_trailers(
                format!("Update {}\n", names.join(", ")).as_bytes(),
                &source,
                &headers,
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
                    .any(|definition| definition.name == leaf_name(&owner))
            {
                let mut root = TrackerDefinition::root();
                root.name = owner.clone();
                out.push(root);
            }
            for mut definition in definitions {
                let itself = definition.name == leaf_name(&owner);
                definition.name = if itself {
                    owner.clone()
                } else {
                    child_id(&owner, &definition.name)
                };
                if at.as_str() != "." {
                    if itself {
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
                if definition.git && definition.mounted() && !itself {
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
        let own_name = leaf_name(tracker);
        let mut children: Vec<TrackerDefinition> = definitions
            .iter()
            .filter(|definition| definition.git && definition.name != own_name)
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
                other.name != child.name && other.name != own_name && other.mount == child.mount
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
                    other.name != child.name && other.name != own_name && other.mount == child.mount
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
            let id = child_id(tracker, &child.name);
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
                        tracker_path(&id)
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
                    if other != pin && !self.descends_from(&id, other, pin)? {
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
            if !(self.options.bound)(&id) {
                let changed = self
                    .first_change(&before, &part_tree, mount)?
                    .unwrap_or_else(|| mount.to_owned());
                return Err(SplitError::Refused(Refusal::Unbound {
                    path: changed,
                    tracker: id,
                }));
            }
            let piece = self.split_child(commit, &id, &part_tree, &before, made)?;
            if piece.new {
                made.push((id, piece.commit.clone()));
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

    /// Whether `descendant` of `tracker` has `ancestor` behind it. A "yes"
    /// holds on a snapshot; a "no" means nothing until the history is here.
    fn descends_from(&mut self, tracker: &str, ancestor: &str, descendant: &str) -> Result<bool> {
        if self.is_ancestor(ancestor, descendant)? {
            return Ok(true);
        }
        (self.options.need_history)(tracker)?;
        self.is_ancestor(ancestor, descendant)
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

/// Whether a merge's message is one nobody wrote: Git's own (`Merge branch
/// 'x'`, `Merge remote-tracking branch 'origin/x'`, with an optional ` into
/// y`) or newgit's (`Merge `x` into y`), one line. A merge exempt from the
/// one-audience rule says nothing its parents do not; one with words of its
/// own is refused like any commit.
pub fn stock_merge_message(message: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(message) else {
        return false;
    };
    let text = text.trim_end_matches('\n');
    if text.contains('\n') {
        return false;
    }
    // The quoted name, and the `into` target, must be names Git would take
    // for a branch: no room in them for a sentence.
    let quoted = |rest: &str, open: char, close: char| -> bool {
        let Some(rest) = rest.strip_prefix(open) else {
            return false;
        };
        let Some(end) = rest.find(close) else {
            return false;
        };
        let after = &rest[end + close.len_utf8()..];
        ref_name(&rest[..end])
            && (after.is_empty() || after.strip_prefix(" into ").is_some_and(ref_name))
    };
    if let Some(rest) = text.strip_prefix("Merge branch ") {
        return quoted(rest, '\'', '\'');
    }
    if let Some(rest) = text.strip_prefix("Merge remote-tracking branch ") {
        return quoted(rest, '\'', '\'');
    }
    if let Some(rest) = text.strip_prefix("Merge ") {
        return quoted(rest, '`', '`') && rest.contains(" into ");
    }
    false
}

/// Whether `name` is one `git check-ref-format --branch` accepts: no
/// whitespace or control characters, none of `~ ^ : ? * [ \`, no `..`,
/// `@{`, or `//`, no component starting with `.` or ending in `.lock`, and
/// not starting with `-` or ending with `/` or `.`.
fn ref_name(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && !name.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && name
            .split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
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
    find_outside_git_under(repo, rev_list, env, all, None)
}

/// [`find_outside_git`] over a tracker's own history, whose files sit at
/// `mount` in the view: each path is judged, and reported, where the view
/// has it. What an import, a rooted export, and a pull from one check.
pub fn find_outside_git_under(
    repo: &Utf8Path,
    rev_list: &[&str],
    env: &[(String, String)],
    all: &[TrackerDefinition],
    mount: Option<&Utf8Path>,
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
        let field = match mount {
            Some(mount) if mount.as_str() != "." => format!("{mount}/{field}"),
            _ => field.to_owned(),
        };
        if let Some(owner) = owner_of(all, Utf8Path::new(&field)).filter(|owner| !owner.git) {
            return Ok(Some((commit, field, owner.name.clone())));
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
    fn only_a_merge_message_nobody_wrote_is_stock() {
        for stock in [
            &b"Merge branch 'main'"[..],
            b"Merge branch 'feature' into main\n",
            b"Merge remote-tracking branch 'origin/main'\n",
            b"Merge `feature-a` into main",
        ] {
            assert!(
                stock_merge_message(stock),
                "{}",
                String::from_utf8_lossy(stock)
            );
        }
        for written in [
            &b"merge with private details"[..],
            b"Merge branch 'main'\n\nwhy: the refund bypass\n",
            b"Merge branch 'main' into main, fixing the bypass",
            b"Merge pull request #3 from x/y",
            b"Merge `x` because",
        ] {
            assert!(
                !stock_merge_message(written),
                "{}",
                String::from_utf8_lossy(written)
            );
        }
    }

    #[test]
    fn a_message_without_our_trailer_is_not_a_split() {
        assert!(parse_split(b"subject\n\nCo-authored-by: Bo\n").is_none());
        assert!(parse_split(b"subject\n\nNewgit-Split: not-an-id\n").is_none());
    }
}
