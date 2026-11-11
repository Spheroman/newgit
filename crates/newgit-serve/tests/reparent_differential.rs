// The re-make (`reparent`) stands in for Git's merge of the composed trees
// whenever the overlap test calls two merges apart, so it must never give a
// different result (#436, second review). Cases are generated from a fixed
// seed across several trackers; whenever the two touches do not overlap,
// the re-made index, composed, must be exactly the tree
// `git -c merge.directoryRenames=true merge-tree` makes of the same three
// composed trees. A case where the touches overlap runs Git's merge on the
// server, so it needs no comparing; the test counts both kinds and asks for
// enough of each.

use std::collections::{BTreeMap, HashMap};
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::view_tree;
use newgit_core::objects::Objects;
use newgit_serve::db::List;
use newgit_serve::error::ApiResult;
use newgit_serve::reparent::{Landings, Touch, remake};

mod support;

const NOBODY: &[u8] = b"newgit <newgit@localhost> 0 +0000";
const SEED: u64 = 0x0436_5eed;
const CASES: usize = 300;

// ── The model: each tracker's own files ────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum Content {
    Blob(Vec<u8>),
    Gitlink(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct File {
    mode: &'static str,
    content: Content,
}

type Files = BTreeMap<Vec<u8>, File>;

/// Every tracker present, by id (`root`, `blog`, `docs__api`), with its own
/// files. A child is defined in its parent's `.newgit/trackers/<leaf>.toml`
/// and pinned at build time.
#[derive(Clone, Debug, Default)]
struct State {
    trackers: BTreeMap<String, Files>,
}

fn parent_of(tracker: &str) -> Option<&str> {
    if tracker == "root" {
        return None;
    }
    Some(tracker.rfind("__").map_or("root", |at| &tracker[..at]))
}

fn leaf(tracker: &str) -> &str {
    tracker.rsplit("__").next().unwrap_or(tracker)
}

fn depth(tracker: &str) -> usize {
    if tracker == "root" {
        0
    } else {
        tracker.matches("__").count() + 1
    }
}

fn definition_path(child: &str) -> Vec<u8> {
    format!(".newgit/trackers/{}.toml", leaf(child)).into_bytes()
}

fn definition(mount: &str) -> File {
    blob(format!("git = true\nmount = \"{mount}\"\n").into_bytes())
}

fn blob(content: Vec<u8>) -> File {
    File {
        mode: "100644",
        content: Content::Blob(content),
    }
}

impl State {
    /// `child`'s mount inside its parent, as the parent defines it.
    fn relative_mount(&self, child: &str) -> Option<String> {
        let parent = self.trackers.get(parent_of(child)?)?;
        let file = parent.get(&definition_path(child))?;
        let Content::Blob(text) = &file.content else {
            return None;
        };
        let text = String::from_utf8_lossy(text);
        let line = text.lines().find(|line| line.starts_with("mount = "))?;
        Some(line["mount = ".len()..].trim_matches('"').to_owned())
    }

    fn children<'s>(&'s self, tracker: &'s str) -> impl Iterator<Item = &'s String> + 's {
        self.trackers
            .keys()
            .filter(move |child| parent_of(child) == Some(tracker))
    }

    /// Whether the state is one a project can be in: each child defined,
    /// each file its tracker's to own (nothing under a child's mount or
    /// above it, nothing in `.newgit/` but definitions and the stray pin),
    /// and no file where another file's directory is.
    fn valid(&self) -> bool {
        for (tracker, files) in &self.trackers {
            let mut mounts = Vec::new();
            for child in self.children(tracker) {
                let Some(mount) = self.relative_mount(child) else {
                    return false;
                };
                mounts.push(mount.into_bytes());
            }
            for path in files.keys() {
                if path.starts_with(b".newgit/") {
                    let defines =
                        path.starts_with(b".newgit/trackers/") && path.ends_with(b".toml");
                    if !defines && path != b".newgit/pins/stray" {
                        return false;
                    }
                    continue;
                }
                for mount in &mounts {
                    if inside(path, mount) || inside(mount, path) || path == mount {
                        return false;
                    }
                }
            }
            for path in files.keys() {
                let mut dir = path.clone();
                dir.push(b'/');
                if files
                    .range(dir.clone()..)
                    .next()
                    .is_some_and(|(next, _)| next.starts_with(&dir))
                {
                    return false;
                }
            }
        }
        true
    }
}

/// Whether `path` is under the directory `dir`.
fn inside(path: &[u8], dir: &[u8]) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

// ── A repository holding every tracker's objects ───────────────────────────

struct Repo {
    dir: Utf8PathBuf,
    blobs: HashMap<Vec<u8>, String>,
}

impl Repo {
    /// A bare repository made as the server makes its scratch one, so its
    /// `core.ignorecase` and the rest are what the server merge sees.
    fn new(name: &str) -> Self {
        let dir = support::tempdir(name).join("all.git");
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(dir.as_str())
            .status()
            .expect("git init");
        assert!(status.success());
        Self {
            dir,
            blobs: HashMap::new(),
        }
    }

    fn write_blobs(&mut self, state: &State) {
        let mut missing: Vec<Vec<u8>> = Vec::new();
        for files in state.trackers.values() {
            for file in files.values() {
                if let Content::Blob(bytes) = &file.content
                    && !self.blobs.contains_key(bytes)
                    && !missing.contains(bytes)
                {
                    missing.push(bytes.clone());
                }
            }
        }
        if missing.is_empty() {
            return;
        }
        let scratch = tempfile_dir(&self.dir);
        let mut paths = String::new();
        for (index, bytes) in missing.iter().enumerate() {
            let path = scratch.join(format!("blob-{index}"));
            std::fs::write(&path, bytes).expect("write blob");
            paths.push_str(path.as_str());
            paths.push('\n');
        }
        let out = git_in(
            &self.dir,
            &["hash-object", "-w", "--no-filters", "--stdin-paths"],
            Some(paths.as_bytes()),
        );
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(ids.len(), missing.len());
        for (bytes, id) in missing.into_iter().zip(ids) {
            self.blobs.insert(bytes, id);
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Each tracker's commit for `state`: its own files and a pin for each
    /// child, deepest first, so equal content is the same commit.
    fn build(&mut self, state: &State) -> List {
        self.write_blobs(state);
        let mut objects = Objects::open(&self.dir).expect("objects");
        let mut order: Vec<&String> = state.trackers.keys().collect();
        order.sort_by_key(|tracker| std::cmp::Reverse(depth(tracker)));
        let mut list = List::new();
        for tracker in order {
            let mut tree = objects.make_tree(Vec::new());
            for (path, file) in &state.trackers[tracker] {
                let id = match &file.content {
                    Content::Blob(bytes) => self.blobs[bytes].clone(),
                    Content::Gitlink(id) => id.clone(),
                };
                tree = objects
                    .set_path(&tree, path, Some((file.mode.as_bytes().to_vec(), id)))
                    .expect("set");
            }
            for child in state.children(tracker) {
                let pin = format!(".newgit/pins/{}", leaf(child));
                tree = objects
                    .set_path(&tree, &pin, Some((b"160000".to_vec(), list[child].clone())))
                    .expect("pin");
            }
            let commit = objects.make_commit(
                &tree,
                &[],
                NOBODY,
                NOBODY,
                b"",
                format!("{tracker}\n").as_bytes(),
            );
            list.insert(tracker.clone(), commit);
        }
        objects.flush().expect("flush");
        list
    }
}

fn tempfile_dir(repo: &Utf8Path) -> Utf8PathBuf {
    let dir = repo.with_file_name("blobs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn git_in(dir: &Utf8Path, args: &[&str], input: Option<&[u8]>) -> std::process::Output {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("git output")
}

/// Commits the re-make writes land in the same repository here.
struct Here<'a>(&'a Utf8Path);

impl Landings for Here<'_> {
    fn open(&mut self, _: &str) -> ApiResult<Objects> {
        Ok(Objects::open(self.0)?)
    }

    fn wait(&mut self, _: &Objects, _: &str, _: &str) -> ApiResult<()> {
        Ok(())
    }

    fn made(&self, _: &str, _: &str) -> bool {
        false
    }
}

// ── One case: the re-make against Git's merge ──────────────────────────────

#[derive(Debug)]
enum Verdict {
    /// The touches overlap: Git's merge runs.
    Overlap,
    /// Re-made, and equal to Git's merge: the composed tree.
    Apart(String),
}

/// What `git merge-tree` makes of three view commits: its tree, or `None`
/// for a conflict.
fn git_merge(dir: &Utf8Path, base: &str, head: &str, list: &str) -> Option<String> {
    let out = git_in(
        dir,
        &[
            "-c",
            "merge.directoryRenames=true",
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            &format!("--merge-base={base}"),
            head,
            list,
        ],
        None,
    );
    let tree = String::from_utf8_lossy(out.stdout.split(|&b| b == 0).next().unwrap_or_default())
        .trim()
        .to_owned();
    match out.status.code() {
        Some(0) => Some(tree),
        Some(1) => None,
        _ => panic!("merge-tree: {}", String::from_utf8_lossy(&out.stderr)),
    }
}

fn view(objects: &mut Objects, root: &str) -> String {
    let tree = view_tree(objects, root).expect("view");
    objects.make_commit(&tree, &[], NOBODY, NOBODY, b"", b"view\n")
}

/// `base` moved to `head` by one landing (landed first) and to `list` by
/// the merge: what the server does on the merge's first attempt.
fn check(repo: &mut Repo, base: &State, head: &State, list: &State) -> Result<Verdict, String> {
    let (b, h, l) = (repo.build(base), repo.build(head), repo.build(list));
    let dir = repo.dir.clone();
    let mut open = |_: &str| -> ApiResult<Objects> { Ok(Objects::open(&dir)?) };
    let mine = Touch::read(&mut open, &b, &l, None).map_err(|e| format!("touch: {e:?}"))?;
    let theirs =
        Touch::read(&mut open, &b, &h, Some(&mine)).map_err(|e| format!("touch: {e:?}"))?;
    if mine.overlap(&theirs).is_some() {
        return Ok(Verdict::Overlap);
    }
    let target = remake(
        &mut Here(&dir),
        "ana",
        "main",
        0,
        [&b, &l, &h],
        [&mine, &theirs],
    )
    .map_err(|e| format!("remake: {e:?}"))?;
    let mut objects = Objects::open(&dir).expect("objects");
    let (bv, hv, lv) = (
        view(&mut objects, &b["root"]),
        view(&mut objects, &h["root"]),
        view(&mut objects, &l["root"]),
    );
    let made = view_tree(&mut objects, &target["root"]).expect("view");
    objects.flush().expect("flush");
    match git_merge(&dir, &bv, &hv, &lv) {
        None => Err(format!(
            "the touches were apart but Git's merge conflicts\nmine {mine:?}\ntheirs {theirs:?}"
        )),
        Some(tree) if tree != made => {
            let diff = git_in(
                &dir,
                &["diff-tree", "-r", "--name-status", &tree, &made],
                None,
            );
            Err(format!(
                "re-made tree differs from Git's merge (Git → re-made):\n{}\nmine {mine:?}\ntheirs {theirs:?}",
                String::from_utf8_lossy(&diff.stdout)
            ))
        }
        Some(tree) => Ok(Verdict::Apart(tree)),
    }
}

// ── Generating cases ───────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

/// Names, some not ASCII, one not UTF-8, two differing only in case.
const NAMES: &[&[u8]] = &[
    b"a.md",
    b"b.md",
    b"notes.txt",
    b"README.md",
    b"readme.md",
    b"x.rs",
    b"run.sh",
    "caf\u{e9}.md".as_bytes(),
    "cafe\u{301}.md".as_bytes(),
    b"caf\xe9.md",
    b"\xff\xfe.bin",
];

fn dirs(tracker: &str) -> &'static [&'static str] {
    match tracker {
        "root" => &["", "src", "src/util", "lib", "site", "posts"],
        "blog" => &["", "2024", "2024/jan", "drafts"],
        "docs" => &["", "guide", "ref"],
        "docs__api" => &["", "v1"],
        _ => &["", "daily"],
    }
}

fn join(dir: &str, name: &[u8]) -> Vec<u8> {
    let mut path = Vec::new();
    if !dir.is_empty() {
        path.extend_from_slice(dir.as_bytes());
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

fn base_name(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

/// Eight lines, so an edit of one keeps a rename above Git's 50%.
fn text(rng: &mut Rng) -> Vec<u8> {
    let tag = rng.next();
    (0..8)
        .map(|line| format!("line {line} of {tag:x}\n"))
        .collect::<String>()
        .into_bytes()
}

fn edited(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    if lines.is_empty() {
        return text(rng);
    }
    let at = rng.below(lines.len());
    lines[at] = format!("edited {:x}", rng.next());
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
        .into_bytes()
}

fn random_path(rng: &mut Rng, tracker: &str) -> Vec<u8> {
    join(rng.pick(dirs(tracker)), rng.pick(NAMES))
}

/// Own content (not a definition or the stray pin) of `tracker`.
fn content_paths(files: &Files) -> Vec<Vec<u8>> {
    files
        .keys()
        .filter(|path| !path.starts_with(b".newgit/"))
        .cloned()
        .collect()
}

fn base_state(rng: &mut Rng) -> State {
    let mut state = State::default();
    let mut root = Files::new();
    root.insert(definition_path("blog"), definition("blog"));
    root.insert(definition_path("docs"), definition("site/docs"));
    let mut trackers = vec!["root", "blog", "docs", "docs__api"];
    if rng.chance(3) {
        root.insert(definition_path("notes"), definition("notes"));
        trackers.push("notes");
    }
    if rng.chance(4) {
        root.insert(b".gitattributes".to_vec(), blob(b"*.md text\n".to_vec()));
    }
    state.trackers.insert("root".into(), root);
    let mut docs = Files::new();
    docs.insert(definition_path("docs__api"), definition("api"));
    state.trackers.insert("docs".into(), docs);
    for tracker in trackers {
        let files = state.trackers.entry(tracker.to_owned()).or_default();
        for _ in 0..2 + rng.below(4) {
            let path = random_path(rng, tracker);
            files.insert(path, blob(text(rng)));
        }
    }
    // A name chosen twice keeps one file; a path that is another's
    // directory cannot happen here, but drop such a state anyway.
    if state.valid() {
        state
    } else {
        base_state(rng)
    }
}

/// One random change to `state`; `false` when it did not apply.
fn change(rng: &mut Rng, state: &mut State) -> bool {
    let trackers: Vec<String> = state.trackers.keys().cloned().collect();
    let tracker = rng.pick(&trackers).clone();
    let own = content_paths(&state.trackers[&tracker]);
    let files = state.trackers.get_mut(&tracker).expect("present");
    let any = |rng: &mut Rng| (!own.is_empty()).then(|| rng.pick(&own).clone());
    match rng.below(20) {
        // An edit.
        0..=2 => {
            let Some(path) = any(rng) else { return false };
            let file = files.get_mut(&path).expect("file");
            file.content = match &file.content {
                Content::Blob(bytes) => Content::Blob(edited(rng, bytes)),
                Content::Gitlink(_) => Content::Gitlink(format!("{:040x}", rng.next() as u128)),
            };
        }
        // An add, or a copy of a file already there.
        3 | 4 => {
            let path = random_path(rng, &tracker);
            let content = match any(rng) {
                Some(from) if rng.chance(3) => files[&from].content.clone(),
                _ => Content::Blob(text(rng)),
            };
            files.insert(
                path,
                File {
                    mode: "100644",
                    content,
                },
            );
        }
        // A delete.
        5 => {
            let Some(path) = any(rng) else { return false };
            files.remove(&path);
        }
        // A rename, with or without an edit.
        6 | 7 => {
            let Some(path) = any(rng) else { return false };
            let mut file = files.remove(&path).expect("file");
            if rng.chance(2)
                && let Content::Blob(bytes) = &file.content
            {
                file.content = Content::Blob(edited(rng, bytes));
            }
            let to = if rng.chance(2) {
                random_path(rng, &tracker)
            } else {
                join(rng.pick(dirs(&tracker)), base_name(&path))
            };
            files.insert(to, file);
        }
        // A directory renamed: every file under it moved.
        8 | 9 => {
            let dirs_here: Vec<Vec<u8>> = own
                .iter()
                .filter_map(|path| {
                    path.iter()
                        .rposition(|&b| b == b'/')
                        .map(|at| path[..at].to_vec())
                })
                .collect();
            if dirs_here.is_empty() {
                return false;
            }
            let from = rng.pick(&dirs_here).clone();
            let to = match rng.below(3) {
                0 => b"renamed".to_vec(),
                1 => {
                    let mut to = from.clone();
                    to.extend_from_slice(b"-moved");
                    to
                }
                _ => rng.pick(dirs(&tracker)).as_bytes().to_vec(),
            };
            if to == from {
                return false;
            }
            let moving: Vec<Vec<u8>> = own
                .iter()
                .filter(|path| inside(path, &from))
                .cloned()
                .collect();
            for path in moving {
                let file = files.remove(&path).expect("file");
                let mut at = to.clone();
                if !at.is_empty() {
                    at.push(b'/');
                }
                at.extend_from_slice(&path[from.len() + 1..]);
                files.insert(at, file);
            }
        }
        // A mode change.
        10 => {
            let Some(path) = any(rng) else { return false };
            let file = files.get_mut(&path).expect("file");
            file.mode = match file.mode {
                "100644" => "100755",
                "100755" => "100644",
                _ => return false,
            };
        }
        // A file becomes a directory, or a directory a file.
        11 => {
            let Some(path) = any(rng) else { return false };
            if rng.chance(2) {
                let file = files.remove(&path).expect("file");
                let mut inner = path.clone();
                inner.extend_from_slice(b"/inner.md");
                files.insert(inner, file);
            } else {
                let Some(at) = path.iter().rposition(|&b| b == b'/') else {
                    return false;
                };
                let dir = path[..at].to_vec();
                files.retain(|other, _| !inside(other, &dir));
                files.insert(dir, blob(text(rng)));
            }
        }
        // A symlink, or a gitlink that is not a pin.
        12 => {
            let path = random_path(rng, &tracker);
            let file = if rng.chance(2) {
                File {
                    mode: "120000",
                    content: Content::Blob(format!("../target-{}", rng.below(4)).into_bytes()),
                }
            } else {
                File {
                    mode: "160000",
                    content: Content::Gitlink(format!("{:040x}", u128::from(rng.next()))),
                }
            };
            files.insert(path, file);
        }
        // A name differing only in case from one already there.
        13 => {
            let Some(path) = any(rng) else { return false };
            let mut twin = path.clone();
            let at = path.iter().rposition(|&b| b == b'/').map_or(0, |at| at + 1);
            twin[at] = if twin[at].is_ascii_lowercase() {
                twin[at].to_ascii_uppercase()
            } else {
                twin[at].to_ascii_lowercase()
            };
            if twin == path {
                return false;
            }
            files.insert(twin, blob(text(rng)));
        }
        // A file moved to another tracker, maybe edited.
        14 | 15 => {
            let Some(path) = any(rng) else { return false };
            let others: Vec<String> = trackers
                .iter()
                .filter(|other| **other != tracker)
                .cloned()
                .collect();
            let to_tracker = rng.pick(&others).clone();
            let mut file = files.remove(&path).expect("file");
            if rng.chance(3)
                && let Content::Blob(bytes) = &file.content
            {
                file.content = Content::Blob(edited(rng, bytes));
            }
            let to = join(rng.pick(dirs(&to_tracker)), base_name(&path));
            state
                .trackers
                .get_mut(&to_tracker)
                .expect("present")
                .insert(to, file);
        }
        // A whole tracker emptied: its files moved into root's `posts/`, or
        // deleted.
        16 => {
            let emptied = *rng.pick(&["blog", "docs__api", "notes"]);
            let Some(files) = state.trackers.get_mut(emptied) else {
                return false;
            };
            let moved: Files = std::mem::take(files);
            if moved.is_empty() {
                return false;
            }
            if rng.chance(2) {
                let root = state.trackers.get_mut("root").expect("root");
                for (path, file) in moved {
                    root.insert(join("posts", &path), file);
                }
            }
        }
        // A mount renamed: `blog` between `blog/` and `journal/`.
        17 => {
            let root = state.trackers.get_mut("root").expect("root");
            let Some(file) = root.get(&definition_path("blog")) else {
                return false;
            };
            let to = if file == &definition("blog") {
                "journal"
            } else {
                "blog"
            };
            root.insert(definition_path("blog"), definition(to));
        }
        // A tracker added or dropped: `notes`.
        18 => {
            if state.trackers.remove("notes").is_some() {
                state
                    .trackers
                    .get_mut("root")
                    .expect("root")
                    .remove(&definition_path("notes"));
            } else {
                let mut notes = Files::new();
                notes.insert(random_path(rng, "notes"), blob(text(rng)));
                state.trackers.insert("notes".into(), notes);
                state
                    .trackers
                    .get_mut("root")
                    .expect("root")
                    .insert(definition_path("notes"), definition("notes"));
            }
        }
        // Root's `.gitattributes`, or a stray file under `.newgit/pins`.
        _ => {
            let root = state.trackers.get_mut("root").expect("root");
            if rng.chance(2) {
                let rule = *rng.pick(&[
                    "*.md text\n",
                    "*.txt binary\n",
                    "* merge=union\n",
                    "*.md -diff\n",
                ]);
                root.insert(b".gitattributes".to_vec(), blob(rule.as_bytes().to_vec()));
            } else {
                root.insert(b".newgit/pins/stray".to_vec(), blob(text(rng)));
            }
        }
    }
    true
}

/// `base` with one to three random changes that leave a valid state.
fn side(rng: &mut Rng, base: &State) -> State {
    loop {
        let mut state = base.clone();
        let wanted = 1 + rng.below(3);
        let mut made = 0;
        for _ in 0..wanted * 8 {
            let mut next = state.clone();
            if change(rng, &mut next) && next.valid() {
                state = next;
                made += 1;
                if made == wanted {
                    break;
                }
            }
        }
        if made > 0 {
            return state;
        }
    }
}

// ── The tests ──────────────────────────────────────────────────────────────

/// Case `case`, generated from its own seed (so a failing case reruns alone,
/// whatever thread ran it): `Ok(true)` when re-made, `Ok(false)` when the
/// touches overlapped.
fn run_case(repo: &mut Repo, case: usize) -> Result<bool, String> {
    let mut rng = Rng(SEED ^ (case as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let base = base_state(&mut rng);
    let head = side(&mut rng, &base);
    let list = side(&mut rng, &base);
    match check(repo, &base, &head, &list) {
        Ok(Verdict::Apart(_)) => Ok(true),
        Ok(Verdict::Overlap) => Ok(false),
        Err(why) => Err(format!(
            "case {case}: {why}\nbase {base:?}\nhead {head:?}\nlist {list:?}"
        )),
    }
}

#[test]
fn the_remake_is_gits_merge_whenever_the_touches_are_apart() {
    // Spread over threads, each with a repository of its own: a case is
    // dozens of short `git` runs.
    let threads = std::thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(2, 8);
    let results: Vec<Result<bool, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                scope.spawn(move || {
                    let mut repo = Repo::new(&format!("reparent-differential-{thread}"));
                    (thread..CASES)
                        .step_by(threads)
                        .map(|case| run_case(&mut repo, case))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a thread of cases"))
            .collect()
    });
    let apart = results
        .iter()
        .filter(|result| matches!(result, Ok(true)))
        .count();
    let overlap = results
        .iter()
        .filter(|result| matches!(result, Ok(false)))
        .count();
    let diverged: Vec<&String> = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .collect();
    eprintln!("{CASES} cases: {apart} re-made and equal to Git's merge, {overlap} overlapping");
    assert!(
        diverged.is_empty(),
        "{} diverged:\n\n{}",
        diverged.len(),
        diverged
            .iter()
            .map(|why| why.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    assert!(
        apart >= CASES / 4,
        "only {apart} of {CASES} cases were apart"
    );
    assert!(
        overlap >= CASES / 10,
        "only {overlap} of {CASES} cases overlapped"
    );
}

fn with_root(files: &[(&[u8], &str)]) -> Files {
    files
        .iter()
        .map(|(path, text)| (path.to_vec(), blob(text.as_bytes().to_vec())))
        .collect()
}

/// A name that is not UTF-8 is set by its own bytes: the re-make keeps it.
#[test]
fn a_name_that_is_not_utf8_survives_the_remake() {
    let mut repo = Repo::new("reparent-non-utf8");
    let mut base = State::default();
    base.trackers
        .insert("root".into(), with_root(&[(b"a.md", "a\n")]));
    let mut head = base.clone();
    head.trackers
        .get_mut("root")
        .expect("root")
        .insert(b"jack.md".to_vec(), blob(b"jack\n".to_vec()));
    let mut list = base.clone();
    list.trackers
        .get_mut("root")
        .expect("root")
        .insert(b"caf\xe9.md".to_vec(), blob(b"ana's bytes\n".to_vec()));
    let verdict = check(&mut repo, &base, &head, &list).expect("agrees with Git");
    let Verdict::Apart(tree) = verdict else {
        panic!("disjoint changes are re-made: {verdict:?}");
    };
    let mut objects = Objects::open(&repo.dir).expect("objects");
    assert!(
        objects
            .entry_at(&tree, b"caf\xe9.md")
            .expect("read")
            .is_some()
    );
    assert!(objects.entry_at(&tree, "jack.md").expect("read").is_some());
}

/// jack moves every file of `blog` (mounted at `blog/`) into root's
/// `posts/`; ana adds `blog/ana.md`. Git follows the directory rename to
/// `posts/ana.md`, so the touches must overlap and Git's merge run.
#[test]
fn a_tracker_emptied_by_a_move_is_followed_by_gits_merge() {
    let mut repo = Repo::new("reparent-emptied");
    let mut base = State::default();
    let mut root = with_root(&[(b"a.md", "a\n")]);
    root.insert(definition_path("blog"), definition("blog"));
    base.trackers.insert("root".into(), root);
    let post = "the post\nwith lines\nenough to be\nfound again\n";
    base.trackers
        .insert("blog".into(), with_root(&[(b"post.md", post)]));
    let mut head = base.clone();
    head.trackers.get_mut("blog").expect("blog").clear();
    head.trackers
        .get_mut("root")
        .expect("root")
        .insert(b"posts/post.md".to_vec(), blob(post.as_bytes().to_vec()));
    let mut list = base.clone();
    list.trackers
        .get_mut("blog")
        .expect("blog")
        .insert(b"ana.md".to_vec(), blob(b"ana's\n".to_vec()));
    let verdict = check(&mut repo, &base, &head, &list).expect("agrees with Git");
    assert!(matches!(verdict, Verdict::Overlap), "{verdict:?}");

    // What Git's merge makes of it, which the server lands instead.
    let (b, h, l) = (repo.build(&base), repo.build(&head), repo.build(&list));
    let mut objects = Objects::open(&repo.dir).expect("objects");
    let views = [&b, &h, &l].map(|list| view(&mut objects, &list["root"]));
    objects.flush().expect("flush");
    let tree = git_merge(&repo.dir, &views[0], &views[1], &views[2]).expect("merges");
    let mut objects = Objects::open(&repo.dir).expect("objects");
    assert!(
        objects
            .entry_at(&tree, "posts/ana.md")
            .expect("read")
            .is_some()
    );
    assert!(
        objects
            .entry_at(&tree, "blog/ana.md")
            .expect("read")
            .is_none()
    );
}

/// A mount renamed by one side moves a whole directory of the view: the
/// other side's file added there is Git's to place.
#[test]
fn a_mount_renamed_overlaps_everything() {
    let mut repo = Repo::new("reparent-mount");
    let mut base = State::default();
    let mut root = with_root(&[(b"a.md", "a\n")]);
    root.insert(definition_path("blog"), definition("blog"));
    base.trackers.insert("root".into(), root);
    base.trackers
        .insert("blog".into(), with_root(&[(b"post.md", "post\n")]));
    let mut head = base.clone();
    head.trackers
        .get_mut("root")
        .expect("root")
        .insert(definition_path("blog"), definition("journal"));
    let mut list = base.clone();
    list.trackers
        .get_mut("blog")
        .expect("blog")
        .insert(b"ana.md".to_vec(), blob(b"ana's\n".to_vec()));
    let verdict = check(&mut repo, &base, &head, &list).expect("agrees with Git");
    assert!(matches!(verdict, Verdict::Overlap), "{verdict:?}");
}
