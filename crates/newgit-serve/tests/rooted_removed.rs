// A stored rooted history is derived data (#412's review, item 4): one for a
// tracker that is on no list any more (here, one left behind in the
// members' store by a tracker gone from every branch) is removed by the
// background pass, its served namespace and its state ref both, while the
// histories still wanted are kept.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use support::{landing, server, tempdir};

fn refs(dir: &Utf8Path) -> String {
    support::git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/namespaces/r/",
            "refs/newgit/rooted/",
        ],
    )
}

#[test]
fn a_history_no_list_names_is_removed() {
    let data = tempdir("rooted-removed");
    let (app, root, commits) = server(&data, 2);
    let app = Arc::new(app);
    let dir = root.dir();
    app.log
        .land(&app, landing(&app, "main", None, &commits[1]))
        .expect("lands");
    newgit_serve::rooted::start(&app);
    let started = Instant::now();
    while !refs(&dir).contains("refs/newgit/rooted/root") {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "root's is made"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // `gone`'s, as a tracker on no list leaves it.
    let state = support::git(&dir, &["rev-parse", "refs/newgit/rooted/root"]);
    let main = support::git(
        &dir,
        &[
            "rev-parse",
            "refs/namespaces/r/refs/namespaces/root/refs/heads/main",
        ],
    );
    support::git(&dir, &["update-ref", "refs/newgit/rooted/gone", &state]);
    support::git(
        &dir,
        &[
            "update-ref",
            "refs/namespaces/r/refs/namespaces/gone/refs/heads/main",
            &main,
        ],
    );
    support::git(
        &dir,
        &[
            "symbolic-ref",
            "refs/namespaces/r/refs/namespaces/gone/HEAD",
            "refs/namespaces/r/refs/namespaces/gone/refs/heads/main",
        ],
    );
    assert!(refs(&dir).contains("gone"));

    // A landing that touches everything there (the project's tags changed,
    // say) looks for histories nobody wants.
    let passes = app.rooted.passes.load(std::sync::atomic::Ordering::SeqCst);
    app.rooted
        .touched([(support::HOME.to_owned(), newgit_serve::rooted::Touched::All)].into());
    while app.rooted.passes.load(std::sync::atomic::Ordering::SeqCst) <= passes {
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
    }
    let left = refs(&dir);
    assert!(!left.contains("gone"), "{left}");
    assert!(left.contains("refs/newgit/rooted/root"), "{left}");
    let head = support::git(
        &dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/namespaces/r/refs/namespaces/gone/",
        ],
    );
    assert_eq!(head, "");
    let _ = std::fs::remove_dir_all(&data);
}
