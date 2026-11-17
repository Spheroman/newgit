// Garbage collection (#434) at its edges (#458's review, items 3, 5, 7):
//
// - a commit a force push left behind goes: a store's reflog keeps a ref's
//   earlier values for the grace, not Git's 30 days;
// - a merge's landing with a ref on a commit whose history is missing
//   still has what is there kept, and its store is still collected;
// - a landing is matched to the store it borrows by canonical path, so one
//   whose alternates spell the path another way is still kept for, and one
//   with a ref that borrows from no store refuses its home's collection.

mod support;

use std::time::Duration;

use camino::Utf8Path;
use newgit_serve::maintenance;
use support::{HOME, git, server, tempdir};

fn has(dir: &Utf8Path, object: &str) -> bool {
    std::process::Command::new("git")
        .args(["--git-dir", dir.as_str(), "cat-file", "-e", object])
        .status()
        .expect("git")
        .success()
}

fn collect(app: &newgit_serve::App) -> newgit_serve::error::ApiResult<maintenance::Collected> {
    maintenance::collect_home_within(app, HOME, Duration::ZERO)
}

/// A commit in `work` on `base`, with one file of its own; returns it and
/// that file's blob.
fn commit_on(work: &Utf8Path, base: &str, name: &str) -> (String, String) {
    git(work, &["checkout", "-q", "--detach", base]);
    std::fs::write(work.join(name), format!("{name}\n")).expect("write");
    git(work, &["add", "."]);
    git(work, &["commit", "-q", "-m", name]);
    (
        git(work, &["rev-parse", "HEAD"]),
        git(work, &["rev-parse", &format!("HEAD:{name}")]),
    )
}

#[test]
fn a_commit_a_force_push_left_behind_goes() {
    let data = tempdir("gc-edges-force");
    let (app, root, commits) = server(&data, 2);
    let store = root.dir();
    let work = data.join("work-jack-p");
    let (left, left_blob) = commit_on(&work, &commits[1], "left");
    let branch = format!("{}refs/heads/side", root.prefix());
    git(
        &work,
        &["push", "-q", store.as_str(), &format!("{left}:{branch}")],
    );
    git(
        &work,
        &[
            "push",
            "-q",
            "-f",
            store.as_str(),
            &format!("{}:{branch}", commits[0]),
        ],
    );
    assert!(
        !git(&store, &["reflog", "show", &branch]).is_empty(),
        "stores keep reflogs"
    );
    // Reflog times are in whole seconds, and one of this second is not
    // older than "0 seconds ago".
    std::thread::sleep(Duration::from_millis(1100));
    collect(&app).expect("collects");
    assert!(!has(&store, &left), "the commit left behind");
    assert!(!has(&store, &left_blob), "and its content");
    assert!(has(&store, &commits[1]));
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_landing_with_missing_history_still_keeps_what_is_there() {
    let data = tempdir("gc-edges-missing");
    let (app, root, commits) = server(&data, 2);
    let store = root.dir();
    let work = data.join("work-jack-p");
    let (only, only_blob) = commit_on(&work, &commits[1], "only");
    let tree = git(&work, &["rev-parse", &format!("{only}^{{tree}}")]);
    git(
        &work,
        &[
            "push",
            "-q",
            store.as_str(),
            &format!("{only}:refs/heads/only"),
        ],
    );

    // ana's merge: a commit on the store's tree, whose parent, in her
    // landing only, went missing.
    let ana = root.open_landing("ana").expect("ana's landing");
    let empty = git(&ana, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let parent = git(&ana, &["commit-tree", &empty, "-m", "lost"]);
    let merged = git(&ana, &["commit-tree", &tree, "-p", &parent, "-m", "merge"]);
    git(
        &ana,
        &[
            "update-ref",
            &format!("refs/newgit/landing/{merged}"),
            &merged,
        ],
    );
    std::fs::remove_file(ana.join(format!("objects/{}/{}", &parent[..2], &parent[2..])))
        .expect("the parent goes");
    git(store.as_path(), &["update-ref", "-d", "refs/heads/only"]);

    let collected = collect(&app).expect("still collects");
    assert!(collected.stores >= 1);
    assert!(has(&store, &tree), "the tree only ana's merge reaches");
    assert!(has(&store, &only_blob), "and its content");
    assert!(!has(&store, &only), "the commit nothing reaches");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn landings_are_matched_to_stores_by_canonical_path() {
    let data = tempdir("gc-edges-canonical");
    let (app, root, commits) = server(&data, 2);
    let store = root.dir();
    let work = data.join("work-jack-p");
    let (base, base_blob) = commit_on(&work, &commits[1], "base");
    git(
        &work,
        &[
            "push",
            "-q",
            store.as_str(),
            &format!("{base}:refs/heads/base"),
        ],
    );
    let ana = root.open_landing("ana").expect("ana's landing");
    let tree = git(&ana, &["rev-parse", &format!("{base}^{{tree}}")]);
    let merged = git(&ana, &["commit-tree", &tree, "-p", &base, "-m", "merge"]);
    git(
        &ana,
        &[
            "update-ref",
            &format!("refs/newgit/landing/{merged}"),
            &merged,
        ],
    );
    git(store.as_path(), &["update-ref", "-d", "refs/heads/base"]);
    // The same store, spelled another way.
    let name = store.file_name().expect("named");
    let alternates = ana.join("objects/info/alternates");
    std::fs::write(
        &alternates,
        format!(
            "{}/../{}/{name}/objects\n",
            store.parent().expect("stores"),
            "stores"
        ),
    )
    .expect("alternates");

    collect(&app).expect("collects");
    assert!(has(&store, &base), "kept for ana's landing");
    assert!(has(&store, &base_blob));

    // A landing with a ref, borrowing from no store here: nothing is
    // collected, and the pass says why.
    std::fs::write(&alternates, format!("{}/gone.git/objects\n", data)).expect("alternates");
    let refused = collect(&app).expect_err("refused");
    let cause = refused.cause().to_owned();
    assert!(cause.contains("borrows from no store"), "{cause}");
    let _ = std::fs::remove_dir_all(&data);
}
