//! `newgit pull --remote <target> --force` (#442), the re-check's findings:
//! a purge that only rewinds the target (`git reset --hard HEAD~1 && git
//! push --force`, no new commit) is refused by every export, not
//! fast-forwarded over; a target that is merely behind still exports; a
//! second target still on the old line is not pulled back in; with no
//! record of exporting there, the refused push names the dry run first;
//! `export --force` says what it would put back before pushing, and pushes
//! it only with `--republish`; the project's own view says what to do; and
//! a dry run of the reset writes only what the docs say it does.

mod imports;

use std::collections::BTreeSet;

use camino::Utf8Path;
use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

/// `t3code` imported at `apps/t3code`, published, and exported to `github`
/// (a bare copy of its upstream, branch `newgit`) rooted at it.
fn exported(temp: &Utf8Path) -> (MetadataStore, camino::Utf8PathBuf) {
    let upstream = upstream(temp);
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&github, "t3code", "branch = \"newgit\"")),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (store, github)
}

fn main_of(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

/// Merge one commit of `files` from a new instance `name` into `main`.
fn work(store: &MetadataStore, name: &str, at: i64, files: &[(&str, &str)], message: &str) {
    let workspace = manager(store)
        .spawn(name, None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(&workspace, "ana", at, files, message);
    manager(store).merge(name, "main").expect("merge");
}

/// A key merged into `t3code` and exported. Returns (clean, leaked):
/// `t3code`'s `main` before and after.
fn leak(store: &MetadataStore) -> (String, String) {
    let clean = main_of(store, "t3code");
    work(
        store,
        "feature",
        1_800_000_000,
        &[(
            "apps/t3code/src/key.ts",
            "export const KEY = \"sk-live\";\n",
        )],
        "t3code: add the key",
    );
    let leaked = main_of(store, "t3code");
    manager(store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (clean, leaked)
}

/// The simplest purge there is: drop the commit and force-push, adding
/// nothing.
fn rewind(github: &Utf8Path, to: &str) {
    git(github, &["update-ref", "refs/heads/newgit", to]);
}

/// `github`'s `newgit` rewritten elsewhere: reset to `onto`, one commit of
/// `files` on it, force-pushed.
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

fn on_target(github: &Utf8Path, commit: &str) -> bool {
    git_ok(github, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        && git_ok(
            github,
            &["merge-base", "--is-ancestor", commit, "refs/heads/newgit"],
        )
}

fn tip_of(github: &Utf8Path) -> String {
    git(github, &["rev-parse", "refs/heads/newgit"])
}

fn dry_run() -> RemoteExportOptions {
    RemoteExportOptions {
        dry_run: true,
        ..RemoteExportOptions::default()
    }
}

fn from_push() -> RemoteExportOptions {
    RemoteExportOptions {
        from_push: true,
        ..RemoteExportOptions::default()
    }
}

fn forced(republish: bool) -> RemoteExportOptions {
    RemoteExportOptions {
        force: true,
        republish,
        ..RemoteExportOptions::default()
    }
}

#[test]
fn a_rewind_only_purge_is_refused_by_plain_export_and_its_dry_run() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (clean, leaked) = leak(&store);
    rewind(&github, &clean);
    assert!(!on_target(&github, &leaked), "purged");

    for (label, options) in [
        ("dry run", dry_run()),
        ("plain", RemoteExportOptions::default()),
    ] {
        let said = manager(&store)
            .export_to_remote("t3code", options)
            .expect_err(label)
            .to_string();
        assert!(said.contains("was rewritten"), "{label}: {said}");
        assert!(
            said.contains("newgit pull --remote t3code --force --dry-run"),
            "{label}: {said}"
        );
        assert!(!on_target(&github, &leaked), "{label}: not put back");
    }

    // Followed, the export works again, and the key stays off.
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert!(!on_target(&github, &leaked));
}

#[test]
fn a_rewind_only_purge_is_refused_by_the_export_a_push_makes() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (clean, leaked) = leak(&store);
    // Later work, so the push has something new for the target.
    work(
        &store,
        "later",
        1_900_000_000,
        &[("apps/t3code/src/later.ts", "later\n")],
        "t3code: later",
    );
    rewind(&github, &clean);

    let said = manager(&store)
        .export_to_remote("t3code", from_push())
        .expect_err("a push does not put the key back")
        .to_string();
    assert!(said.contains("was rewritten"), "{said}");
    assert!(said.contains("This push would put them back"), "{said}");
    assert!(!on_target(&github, &leaked));
    assert_eq!(tip_of(&github), clean, "nothing was pushed");
}

#[test]
fn a_target_merely_behind_still_exports() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let pushed_ref = "refs/newgit/exports/t3code/pushed";

    // Behind by new work: the target is at what was pushed.
    work(
        &store,
        "one",
        1_800_000_000,
        &[("apps/t3code/src/one.ts", "one\n")],
        "t3code: one",
    );
    let plain = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("plain export");
    assert!(plain.pushed && plain.restored == 0, "{plain:?}");
    assert_eq!(tip_of(&github), plain.tip);

    // Behind, and ahead of what this machine pushed last (another machine
    // exported since): the record is an ancestor of the target's tip.
    let older = git(&repo, &["rev-parse", &format!("{pushed_ref}~1")]);
    git(&repo, &["update-ref", pushed_ref, &older]);
    work(
        &store,
        "two",
        1_900_000_000,
        &[("apps/t3code/src/two.ts", "two\n")],
        "t3code: two",
    );
    let pushed = manager(&store)
        .export_to_remote("t3code", from_push())
        .expect("the export a push makes");
    assert!(pushed.pushed && pushed.restored == 0, "{pushed:?}");
    assert_eq!(tip_of(&github), pushed.tip);

    work(
        &store,
        "three",
        2_000_000_000,
        &[("apps/t3code/src/three.ts", "three\n")],
        "t3code: three",
    );
    manager(&store)
        .export_to_remote("t3code", dry_run())
        .expect("a dry run");
    let again = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("plain export");
    assert_eq!(tip_of(&github), again.tip);
}

#[test]
fn export_force_says_what_it_would_put_back_before_pushing() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (clean, leaked) = leak(&store);
    rewind(&github, &clean);

    let dry = manager(&store)
        .export_to_remote(
            "t3code",
            RemoteExportOptions {
                force: true,
                dry_run: true,
                ..RemoteExportOptions::default()
            },
        )
        .expect("a dry run says it");
    assert_eq!(dry.restored, 1, "{dry:?}");

    let said = manager(&store)
        .export_to_remote("t3code", forced(false))
        .expect_err("--force alone does not put the key back")
        .to_string();
    assert!(said.contains("put back 1 commit"), "{said}");
    assert!(said.contains("--force --republish"), "{said}");
    assert!(
        said.contains("newgit pull --remote t3code --force --dry-run"),
        "{said}"
    );
    assert_eq!(tip_of(&github), clean, "nothing was pushed");

    let put_back = manager(&store)
        .export_to_remote("t3code", forced(true))
        .expect("--force --republish");
    assert_eq!(put_back.restored, 1, "{put_back:?}");
    assert!(on_target(&github, &leaked), "asked for by name");
}

#[test]
fn a_second_target_still_on_the_old_line_is_not_pulled_back_in() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let other = temp.join("other.git");
    git(
        &temp,
        &["clone", "-q", "--bare", github.as_str(), other.as_str()],
    );
    set_export(
        &store,
        "t3code",
        Some(&format!(
            "[{}, {}]",
            export_to(&github, "t3code", "branch = \"newgit\""),
            export_to(&other, "other", "branch = \"newgit\"")
        )),
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "second target"]);
    manager(&store).merge("main", "main").expect("merge main");
    let (clean, leaked) = leak(&store);
    manager(&store)
        .export_to_remote("other", RemoteExportOptions::default())
        .expect("export other");
    // Purged on `github` only, and followed here.
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/unrelated.ts", "x\n")],
        "unrelated",
    );
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    let reset = main_of(&store, "t3code");

    let said = manager(&store)
        .pull_remote("other")
        .expect_err("`other` still has the dropped commit")
        .to_string();
    assert!(said.contains("`other` still has a commit"), "{said}");
    assert!(said.contains(&leaked[..8]), "{said}");
    assert!(
        said.contains("newgit pull --remote other --force"),
        "{said}"
    );
    assert!(said.contains("purge them from `other`"), "{said}");
    assert_eq!(main_of(&store, "t3code"), reset, "nothing moved");

    // Its plain export names the way out, and `--force` purges it there.
    let said = manager(&store)
        .export_to_remote("other", RemoteExportOptions::default())
        .expect_err("the push is not a fast-forward")
        .to_string();
    assert!(said.contains("a reset here dropped"), "{said}");
    assert!(
        said.contains("newgit export --remote other --force"),
        "{said}"
    );
    manager(&store)
        .export_to_remote("other", forced(false))
        .expect("replacing `other` puts back nothing it removed");
    assert!(
        !git_ok(
            &other,
            &["merge-base", "--is-ancestor", &leaked, "refs/heads/newgit"]
        ),
        "purged there too"
    );
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export github");
    assert!(!on_target(&github, &leaked));
}

#[test]
fn with_no_record_the_refused_push_names_the_dry_run_first() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (clean, _leaked) = leak(&store);
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/unrelated.ts", "x\n")],
        "unrelated",
    );
    // Another machine, or a lost ref: no record of exporting there.
    git(
        &repo,
        &["update-ref", "-d", "refs/newgit/exports/t3code/pushed"],
    );

    let said = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("the push is not a fast-forward")
        .to_string();
    let dry = said
        .find("newgit pull --remote t3code --force --dry-run")
        .unwrap_or_else(|| panic!("names the dry run: {said}"));
    let force = said
        .find("`--force` replaces the branch")
        .unwrap_or_else(|| panic!("names --force: {said}"));
    assert!(dry < force, "the dry run first: {said}");
    assert!(said.contains("no record of exporting"), "{said}");
    assert!(said.contains("1 commit the export has"), "{said}");
}

#[test]
fn a_rewritten_project_view_says_what_to_do() {
    let (_guard, temp) = tempdir();
    let (store, _github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let mirror = temp.join("mirror.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    set_export(&store, "root", Some(&export_to(&mirror, "mirror", "")));
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "mirror target"]);
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("mirror", RemoteExportOptions::default())
        .expect("export mirror");
    let before = git(&mirror, &["rev-parse", "refs/heads/main"]);
    work(
        &store,
        "feature",
        1_800_000_000,
        &[("src/key.ts", "export const KEY = \"sk-live\";\n")],
        "root: add the key",
    );
    manager(&store)
        .export_to_remote("mirror", RemoteExportOptions::default())
        .expect("export mirror");
    git(&mirror, &["update-ref", "refs/heads/main", &before]);

    let said = manager(&store)
        .export_to_remote("mirror", RemoteExportOptions::default())
        .expect_err("rewound")
        .to_string();
    assert!(said.contains("was rewritten"), "{said}");
    assert!(said.contains("project's own view"), "{said}");
    assert!(
        said.contains("newgit export --remote mirror --force --republish"),
        "{said}"
    );
    assert!(said.contains("putting back"), "{said}");
    assert!(said.contains("rotate it"), "{said}");
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"]),
        before,
        "nothing was pushed"
    );
}

/// What a dry run of the reset writes, exactly as the docs say: the
/// target's branch fetched, and each instance's workspace read as a pull
/// reads it. No tracker's `main`, no pin, no record.
#[test]
fn a_dry_run_of_the_reset_writes_only_what_it_says() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (clean, _leaked) = leak(&store);
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/key.ts", "rotated\n")],
        "rotate the key",
    );
    let refs = || -> BTreeSet<String> {
        git(
            &repo,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
        .lines()
        .map(ToOwned::to_owned)
        .collect()
    };
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let before = refs();
    let outcome = manager(&store)
        .reset_to_remote("t3code", true)
        .expect("dry run");
    assert!(outcome.dry_run);
    let after = refs();
    for changed in after.symmetric_difference(&before) {
        let name = changed.split(' ').next().unwrap_or_default();
        assert!(
            name == "refs/newgit/exports/t3code/upstream"
                || name.starts_with("refs/newgit/pull/")
                || (name.starts_with("refs/trackers/") && name.contains("/instances/")),
            "a dry run wrote {name}"
        );
    }
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert!(!store.paths().local.join("resets.json").exists());
}
