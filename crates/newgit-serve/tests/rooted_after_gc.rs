// A stored rooted history's map names view commits in blobs (`view <id>`),
// which Git does not follow (#458's review, item 1). A branch deleted leaves
// its views reached by nothing, so garbage collection drops them while the
// map still names them; the branch made again, or one built on a commit it
// had, must compose them again rather than serve, or build on, a view that
// is gone.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8Path;
use newgit_core::objects::{EMPTY_TREE, GITLINK_MODE, Objects};
use newgit_serve::repos::Store;
use newgit_serve::rooted::{self, Part, Want};

const PREFIX: &str = "refs/namespaces/r/refs/namespaces/root/";

fn git(dir: &Utf8Path, args: &[&str]) -> Option<String> {
    let output = newgit_core::git::command()
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn has(dir: &Utf8Path, object: &str) -> bool {
    git(dir, &["cat-file", "-e", object]).is_some()
}

struct World<'d> {
    dir: &'d Utf8Path,
    clock: u64,
}

impl World<'_> {
    /// A commit with `files` and, for root, `a` mounted at `a` pinned at
    /// `pin`; kept by a ref of its own, as a tracker's branch keeps it.
    fn commit(&mut self, files: &[(&str, &str)], pin: Option<&str>, parents: &[&str]) -> String {
        let mut objects = Objects::open(self.dir).expect("objects");
        let mut tree = EMPTY_TREE.to_owned();
        for (path, body) in files {
            let blob = objects.make_blob(body.as_bytes());
            tree = objects
                .set_path(&tree, path, Some((b"100644".to_vec(), blob)))
                .expect("set");
        }
        if let Some(pin) = pin {
            let definition = objects.make_blob(b"git = true\nmount = \"a\"\n");
            tree = objects
                .set_path(
                    &tree,
                    ".newgit/trackers/a.toml",
                    Some((b"100644".to_vec(), definition)),
                )
                .expect("set");
            tree = objects
                .set_path(
                    &tree,
                    ".newgit/pins/a",
                    Some((GITLINK_MODE.to_vec(), pin.to_owned())),
                )
                .expect("set");
        }
        self.clock += 1;
        let when = format!("Ana <ana@example.com> {} +0000", 1_700_000_000 + self.clock);
        let parents: Vec<String> = parents.iter().map(|p| (*p).to_owned()).collect();
        let id = objects.make_commit(
            &tree,
            &parents,
            when.as_bytes(),
            when.as_bytes(),
            b"",
            b"step",
        );
        objects.flush().expect("flush");
        git(self.dir, &["update-ref", &format!("refs/keep/{id}"), &id]).expect("kept");
        id
    }
}

fn want(home: &Utf8Path, heads: &[(&str, &str)]) -> Want {
    let set: BTreeSet<String> = ["root".to_owned(), "a".to_owned()].into();
    Want {
        home: home.to_owned(),
        tracker: "root".to_owned(),
        store: Store::ProjectDevs,
        default_branch: "main".to_owned(),
        heads: heads
            .iter()
            .map(|(name, source)| {
                (
                    (*name).to_owned(),
                    Part {
                        source: (*source).to_owned(),
                        set: set.clone(),
                        annotation: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        tags: BTreeMap::new(),
        stores: set
            .iter()
            .map(|name| (name.clone(), Store::ProjectDevs))
            .collect(),
        at: rooted::At::default(),
        own_head: None,
    }
}

/// What the store's branches and kept commits do not reach goes now,
/// reflogs and all: the collection `maintenance` runs, with no grace.
fn collect(dir: &Utf8Path) {
    git(
        dir,
        &[
            "-c",
            "gc.reflogExpire=now",
            "-c",
            "gc.reflogExpireUnreachable=now",
            "gc",
            "--quiet",
            "--prune=now",
        ],
    )
    .expect("gc");
}

#[test]
fn a_view_collected_while_the_map_names_it_is_composed_again() {
    let temp = support::tempdir("rooted-after-gc");
    let home = temp.join("homes/jack/p");
    let dir = Store::ProjectDevs.dir(&home);
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "--bare", dir.as_str()]).expect("init");
    let mut world = World {
        dir: &dir,
        clock: 0,
    };

    let a1 = world.commit(&[("f", "1\n")], None, &[]);
    let main = world.commit(&[("r", "1\n")], Some(&a1), &[]);
    let a2 = world.commit(&[("f", "2\n")], None, &[&a1]);
    let dev1 = world.commit(&[("r", "1\n")], Some(&a2), &[&main]);
    let a3 = world.commit(&[("f", "3\n")], None, &[&a2]);
    let dev2 = world.commit(&[("r", "1\n")], Some(&a3), &[&dev1]);

    rooted::refresh(&want(&home, &[("main", &main), ("dev", &dev2)])).expect("refresh");
    let dev_view = git(&dir, &["rev-parse", &format!("{PREFIX}refs/heads/dev")]).expect("dev");
    let dev_first = git(&dir, &["rev-parse", &format!("{dev_view}^")]).expect("its parent");

    // `dev` deleted, and collected: its views are gone, the map still names
    // them.
    rooted::refresh(&want(&home, &[("main", &main)])).expect("refresh");
    collect(&dir);
    assert!(!has(&dir, &dev_view), "dev's view went with it");
    assert!(!has(&dir, &dev_first));

    // Made again where it was: its views are composed again, the same.
    let done = rooted::refresh(&want(&home, &[("main", &main), ("dev", &dev2)]))
        .expect("dev is served again");
    assert_eq!(done.composed, 2, "{done:?}");
    assert_eq!(
        git(&dir, &["rev-parse", &format!("{PREFIX}refs/heads/dev")]).as_deref(),
        Some(dev_view.as_str())
    );

    // Deleted and collected again, then made on a commit built on what it
    // had: the new view's parents are composed again too.
    rooted::refresh(&want(&home, &[("main", &main)])).expect("refresh");
    collect(&dir);
    assert!(!has(&dir, &dev_view));
    let a4 = world.commit(&[("f", "4\n")], None, &[&a3]);
    let dev3 = world.commit(&[("r", "1\n")], Some(&a4), &[&dev2]);
    let done = rooted::refresh(&want(&home, &[("main", &main), ("dev", &dev3)]))
        .expect("built on what dev had");
    assert_eq!(done.composed, 3, "{done:?}");
    let tip = git(&dir, &["rev-parse", &format!("{PREFIX}refs/heads/dev")]).expect("dev");
    assert_eq!(
        git(&dir, &["rev-parse", &format!("{tip}^")]).as_deref(),
        Some(dev_view.as_str()),
        "on the view composed again"
    );
    git(
        &dir,
        &[
            "fsck",
            "--connectivity-only",
            "--no-dangling",
            "--no-progress",
        ],
    )
    .expect("the store is whole");
    let _ = std::fs::remove_dir_all(&temp);
}
