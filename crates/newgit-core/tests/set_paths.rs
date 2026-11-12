// Composing grafts every child of a parent at once when their mounts are
// apart (`Objects::set_paths`, #412): a parent of thousands of trackers then
// rewrites each directory once, not once per tracker. It must make exactly
// the tree that grafting them one at a time (`set_path`) makes, which
// composing did before and still does when mounts nest. Generated trees and
// edits from a fixed seed: files and directories where an edit lands,
// removals, files in the way of a path, names that are not UTF-8.

use camino::Utf8PathBuf;
use newgit_core::objects::{EMPTY_TREE, ModeId, Objects};

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

const NAMES: [&[u8]; 6] = [b"a", b"b", b"pkgs", b"caf\xe9", b"a-b", b"\xff"];

fn path(rng: &mut Rng) -> Vec<u8> {
    let depth = 1 + rng.below(3);
    let parts: Vec<&[u8]> = (0..depth).map(|_| NAMES[rng.below(NAMES.len())]).collect();
    parts.join(&b'/')
}

/// Whether `inner` is `outer` or inside it.
fn within(inner: &[u8], outer: &[u8]) -> bool {
    inner == outer || (inner.starts_with(outer) && inner.get(outer.len()) == Some(&b'/'))
}

#[test]
fn grafting_apart_mounts_at_once_is_grafting_them_one_by_one() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = Utf8PathBuf::from_path_buf(temp.path().join("r.git")).expect("utf8");
    let made = newgit_core::git::command()
        .args(["init", "-q", "--bare", repo.as_str()])
        .status()
        .expect("git");
    assert!(made.success());
    let mut objects = Objects::open(&repo).expect("objects");
    let mut rng = Rng(0x0412_9a7f);
    let mut cases = 0;
    for _ in 0..400 {
        // A tree of a few files, some of them where an edit's path goes.
        let mut tree = EMPTY_TREE.to_owned();
        for _ in 0..rng.below(8) {
            let blob = objects.make_blob(format!("{}\n", rng.next()).as_bytes());
            tree = objects
                .set_path(&tree, path(&mut rng), Some((b"100644".to_vec(), blob)))
                .expect("set");
        }
        // Edits whose paths are apart.
        let mut edits: Vec<(Vec<u8>, Option<ModeId>)> = Vec::new();
        for _ in 0..1 + rng.below(6) {
            let at = path(&mut rng);
            if edits
                .iter()
                .any(|(other, _)| within(&at, other) || within(other, &at))
            {
                continue;
            }
            let entry = match rng.below(3) {
                0 => None,
                1 => {
                    let blob = objects.make_blob(format!("{}\n", rng.next()).as_bytes());
                    Some((b"100644".to_vec(), blob))
                }
                _ => {
                    let blob = objects.make_blob(b"inside\n");
                    let sub = objects
                        .set_path(EMPTY_TREE, "f", Some((b"100644".to_vec(), blob)))
                        .expect("set");
                    Some((b"40000".to_vec(), sub))
                }
            };
            edits.push((at, entry));
        }
        let mut one_by_one = tree.clone();
        for (at, entry) in &edits {
            one_by_one = objects
                .set_path(&one_by_one, at, entry.clone())
                .expect("set");
        }
        let at_once = objects.set_paths(&tree, edits.clone()).expect("set_paths");
        assert_eq!(at_once, one_by_one, "{tree} with {edits:?}");
        cases += 1;
    }
    assert!(cases > 300);
}
