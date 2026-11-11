//! Compose and split, driven by generated histories. Everything newgit does
//! with a commit rests on these two being exact inverses, and the bugs found
//! in them so far were all in a shape nobody had written a test for: a child
//! nested in a child, a name repeated across parents, a branch from before a
//! carve. So nobody writes the shapes here.
//!
//! Each seed builds one history of edits, spanning commits, carves, and merges,
//! and checks after every step what must hold of any history at all. A failure
//! prints the seed and the steps that led to it. To look harder:
//!
//!     NEWGIT_FUZZ_SEEDS=500 cargo test -p newgit-core --test compose_fuzz
//!     NEWGIT_FUZZ_SEED=1234 cargo test -p newgit-core --test compose_fuzz
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions, SplitOptions};
use newgit_core::tracker::child_id;

/// How many histories a plain `cargo test` tries, and how long each is.
const SEEDS: u64 = 12;
const STEPS: usize = 12;

/// Checking is most of the cost, so every third step, and always the last.
const CHECK_EVERY: usize = 3;

/// A small generator, so a seed means the same history on every machine and
/// every version of every dependency.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .output()
        .expect("git runs");
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[]).unwrap_or_else(|error| panic!("{error}"))
}

const ROOT_TOML: &str =
    "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\n";
const ACCESS_TOML: &str = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \".newgit/trackers\"\npaths = [\".\"]\nmaintainers = \"owner\"\n";

fn mounted(mount: &str) -> String {
    format!(
        "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \"{mount}\"\npaths = [\".\"]\n"
    )
}

/// A directory the generator can edit, carve, or both.
const DIRECTORIES: [&str; 8] = [
    "apps/a",
    "apps/a/lib",
    "apps/a/lib/deep",
    "apps/b",
    "apps/b/lib",
    "docs",
    "tools/x",
    "tools/x/lib",
];

/// A tracker carved so far: its id, and where it sits in the view.
#[derive(Debug, Clone)]
struct Carved {
    id: String,
    mount: String,
}

struct History {
    dir: Utf8PathBuf,
    rng: Random,
    time: i64,
    carved: Vec<Carved>,
    files: Vec<String>,
    tips: Vec<String>,
    serial: usize,
    steps: Vec<String>,
}

impl History {
    fn who(&mut self) -> Vec<(&'static str, String)> {
        self.time += 100;
        vec![
            ("GIT_AUTHOR_NAME", "ana".to_owned()),
            ("GIT_AUTHOR_EMAIL", "ana@example.com".to_owned()),
            ("GIT_AUTHOR_DATE", format!("@{} +0000", self.time)),
            ("GIT_COMMITTER_NAME", "ana".to_owned()),
            ("GIT_COMMITTER_EMAIL", "ana@example.com".to_owned()),
            ("GIT_COMMITTER_DATE", format!("@{} +0000", self.time)),
        ]
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    fn commit(&mut self, message: &str) -> String {
        git(&self.dir, &["add", "-A"]);
        let who = self.who();
        git_env(
            &self.dir,
            &["commit", "-q", "--allow-empty", "-m", message],
            &who,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let tip = git(&self.dir, &["rev-parse", "HEAD"]);
        self.tips.push(tip.clone());
        tip
    }

    fn new_file(&mut self) -> String {
        self.serial += 1;
        let directory = *self.rng.pick(&DIRECTORIES);
        let path = format!("{directory}/f{}.txt", self.serial);
        self.write(&path, &format!("{}\n", self.serial));
        path
    }

    fn step(&mut self) {
        match self.rng.below(12) {
            0..=3 => {
                let path = self.new_file();
                self.steps.push(format!("add {path}"));
                self.files.push(path);
                self.commit("add");
            }
            4 => {
                let (one, two) = (self.new_file(), self.new_file());
                self.steps.push(format!("add both {one} {two}"));
                self.files.extend([one, two]);
                self.commit("add across trackers");
            }
            5 if !self.files.is_empty() => {
                let path = self.rng.pick(&self.files).clone();
                self.serial += 1;
                self.write(&path, &format!("edit {}\n", self.serial));
                self.steps.push(format!("edit {path}"));
                self.commit("edit");
            }
            6 if !self.files.is_empty() => {
                let at = self.rng.below(self.files.len());
                let path = self.files.remove(at);
                std::fs::remove_file(self.dir.join(&path)).expect("remove");
                self.steps.push(format!("delete {path}"));
                self.commit("delete");
            }
            7..=9 => self.carve(),
            _ => self.merge(),
        }
    }

    /// Carve a directory, nested in the deepest tracker holding it, with a
    /// name that may repeat one in another parent.
    fn carve(&mut self) {
        let free: Vec<&str> = DIRECTORIES
            .iter()
            .copied()
            .filter(|directory| {
                // Carving around a tracker is refused (`tracker create`), and
                // a view rooted there would lack it.
                let inside = format!("{directory}/");
                !self.carved.iter().any(|tracker| {
                    tracker.mount == *directory || tracker.mount.starts_with(&inside)
                })
            })
            .collect();
        if free.is_empty() {
            return;
        }
        let mount = (*self.rng.pick(&free)).to_owned();
        self.carve_at(mount);
    }

    fn carve_at(&mut self, mount: String) {
        let parent = self
            .carved
            .iter()
            .filter(|tracker| mount.starts_with(&format!("{}/", tracker.mount)))
            .max_by_key(|tracker| tracker.mount.len())
            .cloned();
        let leaf = mount.rsplit('/').next().expect("a name").to_owned();
        let (definition, relative, id) = match &parent {
            Some(parent) => {
                let relative = mount[parent.mount.len() + 1..].to_owned();
                (
                    format!("{}/.newgit/trackers/{leaf}.toml", parent.mount),
                    relative,
                    child_id(&parent.id, &leaf),
                )
            }
            None => (
                format!(".newgit/trackers/{leaf}.toml"),
                mount.clone(),
                child_id("root", &leaf),
            ),
        };
        if self.carved.iter().any(|tracker| tracker.id == id) {
            return;
        }
        self.write(&definition, &mounted(&relative));
        self.serial += 1;
        self.write(&format!("{mount}/seed{}.txt", self.serial), "seed\n");
        self.steps.push(format!("carve {id} at {mount}"));
        self.carved.push(Carved { id, mount });
        self.commit("carve");
    }

    /// A side branch from somewhere behind the tip, a commit on it, and a
    /// merge back. It may start from before a carve its files now fall under.
    fn merge(&mut self) {
        let base = self.rng.pick(&self.tips).clone();
        self.serial += 1;
        let side = format!("side{}", self.serial);
        git(&self.dir, &["switch", "-q", "-c", &side, &base]);
        let tip_files = self.files.clone();
        let path = self.new_file();
        self.commit("work on a side branch");
        git(&self.dir, &["switch", "-q", "main"]);
        let who = self.who();
        let merged = git_env(
            &self.dir,
            &["merge", "-q", "--no-ff", "-m", "merge", &side],
            &who,
        );
        self.files = tip_files;
        match merged {
            Ok(_) => {
                self.files.push(path.clone());
                self.steps.push(format!("merge {path} from {}", &base[..8]));
                let tip = git(&self.dir, &["rev-parse", "HEAD"]);
                self.tips.push(tip);
            }
            Err(_) => {
                git(&self.dir, &["merge", "--abort"]);
            }
        }
    }

    fn split(&self, tip: &str) -> compose::SplitOutcome {
        let bound = |_: &str| true;
        let audience = |_: &str| None;
        compose::split(
            &self.dir,
            tip,
            &SplitOptions {
                root: "root",
                bound: &bound,
                audience: &audience,
                newgit_made: None,
                need_history: &|_| Ok(()),
                forks_before: &[],
            },
        )
        .unwrap_or_else(|error| panic!("split {tip}: {error}"))
    }
}

fn compose_at(
    dir: &Utf8Path,
    root: &str,
    commit: &str,
    may: &dyn Fn(&str) -> bool,
) -> Option<String> {
    compose::compose(
        dir,
        root,
        &[("refs/heads/fuzz-view".to_owned(), commit.to_owned())],
        &ComposeOptions {
            readable: may,
            admit: None,
        },
    )
    .expect("compose")
    .remove(0)
    .tip
}

/// The view from a repository holding only tracker commits: the remote's
/// position, with no view commits and no notes.
fn compose_elsewhere(
    from: &Utf8Path,
    index: &BTreeMap<String, String>,
    root: &str,
) -> Option<String> {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8");
    git(&dir, &["init", "-q", "--bare"]);
    for (tracker, commit) in index {
        git(
            from,
            &["update-ref", &format!("refs/fuzz/{tracker}"), commit],
        );
    }
    git(
        &dir,
        &["fetch", "-q", from.as_str(), "+refs/fuzz/*:refs/trackers/*"],
    );
    compose_at(&dir, "root", root, &|_| true)
}

fn check(history: &History, tip: &str) {
    let dir = &history.dir;
    let outcome = history.split(tip);
    let index = compose::index(dir, "root", &outcome.root).expect("index");

    // Composing what a split made gives the very commit back.
    assert_eq!(
        compose_at(dir, "root", &outcome.root, &|_| true).as_deref(),
        Some(tip),
        "compose did not give the commit back"
    );
    // So does a repository with nothing but tracker commits.
    assert_eq!(
        compose_elsewhere(dir, &index, &outcome.root).as_deref(),
        Some(tip),
        "compose from tracker commits alone gave another commit"
    );
    // Splitting it again makes nothing.
    let again = history.split(tip);
    assert_eq!(again.root, outcome.root, "a second split moved root");
    assert!(
        again.made.is_empty(),
        "a second split made {:?}",
        again.made
    );

    let view = git(dir, &["ls-tree", "-r", "--name-only", tip]);
    for tracker in &history.carved {
        let Some(commit) = index.get(&tracker.id) else {
            continue;
        };
        // Rooted at a tracker, the view is that tracker's directory.
        let at = format!("{tip}:{}", tracker.mount);
        let expected = git(dir, &["rev-parse", &at]);
        let rooted = compose_at(dir, &tracker.id, commit, &|_| true).expect("a rooted view");
        assert_eq!(
            git(dir, &["rev-parse", &format!("{rooted}^{{tree}}")]),
            expected,
            "the view rooted at {} is not its directory",
            tracker.id
        );

        // A reader who may not read it gets nothing under its mount, and
        // everything that is not.
        let hidden = tracker.id.clone();
        let Some(read) = compose_at(dir, "root", &outcome.root, &|name| name != hidden) else {
            continue;
        };
        let seen = git(dir, &["ls-tree", "-r", "--name-only", &read]);
        let under = format!("{}/", tracker.mount);
        assert!(
            !seen.lines().any(|path| path.starts_with(&under)),
            "a reader of everything but {} saw under {under}",
            tracker.id
        );
        for path in view.lines().filter(|path| !path.starts_with(&under)) {
            assert!(
                seen.lines().any(|line| line == path),
                "hiding {} also hid {path}",
                tracker.id
            );
        }
    }
}

fn run(seed: u64, steps: usize) {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "commit.gpgSign", "false"]);
    let mut history = History {
        dir: dir.clone(),
        rng: Random::new(seed),
        time: 1_000,
        carved: Vec::new(),
        files: Vec::new(),
        tips: Vec::new(),
        serial: 0,
        steps: Vec::new(),
    };
    history.write("README.md", "hello\n");
    let first = history.commit("start");
    git(
        &dir,
        &["update-ref", "refs/newgit/trackers/root/main", &first],
    );
    history.write(".newgit/trackers/root.toml", ROOT_TOML);
    history.write(".newgit/trackers/access.toml", ACCESS_TOML);
    history.commit("definitions");
    // Nesting is where the hard cases are, so every history has some: a chain
    // two deep, with `lib` as a name another parent can repeat.
    let chain = *history.rng.pick(&[
        ["apps/a", "apps/a/lib"],
        ["apps/b", "apps/b/lib"],
        ["tools/x", "tools/x/lib"],
    ]);
    for mount in chain {
        history.carve_at(mount.to_owned());
    }

    for step in 1..=steps {
        history.step();
        if step % CHECK_EVERY != 0 && step != steps {
            continue;
        }
        let tip = git(&dir, &["rev-parse", "HEAD"]);
        let checked =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&history, &tip)));
        if let Err(failure) = checked {
            eprintln!(
                "\nseed {seed} failed after:\n  {}\nrerun with NEWGIT_FUZZ_SEED={seed}\n",
                history.steps.join("\n  ")
            );
            std::panic::resume_unwind(failure);
        }
    }
}

#[test]
fn compose_and_split_are_inverses_on_any_history() {
    if let Ok(seed) = std::env::var("NEWGIT_FUZZ_SEED") {
        run(seed.parse().expect("a number"), STEPS * 3);
        return;
    }
    let seeds = std::env::var("NEWGIT_FUZZ_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(SEEDS);
    let threads = std::thread::available_parallelism().map_or(4, |count| count.get() as u64);
    let next = std::sync::atomic::AtomicU64::new(1);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if seed > seeds {
                        return;
                    }
                    run(seed, STEPS);
                }
            });
        }
    });
}
