// Garbage collection (#434) loses nothing anything still reaches: every ref
// in a store (any namespace), what a merge's landing reaches through the
// store it borrows from, and the landing log at every stamp. What nothing
// reaches goes, and so does a landing nothing waits in.

mod support;

use std::time::Duration;

use camino::Utf8Path;
use newgit_serve::maintenance;
use support::{HOME, git, landing, server, tempdir};

fn has(dir: &Utf8Path, object: &str) -> bool {
    std::process::Command::new("git")
        .args(["--git-dir", dir.as_str(), "cat-file", "-e", object])
        .status()
        .expect("git")
        .success()
}

fn fsck(dir: &Utf8Path) {
    let output = std::process::Command::new("git")
        .args([
            "--git-dir",
            dir.as_str(),
            "fsck",
            "--connectivity-only",
            "--no-dangling",
        ])
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "{dir} is whole: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn nothing_reachable_is_lost_and_nothing_else_stays() {
    let data = tempdir("gc-keeps-reachable");
    let (app, root, commits) = server(&data, 3);
    let store = root.dir();
    let work = data.join("work-jack-p");
    for (n, commit) in commits.iter().enumerate() {
        let base = n.checked_sub(1).map(|n| commits[n].as_str());
        app.log
            .land(&app, landing(&app, "main", base, commit))
            .expect("lands");
    }
    let tip = app.log.tip();

    // A fork's namespace naming a commit only it reaches.
    git(&work, &["checkout", "-q", "--detach", &commits[2]]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "fork only"]);
    let forked = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &[
            "push",
            "-q",
            store.as_str(),
            &format!(
                "{forked}:refs/namespaces/t/refs/namespaces/root/refs/namespaces/i/refs/namespaces/fix/refs/heads/main"
            ),
        ],
    );

    // A commit the store held, then dropped: nothing reaches it.
    git(&work, &["checkout", "-q", "--detach", &commits[2]]);
    std::fs::write(work.join("dropped"), "dropped\n").expect("write");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "dropped"]);
    let dropped = git(&work, &["rev-parse", "HEAD"]);
    let dropped_blob = git(&work, &["rev-parse", "HEAD:dropped"]);
    git(
        &work,
        &[
            "push",
            "-q",
            store.as_str(),
            &format!("{dropped}:refs/heads/gone"),
        ],
    );
    git(&work, &["update-ref", "-d", "refs/heads/gone"]);
    git(store.as_path(), &["update-ref", "-d", "refs/heads/gone"]);

    // ana's merge waits in her landing on `base`, a commit the store now
    git(&work, &["checkout", "-q", "--detach", &commits[2]]);
    // reaches through nothing of its own: its ref went after she sent.
    std::fs::write(work.join("base"), "base\n").expect("write");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
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
    let merged = git(
        &ana,
        &["commit-tree", &tree, "-p", &base, "-m", "ana's merge"],
    );
    git(
        &ana,
        &[
            "update-ref",
            &format!("refs/newgit/landing/{merged}"),
            &merged,
        ],
    );
    git(store.as_path(), &["update-ref", "-d", "refs/heads/base"]);

    // eve's landing waits for nothing.
    let eve = root.open_landing("eve").expect("eve's landing");

    // Opened lately, eve's stays.
    let kept =
        maintenance::collect_home_within(&app, HOME, Duration::from_secs(3600)).expect("collects");
    assert_eq!(kept.landings_removed, 0, "opened within the grace");
    assert!(eve.exists());

    let collected = maintenance::collect_home_within(&app, HOME, Duration::ZERO).expect("collects");
    assert_eq!(collected.landings_removed, 1, "{collected:?}");
    assert!(!eve.exists(), "eve's landing is gone");
    assert!(
        !data.join("homes/jack/p/landing/eve").exists(),
        "and her directory"
    );
    assert!(ana.exists(), "ana's waits for her merge");
    assert!(collected.stores >= 1);

    // Everything any ref reaches, in every namespace.
    fsck(&store);
    for commit in &commits {
        assert!(has(&store, commit));
    }
    assert!(has(&store, &forked), "the fork's");
    // What ana's landing reaches through the store.
    assert!(has(&store, &base), "kept for ana's landing");
    fsck(&ana);
    assert_eq!(git(&ana, &["rev-parse", &format!("{merged}^")]), base);
    // What nothing reaches, gone.
    assert!(!has(&store, &dropped), "the dropped commit");
    assert!(!has(&store, &dropped_blob), "and its content");

    // The landing log: collected, nothing dropped.
    let log = data.join("landing.git");
    let loose = |dir: &Utf8Path| {
        let counted = git(dir, &["count-objects", "-v"]);
        let field = |name: &str| -> u64 {
            counted
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0)
        };
        (field("count:"), field("packs:"))
    };
    let (before, _) = loose(&log);
    assert!(before > 0, "small batches write loose objects");
    app.log.collect().expect("the log collects");
    assert_eq!(loose(&log), (0, 1), "rolled up into one pack");
    fsck(&log);
    assert_eq!(
        git(&log, &["rev-list", "--count", "refs/heads/main"]),
        tip.to_string()
    );
    let incarnation = support::incarnation(&app, HOME);
    for (n, commit) in commits.iter().enumerate() {
        let stamp = tip - (commits.len() - 1 - n) as u64;
        let list = app
            .log
            .list_at(HOME, &incarnation, "main", stamp, false)
            .expect("reads")
            .expect("main then");
        assert_eq!(&list["root"], commit, "the state at {stamp}");
    }

    // A whole pass says what it did.
    let pass = maintenance::run(&app);
    assert!(pass.failed.is_empty(), "{:?}", pass.failed);
    assert_eq!(
        app.maintenance.last().map(|last| last.stores),
        Some(pass.stores)
    );
    let _ = std::fs::remove_dir_all(&data);
}
