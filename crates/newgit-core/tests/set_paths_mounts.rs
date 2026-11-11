// Grafting a parent's children in one pass (`compose::graft_all`, #412's
// review, item 3) must agree with grafting them one at a time
// (`compose::graft_each`) for every set of mounts, not only the canonical
// ones it was written for. Mounts that are equal or nested by path but not
// by string (`pkgs/a` beside `/pkgs/a/b` or `/pkgs/a`, `pkgs//a`), `..`, `.`,
// and the empty mount fall back to one at a time; `Objects::set_paths`
// refuses paths that are not apart with an error, never an index out of
// bounds. Then a property test over generated mounts, canonical and not.

use camino::Utf8PathBuf;
use newgit_core::compose::{graft_all, graft_each};
use newgit_core::objects::{EMPTY_TREE, Objects, TREE_MODE};

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

fn repo() -> (tempfile::TempDir, Objects) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = Utf8PathBuf::from_path_buf(temp.path().join("r.git")).expect("utf8");
    let made = newgit_core::git::command()
        .args(["init", "-q", "--bare", repo.as_str()])
        .status()
        .expect("git");
    assert!(made.success());
    let objects = Objects::open(&repo).expect("objects");
    (temp, objects)
}

fn file_tree(objects: &mut Objects, path: &str, body: &str) -> String {
    let blob = objects.make_blob(body.as_bytes());
    objects
        .set_path(EMPTY_TREE, path, Some((b"100644".to_vec(), blob)))
        .expect("set")
}

fn agree(objects: &mut Objects, own: &str, children: &[(&str, &str)]) {
    let each = graft_each(objects, own, children);
    let all = graft_all(objects, own, children);
    match (each, all) {
        (Ok(each), Ok(all)) => assert_eq!(all, each, "mounts {children:?}"),
        (Err(_), Err(_)) => {}
        (each, all) => panic!("mounts {children:?}: one at a time {each:?}, at once {all:?}"),
    }
}

#[test]
fn mounts_equal_or_nested_by_path_graft_as_one_at_a_time() {
    let (_temp, mut objects) = repo();
    let own = file_tree(&mut objects, "README.md", "root\n");
    let a = file_tree(&mut objects, "a.txt", "a\n");
    let b = file_tree(&mut objects, "b.txt", "b\n");
    for mounts in [
        ["pkgs/a", "/pkgs/a/b"],
        ["pkgs/a", "/pkgs/a"],
        ["/pkgs/a", "pkgs/a"],
        ["pkgs/a", "pkgs//a"],
        ["pkgs/a/", "pkgs/a"],
        ["pkgs/../a", "a"],
        ["", "pkgs/a"],
        [".", "pkgs/a"],
        ["pkgs/a", "pkgs/b"],
    ] {
        let children = [(mounts[0], a.as_str()), (mounts[1], b.as_str())];
        agree(&mut objects, &own, &children);
    }
    // The empty mount, alone, grafts as it did before one-pass grafting
    // rather than being refused.
    let alone = graft_all(&mut objects, &own, &[("", a.as_str())]).expect("grafts");
    assert_eq!(
        alone,
        graft_each(&mut objects, &own, &[("", a.as_str())]).expect("grafts")
    );
}

#[test]
fn set_paths_refuses_paths_not_apart_rather_than_panicking() {
    let (_temp, mut objects) = repo();
    let own = file_tree(&mut objects, "README.md", "root\n");
    let child = file_tree(&mut objects, "a.txt", "a\n");
    let entry = || Some((TREE_MODE.to_vec(), child.clone()));
    for paths in [
        vec!["pkgs/a", "/pkgs/a/b"],
        vec!["pkgs/a", "/pkgs/a"],
        vec!["pkgs/a", "pkgs/a"],
        vec!["", "pkgs/a"],
        vec!["/", "x"],
        vec!["pkgs/a/b", "pkgs/a"],
    ] {
        let edits = paths
            .iter()
            .map(|path| (path.as_bytes().to_vec(), entry()))
            .collect();
        assert!(
            objects.set_paths(&own, edits).is_err(),
            "{paths:?} are not apart"
        );
    }
}

const PARTS: [&str; 7] = ["a", "b", "pkgs", "a-b", "..", ".", ""];

/// A mount: mostly canonical, sometimes absolute, doubled, trailing, with
/// `.` or `..`, or empty.
fn mount(rng: &mut Rng) -> String {
    let depth = 1 + rng.below(3);
    let parts: Vec<&str> = (0..depth)
        .map(|_| {
            if rng.below(5) == 0 {
                PARTS[rng.below(PARTS.len())]
            } else {
                PARTS[rng.below(4)]
            }
        })
        .collect();
    let mut mount = parts.join("/");
    match rng.below(10) {
        0 => mount.insert(0, '/'),
        1 => mount.push('/'),
        2 => mount.clear(),
        _ => {}
    }
    mount
}

#[test]
fn grafting_at_once_and_one_at_a_time_agree_on_generated_mounts() {
    let (_temp, mut objects) = repo();
    let mut rng = Rng(0x0412_3a17);
    for case in 0..500 {
        let mut own = EMPTY_TREE.to_owned();
        for _ in 0..rng.below(6) {
            let blob = objects.make_blob(format!("{}\n", rng.next()).as_bytes());
            let at = mount(&mut rng);
            if let Ok(tree) = objects.set_path(&own, &at, Some((b"100644".to_vec(), blob))) {
                own = tree;
            }
        }
        let count = 1 + rng.below(5);
        let mut mounts = Vec::new();
        let mut views = Vec::new();
        for index in 0..count {
            mounts.push(mount(&mut rng));
            views.push(file_tree(
                &mut objects,
                &format!("f{index}"),
                &format!("{case} {index}\n"),
            ));
        }
        let children: Vec<(&str, &str)> = mounts
            .iter()
            .map(String::as_str)
            .zip(views.iter().map(String::as_str))
            .collect();
        agree(&mut objects, &own, &children);
    }
}
