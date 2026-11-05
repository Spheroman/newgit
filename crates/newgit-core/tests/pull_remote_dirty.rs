//! A `newgit pull --remote` the project root cannot take (#142 review): an
//! uncommitted edit to a file the pull changes. Nothing moves — not the
//! tracker's `main`, not root's branch, not the project root — so after the
//! tree is cleaned the same pull brings in exactly what it would have.

mod imports;

use imports::*;
use newgit_core::manager::RemoteExportOptions;

#[test]
fn a_pull_into_a_dirty_tree_moves_nothing_and_runs_again_once_clean() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    let github = github_copy(&temp, &upstream, "t3code");
    let outcome = import(&store, &upstream);
    define_target(
        &store,
        "t3code",
        &format!("url = \"{github}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/app.ts", "theirs\n")],
        "t3: change app",
    );

    std::fs::write(repo.join("apps/t3code/src/app.ts"), "mine, uncommitted\n").expect("write");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let refs = |needle: &str| {
        refs_mentioning(&store, needle)
            .into_iter()
            .map(|name| {
                let commit = if name.starts_with("refs/trackers/") {
                    store_ref(&store, &name).unwrap_or_default()
                } else {
                    git(&repo, &["rev-parse", &name])
                };
                (name, commit)
            })
            .collect::<Vec<_>>()
    };
    let before = (refs("trackers/t3code"), refs("trackers/root"));

    let error = manager(&store).pull_remote("t3code").expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("uncommitted changes in the way?"),
        "{error}"
    );
    assert_eq!(
        (refs("trackers/t3code"), refs("trackers/root")),
        before,
        "no ref moved"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        store_ref(&store, "refs/trackers/t3code/main").as_deref(),
        Some(outcome.main.as_str())
    );
    assert_eq!(
        refs_mentioning(&store, "refs/newgit/pull/"),
        Vec::<String>::new()
    );

    git(&repo, &["checkout", "--", "apps/t3code/src/app.ts"]);
    let pulled = manager(&store).pull_remote("t3code").expect("pull");
    assert_eq!(pulled.commits, 2, "the real count");
    assert!(pulled.view.is_some());
    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/src/app.ts")).expect("read"),
        "theirs\n"
    );
}
