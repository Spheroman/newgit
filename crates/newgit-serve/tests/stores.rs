// A store is made for one audience and never takes a second (#409): it is
// never a boundary within itself, so the only boundary is between stores.

use camino::Utf8PathBuf;
use newgit_serve::repos::{self, Repo, Store};

fn tempdir(name: &str) -> Utf8PathBuf {
    let dir = std::env::temp_dir().join(format!("newgit-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8")
}

#[test]
fn a_store_holds_the_audience_it_was_made_for_and_no_other() {
    let home = tempdir("stores");
    let devs = Store::ProjectDevs.dir(&home);
    repos::create_store(&devs, &Store::ProjectDevs).expect("made");
    repos::create_store(&devs, &Store::ProjectDevs).expect("made again: a no-op");
    assert!(repos::create_store(&devs, &Store::Public).is_err());

    // A tracker's repo is a namespace in it, with its own HEAD.
    let root = Repo::new(&home, Store::ProjectDevs, "t/root".into());
    assert!(!root.exists());
    root.create().expect("root");
    assert!(root.exists());
    assert_eq!(root.dir(), devs);

    // A seed alone moves by renaming its store when it is its audience's
    // first, and by copying when the audience has one already.
    let site = Repo::new(&home, Store::Tracker("site".into()), "t/site".into());
    site.create().expect("site");
    let copied = site.copy_to(&Store::Public).expect("copied");
    site.finish_move(&copied).expect("moved");
    let moved = copied.to;
    assert!(moved.exists() && !site.dir().exists());
    assert_eq!(repos::audience_of(&moved.dir()).as_deref(), Some("public"));
    let lib = Repo::new(&home, Store::Tracker("lib".into()), "t/lib".into());
    lib.create().expect("lib");
    let copied = lib.copy_to(&Store::ProjectDevs).expect("copied");
    lib.finish_move(&copied).expect("moved");
    let moved = copied.to;
    assert!(moved.exists() && root.exists() && !lib.dir().exists());
    assert!(!home.join("stores/tracker").exists());

    let _ = std::fs::remove_dir_all(&home);
}
