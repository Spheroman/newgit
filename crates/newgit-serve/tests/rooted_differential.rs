// A stored rooted history (#412) is extended at each landing by composing
// only the commits that are new (`rooted::refresh`), so it must never come
// out different from composing the whole history from scratch. Generated
// landings from a fixed seed, over trackers in two stores (`root`, `a`,
// `a__c`, `b` with the members, `p` public, `h` in a store nobody here
// reads): edits (some to names that are not UTF-8), merge commits the
// server writes, re-made commits with the merger's as second parent, commits
// split from a view (whose canonical ids become notes), a tracker added and
// removed, a mount renamed, a hidden tracker's pin moving alone (dropped from
// the view), a second branch made, moved, and deleted, tags light and
// annotated, and the readable set changing. After each landing the views
// rooted at `root` and at `a` are refreshed, recorded every three commits,
// and compared with `compose::compose` from scratch in a repository of its
// own: every branch's and tag's tip, what every commit of the tracker's
// history composed to, and the canonical notes.

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions, Composition};
use newgit_core::endpoint::TagAnnotation;
use newgit_core::objects::{EMPTY_TREE, GITLINK_MODE, Objects};
use newgit_serve::repos::Store;
use newgit_serve::rooted::{self, Part, Want};

mod support;

const SEED: u64 = 0x0412_5eed;
const STEPS: usize = 50;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const TRACKERS: [&str; 6] = ["root", "a", "a__c", "b", "p", "h"];

fn parent_of(tracker: &str) -> Option<&'static str> {
    match tracker {
        "root" => None,
        "a__c" => Some("a"),
        _ => Some("root"),
    }
}

fn leaf(tracker: &str) -> &str {
    tracker.rsplit("__").next().unwrap_or(tracker)
}

fn chain(tracker: &str) -> Vec<&'static str> {
    let mut up = Vec::new();
    let mut at = parent_of(tracker);
    while let Some(parent) = at {
        up.push(parent);
        at = parent_of(parent);
    }
    up
}

/// One branch: each tracker's head and own files.
#[derive(Clone, Debug, Default)]
struct Branch {
    heads: BTreeMap<String, String>,
    files: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
}

struct World {
    devs: Utf8PathBuf,
    public: Utf8PathBuf,
    hidden: Utf8PathBuf,
    rng: Rng,
    clock: u64,
    branches: BTreeMap<String, Branch>,
    tags: BTreeMap<String, (String, String, Option<TagAnnotation>)>,
    /// Where `a` is mounted in root.
    a_mount: String,
    b_present: bool,
    /// Whether `p` is in the readable set.
    p_readable: bool,
}

impl World {
    fn dir_of(&self, tracker: &str) -> &Utf8Path {
        match tracker {
            "p" => &self.public,
            "h" => &self.hidden,
            _ => &self.devs,
        }
    }

    fn mount_of(&self, tracker: &str) -> String {
        match tracker {
            "a" => self.a_mount.clone(),
            "a__c" => "c".to_owned(),
            "p" => "pub".to_owned(),
            other => leaf(other).to_owned(),
        }
    }

    fn children(&self, tracker: &str) -> Vec<&'static str> {
        TRACKERS
            .iter()
            .copied()
            .filter(|child| parent_of(child) == Some(tracker))
            .filter(|child| *child != "b" || self.b_present)
            .collect()
    }

    fn stamp(&mut self) -> Vec<u8> {
        self.clock += 1;
        format!("Ana <ana@example.com> {} +0000", 1_700_000_000 + self.clock).into_bytes()
    }

    /// A commit of `tracker` on `branch` with `parents`, its own files as
    /// the branch has them, and each child's definition and pin.
    fn commit(
        &mut self,
        branch: &str,
        tracker: &str,
        parents: &[String],
        message: &[u8],
    ) -> String {
        let dir = self.dir_of(tracker).to_owned();
        let mut objects = Objects::open(&dir).expect("objects");
        let mut tree = EMPTY_TREE.to_owned();
        let own = self.branches[branch]
            .files
            .get(tracker)
            .cloned()
            .unwrap_or_default();
        for (path, body) in &own {
            let blob = objects.make_blob(body);
            tree = objects
                .set_path(&tree, path, Some((b"100644".to_vec(), blob)))
                .expect("set");
        }
        for child in self.children(tracker) {
            let definition = format!("git = true\nmount = \"{}\"\n", self.mount_of(child));
            let blob = objects.make_blob(definition.as_bytes());
            tree = objects
                .set_path(
                    &tree,
                    format!(".newgit/trackers/{}.toml", leaf(child)),
                    Some((b"100644".to_vec(), blob)),
                )
                .expect("set");
            if let Some(pin) = self.branches[branch].heads.get(child) {
                tree = objects
                    .set_path(
                        &tree,
                        format!(".newgit/pins/{}", leaf(child)),
                        Some((GITLINK_MODE.to_vec(), pin.clone())),
                    )
                    .expect("set");
            }
        }
        let when = self.stamp();
        let id = objects.make_commit(&tree, parents, &when, &when, b"", message);
        objects.flush().expect("flush");
        id
    }

    /// Change one of `tracker`'s files on `branch` and commit it, then each
    /// tracker above it, pin moved.
    fn edit(&mut self, branch: &str, tracker: &str, message: &[u8]) {
        let names: [&[u8]; 5] = [
            b"README.md",
            b"caf\xe9.txt",
            b"dir/\xff\xfe.bin",
            b"x.txt",
            b"deep/er/f",
        ];
        let name = names[self.rng.below(names.len())].to_vec();
        let body = format!("{}\n", self.rng.next()).into_bytes();
        let files = self
            .branches
            .get_mut(branch)
            .expect("branch")
            .files
            .entry(tracker.to_owned())
            .or_default();
        if files.len() > 2 && self.rng.below(4) == 0 {
            let first = files.keys().next().cloned().expect("a file");
            files.remove(&first);
        } else {
            files.insert(name, body);
        }
        self.bubble(branch, tracker, message);
    }

    /// Commit `tracker` and every tracker above it on `branch`.
    fn bubble(&mut self, branch: &str, tracker: &str, message: &[u8]) {
        for one in std::iter::once(tracker).chain(chain(tracker)) {
            let parents: Vec<String> = self.branches[branch]
                .heads
                .get(one)
                .cloned()
                .into_iter()
                .collect();
            let made = self.commit(branch, one, &parents, message);
            self.branches
                .get_mut(branch)
                .expect("branch")
                .heads
                .insert(one.to_owned(), made);
        }
    }

    /// Root commits on `branch`'s first-parent line.
    fn first_parents(&self, branch: &str) -> Vec<String> {
        let head = &self.branches[branch].heads["root"];
        support::git(&self.devs, &["rev-list", "--first-parent", head])
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }
}

fn init_bare(dir: &Utf8Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    support::git(dir, &["init", "-q", "--bare", dir.as_str()]);
}

fn set_of(world: &World, root: &str) -> BTreeSet<String> {
    TRACKERS
        .iter()
        .filter(|tracker| newgit_serve::names::within(tracker, root))
        .filter(|tracker| **tracker != "h" && (**tracker != "p" || world.p_readable))
        .map(|tracker| (*tracker).to_owned())
        .collect()
}

fn want_of(world: &World, home: &Utf8Path, root: &str) -> Want {
    let set = set_of(world, root);
    let mut heads = BTreeMap::new();
    for (name, branch) in &world.branches {
        heads.insert(
            name.clone(),
            Part {
                source: branch.heads[root].clone(),
                set: set.clone(),
                annotation: None,
            },
        );
    }
    let mut tags = BTreeMap::new();
    for (name, (root_commit, a_commit, annotation)) in &world.tags {
        tags.insert(
            name.clone(),
            Part {
                source: if root == "root" {
                    root_commit
                } else {
                    a_commit
                }
                .clone(),
                set: set.clone(),
                annotation: annotation.clone(),
            },
        );
    }
    let stores = set
        .iter()
        .map(|tracker| {
            (
                tracker.clone(),
                if tracker == "p" {
                    Store::Public
                } else {
                    Store::ProjectDevs
                },
            )
        })
        .collect();
    Want {
        home: home.to_owned(),
        tracker: root.to_owned(),
        store: Store::ProjectDevs,
        default_branch: "main".to_owned(),
        heads,
        tags,
        stores,
    }
}

fn rev(dir: &Utf8Path, name: &str) -> Option<String> {
    let output = newgit_core::git::command()
        .args([
            "--git-dir",
            dir.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            name,
        ])
        .output()
        .expect("git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Canonical notes under `notes_ref` in `dir`, for the view commits `only`,
/// read from its tree whatever its fan-out.
fn notes(dir: &Utf8Path, notes_ref: &str, only: &BTreeSet<String>) -> BTreeMap<String, String> {
    let Some(_) = rev(dir, notes_ref) else {
        return BTreeMap::new();
    };
    support::git(
        dir,
        &["--git-dir", dir.as_str(), "ls-tree", "-r", notes_ref],
    )
    .lines()
    .filter_map(|line| {
        let (meta, path) = line.split_once('\t')?;
        let blob = meta.split_whitespace().nth(2)?.to_owned();
        Some((path.replace('/', ""), blob))
    })
    .filter(|(commit, _)| only.contains(commit))
    .map(|(commit, blob)| {
        (
            commit,
            support::git(dir, &["--git-dir", dir.as_str(), "cat-file", "-p", &blob]),
        )
    })
    .collect()
}

/// Compare `root`'s stored rooted history with one composed from scratch
/// in `scratch`. Returns how many commits were compared.
fn compare(
    world: &World,
    home: &Utf8Path,
    scratch: &Utf8Path,
    root: &str,
    step: usize,
) -> (usize, usize) {
    let want = want_of(world, home, root);
    let refreshed = rooted::refresh_with(&want, 3, &|| {}).expect("refresh");
    let set = set_of(world, root);
    let public = world.public.clone();
    let public_objects = std::cell::RefCell::new(Objects::open(&public).expect("objects"));
    let admit = |tracker: &str, commit: &str| {
        tracker != "p"
            || public_objects
                .borrow_mut()
                .has_commit(commit)
                .unwrap_or(false)
    };
    let readable = |name: &str| set.contains(name);
    let options = ComposeOptions {
        readable: &readable,
        admit: Some(&admit),
    };
    let prefix = format!("refs/namespaces/r/refs/namespaces/{root}/");
    let mut tips = Vec::new();
    let mut labels = Vec::new();
    for (name, part) in &want.heads {
        tips.push((
            format!("refs/scratch/{step}/heads/{name}"),
            part.source.clone(),
        ));
        labels.push(format!("{prefix}refs/heads/{name}"));
    }
    for (name, part) in &want.tags {
        tips.push((
            format!("refs/scratch/{step}/tags/{name}"),
            part.source.clone(),
        ));
        labels.push(format!("{prefix}refs/tags/{name}"));
    }
    let views = compose::compose(scratch, root, &tips, &options).expect("compose from scratch");
    for ((label, view), (_, source)) in labels.iter().zip(&views).zip(&tips) {
        let stored = rev(&world.devs, &format!("{label}^{{commit}}"));
        assert_eq!(
            stored, view.tip,
            "step {step}: `{root}`'s {label} (from {source}) is not what composing from scratch gives ({refreshed:?})"
        );
    }
    // Every commit of the tracker's history, by what it composed to.
    let sources: Vec<String> = tips.iter().map(|(_, source)| source.clone()).collect();
    let walk = compose::walk_of(&world.devs, &sources, &[]).expect("walk");
    let mut none = |_: &str| Ok(None);
    let scratch_made = compose::extend(scratch, root, &walk, &mut none, &options).expect("extend");
    for (commit, expected) in &scratch_made {
        let stored: Option<Composition> =
            rooted::stored_composition(&want, &set, commit).expect("read");
        assert_eq!(
            stored.as_ref(),
            Some(expected),
            "step {step}: `{root}`'s commit {commit} composed differently"
        );
    }
    // The canonical notes of every view commit a branch reaches.
    let heads: Vec<String> = views[..want.heads.len()]
        .iter()
        .filter_map(|view| view.tip.clone())
        .collect();
    let reachable: BTreeSet<String> = support::git(
        scratch,
        &[
            &["--git-dir", scratch.as_str(), "rev-list"][..],
            &heads.iter().map(String::as_str).collect::<Vec<_>>(),
        ]
        .concat(),
    )
    .lines()
    .map(ToOwned::to_owned)
    .collect();
    let expected = notes(scratch, compose::CANONICAL_NOTES_REF, &reachable);
    let stored = notes(
        &world.devs,
        &format!("{prefix}refs/notes/commits"),
        &reachable,
    );
    assert_eq!(
        stored, expected,
        "step {step}: `{root}`'s canonical notes differ"
    );
    (scratch_made.len(), expected.len())
}

#[test]
fn stored_rooted_histories_match_a_composition_from_scratch() {
    let temp = support::tempdir("rooted-differential");
    let home = temp.join("homes/jack/p");
    let devs = Store::ProjectDevs.dir(&home);
    let public = Store::Public.dir(&home);
    let hidden = Store::User("x".into()).dir(&home);
    for dir in [&devs, &public, &hidden] {
        init_bare(dir);
    }
    // Composing reads every store; it writes only here, served to no one.
    let scratch = temp.join("scratch.git");
    init_bare(&scratch);
    std::fs::write(
        scratch.join("objects/info/alternates"),
        format!("{devs}/objects\n{public}/objects\n{hidden}/objects\n"),
    )
    .expect("alternates");

    let mut world = World {
        devs,
        public,
        hidden,
        rng: Rng(SEED),
        clock: 0,
        branches: BTreeMap::from([("main".to_owned(), Branch::default())]),
        tags: BTreeMap::new(),
        a_mount: "a".to_owned(),
        b_present: true,
        p_readable: true,
    };
    // Children first, so every pin names a commit.
    for tracker in ["a__c", "b", "p", "h", "a", "root"] {
        world
            .branches
            .get_mut("main")
            .expect("main")
            .files
            .entry(tracker.to_owned())
            .or_default()
            .insert(b"start.txt".to_vec(), format!("{tracker}\n").into_bytes());
        let made = world.commit(
            "main",
            tracker,
            &[],
            format!("start {tracker}\n").as_bytes(),
        );
        world
            .branches
            .get_mut("main")
            .expect("main")
            .heads
            .insert(tracker.to_owned(), made);
    }

    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let mut compared = 0;
    let mut noted = 0;
    for step in 0..STEPS {
        let on_dev = world.branches.contains_key("dev") && world.rng.below(4) == 0;
        let branch = if on_dev { "dev" } else { "main" }.to_owned();
        let kind = match world.rng.below(13) {
            0..=3 => {
                let tracker = ["root", "a", "a__c", "p", "b"][world.rng.below(5)];
                if tracker == "b" && !world.b_present {
                    "skip"
                } else {
                    world.edit(&branch, tracker, format!("edit {step}\n").as_bytes());
                    "edit"
                }
            }
            4 => {
                // A hidden tracker's pin moves alone.
                world.edit(&branch, "h", format!("hidden {step}\n").as_bytes());
                "hidden"
            }
            5 | 6 => {
                // The server's merge: the merger's commit, made on an older
                // root commit, second; or a re-made commit on the head.
                let older = world.first_parents(&branch);
                let base = older[world.rng.below(older.len().min(6))].clone();
                let mut side_branch = world.branches[&branch].clone();
                side_branch
                    .files
                    .entry("root".to_owned())
                    .or_default()
                    .insert(format!("side-{step}").into_bytes(), b"side\n".to_vec());
                world.branches.insert("side".to_owned(), side_branch);
                let side =
                    world.commit("side", "root", &[base], format!("side {step}\n").as_bytes());
                world.branches.remove("side");
                world
                    .branches
                    .get_mut(&branch)
                    .expect("branch")
                    .files
                    .entry("root".to_owned())
                    .or_default()
                    .insert(format!("side-{step}").into_bytes(), b"side\n".to_vec());
                let head = world.branches[&branch].heads["root"].clone();
                let message: &[u8] = if world.rng.below(2) == 0 {
                    b"Merge `ana` into main"
                } else {
                    b"re-made\n"
                };
                let made = world.commit(&branch, "root", &[head, side], message);
                world
                    .branches
                    .get_mut(&branch)
                    .expect("branch")
                    .heads
                    .insert("root".to_owned(), made);
                "merge"
            }
            7 => {
                // Split from a view commit: root and `a` carry its trailers.
                let source = format!(
                    "{:040x}",
                    u128::from(world.rng.next()) << 64 | u128::from(world.rng.next())
                );
                let source = &source[source.len() - 40..];
                let message = format!("work {step}\n\nNewgit-Split: {source}\n");
                world
                    .branches
                    .get_mut(&branch)
                    .expect("branch")
                    .files
                    .entry("root".to_owned())
                    .or_default()
                    .insert(b"split.txt".to_vec(), format!("{step}\n").into_bytes());
                world.edit(&branch, "a", message.as_bytes());
                "split"
            }
            8 => {
                world.b_present = !world.b_present;
                if world.b_present && !world.branches[&branch].heads.contains_key("b") {
                    world.edit(&branch, "b", b"b again\n");
                } else {
                    world.bubble(&branch, "root", format!("b toggled {step}\n").as_bytes());
                }
                "add-remove"
            }
            9 => {
                world.a_mount = if world.a_mount == "a" { "a2" } else { "a" }.to_owned();
                world.bubble(&branch, "root", format!("mount {step}\n").as_bytes());
                "rename-mount"
            }
            10 => {
                if world.branches.contains_key("dev") && world.rng.below(2) == 0 {
                    world.branches.remove("dev");
                    "dev-deleted"
                } else {
                    let main = world.branches["main"].clone();
                    world.branches.insert("dev".to_owned(), main);
                    "dev-made"
                }
            }
            11 => {
                let name = format!("v{}", world.rng.below(3));
                if world.tags.contains_key(&name) && world.rng.below(3) == 0 {
                    world.tags.remove(&name);
                    "tag-deleted"
                } else {
                    let main = &world.branches["main"];
                    let annotation = (world.rng.below(2) == 0).then(|| TagAnnotation {
                        tagger: "Ana <ana@example.com> 1700000000 +0000".to_owned(),
                        message: format!("release {step}\n"),
                    });
                    world.tags.insert(
                        name,
                        (
                            main.heads["root"].clone(),
                            main.heads["a"].clone(),
                            annotation,
                        ),
                    );
                    "tag"
                }
            }
            _ => {
                world.p_readable = !world.p_readable;
                "readable-set"
            }
        };
        *kinds.entry(kind).or_default() += 1;
        if world.branches.contains_key("dev") && kind == "dev-made" {
            world.edit("dev", "a__c", format!("dev {step}\n").as_bytes());
        }
        for root in ["root", "a"] {
            let (commits, notes) = compare(&world, &home, &scratch, root, step);
            compared += commits;
            noted += notes;
        }
    }
    for kind in [
        "edit",
        "hidden",
        "merge",
        "split",
        "add-remove",
        "rename-mount",
        "dev-made",
        "tag",
    ] {
        assert!(
            kinds.get(kind).copied().unwrap_or(0) > 0,
            "the generated landings never did `{kind}`: {kinds:?}"
        );
    }
    assert!(noted > 0, "no view commit needed a canonical note");
    eprintln!("{kinds:?}; {compared} commits and {noted} notes compared");
    let _ = std::fs::remove_dir_all(&temp);
}
