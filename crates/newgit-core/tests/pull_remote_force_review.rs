//! `newgit pull --remote <target> --force` (#442), the review's findings:
//! what a reset drops stays out of every target that lacks it, the project's
//! own view included, whose older root commits still pin the old line; a
//! rewritten target is never merged with or exported over by a plain command,
//! which names `--force` instead; `export --force` says what it puts back; a
//! second reset back onto dropped commits takes them off the record; the
//! project root's own work stops the reset; and a reset that moved nothing
//! leaves no record.

mod imports;

use camino::Utf8Path;
use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::source::RefUpdate;
use newgit_core::store::MetadataStore;

/// `t3code` imported at `apps/t3code`, published, exported to `github` (a
/// bare copy of its upstream, branch `newgit`) rooted at it, with `extra`
/// keys on the target.
fn exported_with(temp: &Utf8Path, extra: &str) -> (MetadataStore, camino::Utf8PathBuf) {
    let upstream = upstream(temp);
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    import(&store, &upstream);
    let keys = if extra.is_empty() {
        "branch = \"newgit\"".to_owned()
    } else {
        format!("branch = \"newgit\", {extra}")
    };
    set_export(&store, "t3code", Some(&export_to(&github, "t3code", &keys)));
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (store, github)
}

fn exported(temp: &Utf8Path) -> (MetadataStore, camino::Utf8PathBuf) {
    exported_with(temp, "")
}

fn main_of(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

/// `github`'s `newgit` rewritten elsewhere: reset to `onto`, one commit of
/// `files` on it, force-pushed. Returns the new tip.
fn force_push(
    temp: &Utf8Path,
    dir: &str,
    github: &Utf8Path,
    onto: &str,
    files: &[(&str, &str)],
    message: &str,
) -> String {
    let clone = temp.join(dir);
    git(
        temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            clone.as_str(),
        ],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(&clone, &["fetch", "-q", "origin"]);
    git(&clone, &["reset", "-q", "--hard", onto]);
    commit_as(&clone, "pat", 2_100_000_000, files, message);
    git(&clone, &["push", "-q", "--force", "origin", "newgit"]);
    git(&clone, &["rev-parse", "HEAD"])
}

/// A key lands in t3code through `feature` and is exported. Returns (the
/// commit before it, the commit adding it).
fn leak(store: &MetadataStore) -> (String, String) {
    let clean = main_of(store, "t3code");
    let feature = manager(store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &feature,
        "ana",
        1_800_000_000,
        &[(
            "apps/t3code/src/key.ts",
            "export const KEY = \"sk-live\";\n",
        )],
        "t3code: add the key",
    );
    manager(store).merge("feature", "main").expect("merge");
    let leaked = main_of(store, "t3code");
    manager(store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (clean, leaked)
}

/// Leak a key, export it, and purge it on the target. Returns (clean,
/// leaked, purged).
fn leak_and_purge(
    temp: &Utf8Path,
    store: &MetadataStore,
    github: &Utf8Path,
) -> (String, String, String) {
    let (clean, leaked) = leak(store);
    let purged = force_push(
        temp,
        "purge",
        github,
        &clean,
        &[("src/key.ts", "rotated\n")],
        "rotate the key",
    );
    (clean, leaked, purged)
}

fn on_target(github: &Utf8Path, commit: &str) -> bool {
    git_ok(github, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        && git_ok(
            github,
            &["merge-base", "--is-ancestor", commit, "refs/heads/newgit"],
        )
}

fn resets_file(store: &MetadataStore) -> camino::Utf8PathBuf {
    store.paths().local.join("resets.json")
}

#[test]
fn plain_pull_and_export_refuse_a_rewritten_target_and_name_the_reset() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (clean, leaked) = leak(&store);
    // A purge that merges cleanly with the old line: plain `pull --remote`
    // used to merge it, and the next export put the key back.
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/unrelated.ts", "x\n")],
        "unrelated",
    );

    let pulled = manager(&store)
        .pull_remote("t3code")
        .expect_err("a rewritten target is not merged with");
    let said = pulled.to_string();
    assert!(said.contains("was rewritten"), "{said}");
    assert!(
        said.contains("1 commit that export put there is gone"),
        "{said}"
    );
    assert!(
        said.contains("newgit pull --remote t3code --force --dry-run"),
        "{said}"
    );
    assert!(!said.contains("export --remote t3code --force"), "{said}");
    assert_eq!(main_of(&store, "t3code"), leaked, "nothing moved");

    let export = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("nor exported over");
    let said = export.to_string();
    assert!(said.contains("was rewritten"), "{said}");
    assert!(
        said.contains("newgit pull --remote t3code --force --dry-run"),
        "{said}"
    );
    assert!(!said.contains("`--force` replaces"), "{said}");
    assert!(!on_target(&github, &leaked));

    // Followed, both work again, and the key stays off the target.
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    let again = manager(&store).pull_remote("t3code").expect("pull");
    assert_eq!(again.commits, 0, "{again:?}");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert!(!on_target(&github, &leaked));
}

#[test]
fn export_force_over_a_rewritten_target_says_what_it_puts_back() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (_clean, leaked, _purged) = leak_and_purge(&temp, &store, &github);
    let dry = manager(&store)
        .export_to_remote(
            "t3code",
            RemoteExportOptions {
                force: true,
                dry_run: true,
                ..RemoteExportOptions::default()
            },
        )
        .expect("dry run");
    assert_eq!(dry.restored, 1, "{dry:?}");
    assert!(!on_target(&github, &leaked));
    let forced = manager(&store)
        .export_to_remote(
            "t3code",
            RemoteExportOptions {
                force: true,
                ..RemoteExportOptions::default()
            },
        )
        .expect("export --force");
    assert_eq!(forced.restored, 1, "{forced:?}");
    assert!(on_target(&github, &leaked), "asked for, and said");
}

#[test]
fn a_second_reset_back_onto_dropped_commits_takes_them_off_the_record() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (_clean, leaked, purged) = leak_and_purge(&temp, &store, &github);
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert_eq!(main_of(&store, "t3code"), purged);

    // The purge is undone on the target: back to the old line, one more
    // commit on it.
    let restored = force_push(
        &temp,
        "restore",
        &github,
        &leaked,
        &[("src/after.ts", "after\n")],
        "after the restore",
    );
    let dry = manager(&store)
        .reset_to_remote("t3code", true)
        .expect("dry run");
    let [line] = dry.lines.as_slice() else {
        panic!("one line: {dry:?}");
    };
    let back: Vec<&str> = line.restored.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(back, [leaked.as_str()], "the dry run says so: {dry:?}");
    let reset = manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    assert_eq!(reset.lines[0].restored.len(), 1, "{reset:?}");
    assert_eq!(main_of(&store, "t3code"), restored);
    let record = std::fs::read_to_string(resets_file(&store)).expect("record");
    assert!(!record.contains(&leaked), "{record}");

    // Nothing is wedged: the reset publishes, exports, and later work merges.
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    let later = manager(&store)
        .spawn("later", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &later,
        "ana",
        2_200_000_000,
        &[("README.md", "later\n")],
        "root: later",
    );
    manager(&store).merge("later", "main").expect("merge later");
}

#[test]
fn the_project_roots_own_work_on_the_old_line_stops_the_reset() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (_clean, leaked, _purged) = leak_and_purge(&temp, &store, &github);
    commit_as(
        &repo,
        "ana",
        1_950_000_000,
        &[("apps/t3code/src/rootwork.ts", "root work\n")],
        "t3code: work in the project root",
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let dry = manager(&store)
        .reset_to_remote("t3code", true)
        .expect("dry run");
    let root: Vec<(usize, usize)> = dry
        .instances
        .iter()
        .filter(|held| held.project_root)
        .map(|held| (held.dropped, held.own))
        .collect();
    assert_eq!(root, [(1, 1)], "{dry:?}");

    let refused = manager(&store)
        .reset_to_remote("t3code", false)
        .expect_err("the project root's work would be discarded");
    let said = refused.to_string();
    assert!(
        said.contains("the project root has a commit of its own"),
        "{said}"
    );
    assert!(said.contains("t3code: work in the project root"), "{said}");
    assert!(repo.join("apps/t3code/src/rootwork.ts").exists());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(main_of(&store, "t3code"), leaked);
    assert!(!resets_file(&store).exists(), "nothing written down");
}

#[test]
fn a_reset_that_moved_nothing_leaves_no_record() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (_clean, leaked, _purged) = leak_and_purge(&temp, &store, &github);
    std::fs::write(repo.join("apps/t3code/src/key.ts"), "local edit\n").expect("write");
    let refused = manager(&store)
        .reset_to_remote("t3code", false)
        .expect_err("the project root cannot take it");
    assert!(
        refused.to_string().contains("uncommitted changes"),
        "{refused}"
    );
    assert_eq!(main_of(&store, "t3code"), leaked);
    assert!(!resets_file(&store).exists(), "a reset that never happened");

    // So unrelated work still merges, with no word about a reset.
    let later = manager(&store)
        .spawn("later", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &later,
        "ana",
        2_200_000_000,
        &[("README.md", "later\n")],
        "root: later",
    );
    manager(&store).merge("later", "main").expect("merge later");
}

#[test]
fn the_projects_own_view_is_refused_while_its_history_pins_a_dropped_commit() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (_clean, leaked, _purged) = leak_and_purge(&temp, &store, &github);
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");

    // A new target for the project's own view, which t3code ships in too:
    // root's older commits still pin the key's commit.
    let mirror = temp.join("mirror.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    set_export(&store, "root", Some(&export_to(&mirror, "mirror", "")));
    set_export(
        &store,
        "t3code",
        Some(&format!(
            "[\"inherit\", {}]",
            export_to(&github, "t3code", "branch = \"newgit\"")
        )),
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "mirror target"]);
    manager(&store).merge("main", "main").expect("merge main");

    let refused = manager(&store)
        .export_to_remote("mirror", RemoteExportOptions::default())
        .expect_err("the project's history pins the dropped commit");
    let said = refused.to_string();
    assert!(said.contains(&leaked[..8]), "{said}");
    assert!(said.contains("dropped"), "{said}");
    assert!(said.contains("project's own history"), "{said}");
    assert_eq!(git(&mirror, &["for-each-ref"]), "", "nothing was pushed");
}

#[test]
fn an_instance_export_holding_a_dropped_commit_is_refused() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported_with(&temp, "branches = true");
    let repo = store.paths().project_root.clone();
    // A plain-Git project: a push from a workspace goes to the store's
    // `origin`, then its branch to each `branches = true` target.
    let origin = temp.join("origin.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", origin.as_str()],
    );
    git(&repo, &["remote", "add", "origin", origin.as_str()]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let (_clean, leaked, _purged) = leak_and_purge(&temp, &store, &github);
    // An instance on the old line, with work of its own on it.
    let worker = manager(&store).spawn("worker", None).expect("spawn").branch;
    commit_as(
        &worker.workspace_path,
        "ana",
        1_900_000_000,
        &[("apps/t3code/src/more.ts", "more\n")],
        "t3code: more",
    );
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");

    let head = git(&worker.workspace_path, &["rev-parse", "HEAD"]);
    let pushed = manager(&store)
        .receive_push(
            "worker",
            &[RefUpdate {
                old: None,
                new: Some(head),
                name: format!("refs/heads/{}", worker.source_ref),
            }],
            None,
        )
        .expect("the push to `origin` itself");
    let [export] = pushed.exports.as_slice() else {
        panic!("one export: {:?}", pushed.exports);
    };
    let said = export
        .result
        .as_ref()
        .expect_err("the branch holds the dropped commit");
    assert!(said.contains(&leaked[..8]), "{said}");
    assert!(said.contains("dropped"), "{said}");
    assert!(
        !git_ok(
            &github,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", worker.source_ref)
            ]
        ),
        "nothing reached the target"
    );
}
