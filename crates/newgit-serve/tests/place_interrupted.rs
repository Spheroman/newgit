//! A placement stopped part way, as a crash leaves it, is finished by the
//! next one, and the store a landing then names its commit in passes `git
//! fsck` (#460 review). The in-process copy puts each object in place only
//! after everything it names, so a store that has the commit has all of
//! it: the next placement, which skips a commit the store has, skips it
//! only when nothing is missing.

use camino::Utf8Path;
use newgit_core::objects::{self, Entry, Objects};
use newgit_serve::readers::Readers;
use newgit_serve::repos::{Repo, Store};

mod support;

const BY: &[u8] = b"t <t@example.com> 1 +0000";

/// Two commits waiting in `landing` at their landing refs, each adding a
/// file under `docs/`: the second's id, and how many objects they are.
fn waiting(landing: &Utf8Path) -> (String, usize) {
    let mut made = Objects::open(landing).expect("open");
    let mut parents = Vec::new();
    let mut entries = Vec::new();
    for name in ["a.md", "b.md"] {
        entries.push(Entry {
            mode: b"100644".to_vec(),
            name: name.as_bytes().to_vec(),
            id: made.make_blob(format!("{name}\n").as_bytes()),
        });
        let docs = made.make_tree(entries.clone());
        let tree = made.make_tree(vec![Entry {
            mode: b"40000".to_vec(),
            name: b"docs".to_vec(),
            id: docs,
        }]);
        parents = vec![made.make_commit(&tree, &parents, BY, BY, b"", name.as_bytes())];
    }
    made.flush().expect("flush");
    let tip = parents.remove(0);
    support::git(
        landing,
        &["update-ref", &format!("refs/newgit/landing/{tip}"), &tip],
    );
    (tip, 8)
}

#[test]
fn a_placement_after_one_stopped_part_way_leaves_the_store_whole() {
    let temp = support::tempdir("place-interrupted");
    for renames in 0..8 {
        let home = temp.join(format!("home-{renames}"));
        std::fs::create_dir_all(&home).expect("mkdir");
        let root = Repo::new(&home, Store::ProjectDevs, "t/root".into());
        root.create().expect("store");
        let landing = root.open_landing("ana").expect("landing");
        let (tip, made) = waiting(&landing);
        assert!(renames < made);

        objects::stop_writes_after(Some(renames));
        let stopped = root.place(&mut Readers::new(), "ana", &tip, &root);
        objects::stop_writes_after(None);
        assert!(stopped.is_err(), "stopped after {renames}");

        let placed = root
            .place(&mut Readers::new(), "ana", &tip, &root)
            .expect("placed");
        assert!(placed.is_some(), "the commit waited in the landing");
        // What a landing then does: name the commit in the store.
        let store = root.dir();
        support::git(&store, &["update-ref", &root.full("refs/heads/main"), &tip]);
        support::git(&store, &["fsck", "--strict", "--no-dangling"]);
        assert_eq!(
            support::git(&store, &["rev-list", "--objects", &tip])
                .lines()
                .count(),
            made,
            "after {renames}"
        );
    }
    let _ = std::fs::remove_dir_all(&temp);
}
