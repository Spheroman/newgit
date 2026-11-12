// What stored rooted histories cost (#412): a synthetic project of
// `BENCH_TRACKERS` trackers (2,000 by default) mounted flat under `pkgs/` in
// root, each with a short history of its own, all in the members' store.
// Ignored by default; run with
//
//   cargo test --release -p newgit-serve --test rooted_bench -- --ignored --nocapture
//
// It prints: the backfill of every tracker's stored history; per landing
// (one tracker's file changed, root's pin moved), the extension of that
// tracker's and root's; a clone of one tracker's stored rooted view (time
// and bytes), against composing that view from scratch as a fetch did
// before; and what composing root's view from scratch costs.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, ComposeOptions};
use newgit_core::objects::{EMPTY_TREE, GITLINK_MODE, Objects};
use newgit_serve::repos::Store;
use newgit_serve::rooted::{self, Part, Want};

mod support;

const NOBODY: &[u8] = b"Ana <ana@example.com> 1700000000 +0000";

fn trackers() -> usize {
    std::env::var("BENCH_TRACKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
}

fn name(index: usize) -> String {
    format!("pkg{index:05}")
}

struct Project {
    home: Utf8PathBuf,
    heads: BTreeMap<String, String>,
    serial: u64,
}

impl Project {
    fn leaf_commit(&mut self, objects: &mut Objects, leaf: &str) -> String {
        self.serial += 1;
        let blob = objects.make_blob(format!("{leaf} {}\n", self.serial).as_bytes());
        let tree = objects
            .set_path(EMPTY_TREE, "package.nix", Some((b"100644".to_vec(), blob)))
            .expect("set");
        let parents: Vec<String> = self.heads.get(leaf).cloned().into_iter().collect();
        let id = objects.make_commit(
            &tree,
            &parents,
            NOBODY,
            NOBODY,
            b"",
            format!("{leaf}: {}\n", self.serial).as_bytes(),
        );
        self.heads.insert(leaf.to_owned(), id.clone());
        id
    }

    /// Root's commit pinning every leaf: definitions and pins written as
    /// one tree each.
    fn root_commit(&mut self, objects: &mut Objects, definitions: &str, message: &str) -> String {
        let pins: Vec<newgit_core::objects::Entry> = self
            .heads
            .iter()
            .filter(|(name, _)| name.as_str() != "root")
            .map(|(name, commit)| newgit_core::objects::Entry {
                mode: GITLINK_MODE.to_vec(),
                name: name.as_bytes().to_vec(),
                id: commit.clone(),
            })
            .collect();
        let pins = objects.make_tree(pins);
        let readme = objects.make_blob(b"root\n");
        let mut tree = objects
            .set_path(EMPTY_TREE, "README.md", Some((b"100644".to_vec(), readme)))
            .expect("set");
        tree = objects
            .set_path(&tree, ".newgit/pins", Some((b"40000".to_vec(), pins)))
            .expect("set");
        tree = objects
            .set_path(
                &tree,
                ".newgit/trackers",
                Some((b"40000".to_vec(), definitions.to_owned())),
            )
            .expect("set");
        let parents: Vec<String> = self.heads.get("root").cloned().into_iter().collect();
        let id = objects.make_commit(&tree, &parents, NOBODY, NOBODY, b"", message.as_bytes());
        self.heads.insert("root".to_owned(), id.clone());
        id
    }

    fn want(&self, tracker: &str, set: &BTreeSet<String>) -> Want {
        Want {
            home: self.home.clone(),
            tracker: tracker.to_owned(),
            store: Store::ProjectDevs,
            default_branch: "main".to_owned(),
            heads: BTreeMap::from([(
                "main".to_owned(),
                Part {
                    source: self.heads[tracker].clone(),
                    set: set.clone(),
                    annotation: None,
                },
            )]),
            tags: BTreeMap::new(),
            stores: set
                .iter()
                .map(|name| (name.clone(), Store::ProjectDevs))
                .collect(),
            at: rooted::At::default(),
            own_head: None,
        }
    }
}

fn dir_bytes(dir: &Utf8Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.as_std_path().to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
            let meta = entry.metadata().expect("meta");
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[test]
#[ignore = "a benchmark: run it by hand"]
fn rooted_histories_at_scale() {
    let count = trackers();
    let temp = support::tempdir("rooted-bench");
    let home = temp.join("homes/jack/p");
    let devs = Store::ProjectDevs.dir(&home);
    std::fs::create_dir_all(&devs).expect("mkdir");
    support::git(
        &devs,
        &[
            "init",
            "-q",
            "--bare",
            "--ref-format=reftable",
            devs.as_str(),
        ],
    );
    let mut project = Project {
        home: home.clone(),
        heads: BTreeMap::new(),
        serial: 0,
    };

    // Every tracker with a few commits, then root pinning them all.
    let started = Instant::now();
    let mut objects = Objects::open(&devs).expect("objects");
    let mut definitions = Vec::new();
    for index in 0..count {
        let leaf = name(index);
        for _ in 0..(1 + index % 4) {
            project.leaf_commit(&mut objects, &leaf);
        }
        let text = format!("git = true\nmount = \"pkgs/{leaf}\"\n");
        definitions.push(newgit_core::objects::Entry {
            mode: b"100644".to_vec(),
            name: format!("{leaf}.toml").into_bytes(),
            id: objects.make_blob(text.as_bytes()),
        });
    }
    let definitions = objects.make_tree(definitions);
    project.root_commit(&mut objects, &definitions, "every package\n");
    objects.flush().expect("flush");
    drop(objects);
    eprintln!(
        "built {count} trackers in {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let all: BTreeSet<String> = std::iter::once("root".to_owned())
        .chain((0..count).map(name))
        .collect();
    let set_of = |tracker: &str| -> BTreeSet<String> {
        if tracker == "root" {
            all.clone()
        } else {
            BTreeSet::from([tracker.to_owned()])
        }
    };

    // Backfill: every tracker's stored history, as a start after an
    // upgrade composes them.
    let before = dir_bytes(&devs);
    let started = Instant::now();
    for index in 0..count {
        let leaf = name(index);
        rooted::refresh(&project.want(&leaf, &set_of(&leaf))).expect("leaf");
    }
    let leaves = started.elapsed();
    let started = Instant::now();
    rooted::refresh(&project.want("root", &all)).expect("root");
    let root_backfill = started.elapsed();
    eprintln!(
        "backfill: {count} trackers' histories {:.1} s ({:.2} ms each), root's {:.1} s; store grew {:.1} MB",
        leaves.as_secs_f64(),
        ms(leaves) / count as f64,
        root_backfill.as_secs_f64(),
        (dir_bytes(&devs) - before) as f64 / 1e6
    );

    // Landings: one package changes, root's pin follows.
    let landings = 30;
    let mut leaf_times = Vec::new();
    let mut root_times = Vec::new();
    let before = dir_bytes(&devs);
    for landing in 0..landings {
        let leaf = name((landing * 7919) % count);
        let mut objects = Objects::open(&devs).expect("objects");
        project.leaf_commit(&mut objects, &leaf);
        project.root_commit(&mut objects, &definitions, &format!("land {leaf}\n"));
        objects.flush().expect("flush");
        drop(objects);
        let started = Instant::now();
        let done = rooted::refresh(&project.want(&leaf, &set_of(&leaf))).expect("leaf");
        leaf_times.push(ms(started.elapsed()));
        assert_eq!(done.composed, 1);
        let started = Instant::now();
        let done = rooted::refresh(&project.want("root", &all)).expect("root");
        root_times.push(ms(started.elapsed()));
        assert_eq!(done.composed, 1);
    }
    leaf_times.sort_by(f64::total_cmp);
    root_times.sort_by(f64::total_cmp);
    eprintln!(
        "per landing ({landings}): the tracker's extension p50 {:.1} ms p99 {:.1} ms; root's p50 {:.1} ms p99 {:.1} ms; store grew {:.2} MB a landing",
        percentile(&leaf_times, 0.5),
        percentile(&leaf_times, 0.99),
        percentile(&root_times, 0.5),
        percentile(&root_times, 0.99),
        (dir_bytes(&devs) - before) as f64 / 1e6 / landings as f64
    );
    // Checked and fresh: what a fetch pays when nothing landed since.
    let started = Instant::now();
    let fresh = rooted::refresh(&project.want("root", &all)).expect("fresh");
    assert!(fresh.fresh);
    eprintln!("a fresh check: {:.1} ms", ms(started.elapsed()));

    // A clone of one tracker's stored rooted view, as upload-pack serves it.
    let leaf = name(0);
    let mut clone_ms = Vec::new();
    let mut bytes = 0;
    for rep in 0..5 {
        let into = temp.join(format!("clone-{rep}"));
        let started = Instant::now();
        let output = newgit_core::git::command()
            .args([
                "clone",
                "-q",
                "--bare",
                "--no-local",
                &format!("file://{devs}"),
                into.as_str(),
            ])
            .env("GIT_NAMESPACE", rooted::namespace(&leaf))
            .output()
            .expect("clone");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        clone_ms.push(ms(started.elapsed()));
        bytes = dir_bytes(&into.join("objects"));
    }
    clone_ms.sort_by(f64::total_cmp);
    eprintln!(
        "clone of `{leaf}`'s stored view: {:.0} ms (median of 5), {:.1} KB of objects",
        percentile(&clone_ms, 0.5),
        bytes as f64 / 1e3
    );

    // Composing it per fetch, as before: every commit of its history, then
    // a repository of its own (what `endpoint::compose_view` did each time
    // anything landed).
    let scratch = temp.join("scratch.git");
    support::git(&temp, &["init", "-q", "--bare", scratch.as_str()]);
    std::fs::write(
        scratch.join("objects/info/alternates"),
        format!("{devs}/objects\n"),
    )
    .expect("alt");
    for tracker in [leaf.as_str(), "root"] {
        let set = set_of(tracker);
        let readable = |name: &str| set.contains(name);
        let options = ComposeOptions {
            readable: &readable,
            admit: None,
        };
        let started = Instant::now();
        compose::compose(
            &scratch,
            tracker,
            &[(
                format!("refs/views/{tracker}"),
                project.heads[tracker].clone(),
            )],
            &options,
        )
        .expect("compose");
        eprintln!(
            "composing `{tracker}`'s view from scratch: {:.0} ms",
            ms(started.elapsed())
        );
    }
    eprintln!("data: {:.1} MB in the store", dir_bytes(&devs) as f64 / 1e6);
    let _ = std::fs::remove_dir_all(&temp);
}
