//! `newgit pull --remote <target> --force` (#442), the second re-check:
//! every push to an export target goes through one gate, so a rewrite that
//! purged a commit there is not undone by an instance's branch export, by a
//! branch the target deleted, or by `--republish` without `--force`. A stale
//! record (another machine exported after this one) cannot be told from
//! history of the target's own here (#473): the refusal says so and names
//! the dry run. Normal work after a reset, and a pull request merged with no
//! rewrite at all, still export.

mod imports;

use camino::Utf8Path;
use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

use newgit_core::source::RefUpdate;

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

const PUSHED: &str = "refs/newgit/exports/t3code/pushed";

/// Does any ref on `github` reach `commit`?
fn anywhere_on(github: &Utf8Path, commit: &str) -> Vec<String> {
    if !git_ok(github, &["cat-file", "-e", &format!("{commit}^{{commit}}")]) {
        return Vec::new();
    }
    git(
        github,
        &["for-each-ref", "--contains", commit, "--format=%(refname)"],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Rewind-only purge of the target's branch; nothing followed yet. An
/// agent's push from an instance spawned off `main` (which still holds the
/// key) exports its branch to a `branches = true` target.
#[test]
fn instance_export_after_a_rewind_does_not_republish() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported_with(&temp, "branches = true");
    let repo = store.paths().project_root.clone();
    let origin = temp.join("origin.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", origin.as_str()],
    );
    git(&repo, &["remote", "add", "origin", origin.as_str()]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let (clean, leaked) = leak(&store);
    rewind(&github, &clean);
    assert!(anywhere_on(&github, &leaked).is_empty(), "purged");

    let worker = manager(&store).spawn("worker", None).expect("spawn").branch;
    commit_as(
        &worker.workspace_path,
        "ana",
        1_900_000_000,
        &[("apps/t3code/src/more.ts", "more\n")],
        "t3code: more",
    );
    // A real `git push` brings the objects to the store; here a checkpoint does.
    manager(&store)
        .checkpoint("worker", None)
        .expect("checkpoint");
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
        .expect("push");
    let refs = anywhere_on(&github, &leaked);
    assert!(
        refs.is_empty(),
        "the purged commit is back on the target at {refs:?}"
    );
    let said = pushed
        .exports
        .iter()
        .find(|export| export.target == "t3code")
        .expect("exported to t3code")
        .result
        .as_ref()
        .expect_err("refused");
    assert!(said.contains("was rewritten"), "{said}");
    assert!(said.contains("would put them back"), "{said}");
}

/// The limit (#473): this machine's record is stale, not missing: it exported `clean`,
/// another machine exported the key (simulated as the PR's own test does,
/// by moving `pushed` back), the key was purged by a rewind. `pushed` ==
/// the target's tip, so nothing looks rewritten.
#[test]
#[ignore = "#473: a record older than another machine's export cannot be told from one that is current"]
fn stale_record_rewind() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (clean, leaked) = leak(&store);
    git(&repo, &["update-ref", PUSHED, &clean]);
    rewind(&github, &clean);
    let result = manager(&store).export_to_remote("t3code", RemoteExportOptions::default());
    let _ = result;
    assert!(
        !on_target(&github, &leaked),
        "republished with a stale record"
    );
}

/// A stale record, and the target rewritten then moved on past what this
/// machine pushed (`pushed` is an ancestor of its tip): it looks like a
/// pull request merged there (#473), so the refused push names the dry run
/// that would show the rewrite before anything is merged in.
#[test]
fn stale_record_refusal_names_the_dry_run() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let (clean, _leaked) = leak(&store);
    git(&repo, &["update-ref", PUSHED, &clean]);
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/unrelated.ts", "unrelated\n")],
        "unrelated work",
    );
    let said = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("diverged")
        .to_string();
    assert!(said.contains("has commits of its own"), "{said}");
    assert!(said.contains("another machine exported"), "{said}");
    assert!(
        said.contains("newgit pull --remote t3code --force --dry-run"),
        "{said}"
    );
}

/// A purge by deleting the branch on the target (or deleting and
/// recreating the repository), with a record of exporting there: a rewrite
/// that removed everything. Refused plainly, refused with `--force`, said
/// by a dry run, and put back only with `--force --republish`, counted.
#[test]
fn branch_deleted_on_the_target_is_not_republished() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (_clean, leaked) = leak(&store);
    git(&github, &["update-ref", "-d", "refs/heads/newgit"]);

    let said = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("refused")
        .to_string();
    assert!(said.contains("is gone"), "{said}");
    assert!(said.contains("--force --republish"), "{said}");
    assert!(
        anywhere_on(&github, &leaked).is_empty(),
        "republished to a deleted branch"
    );

    // A push exports only what is new, so something new first.
    work(
        &store,
        "more",
        1_900_000_000,
        &[("apps/t3code/src/more.ts", "more\n")],
        "t3code: more",
    );
    let said = manager(&store)
        .export_to_remote("t3code", from_push())
        .expect_err("refused")
        .to_string();
    assert!(said.contains("is gone"), "{said}");
    assert!(anywhere_on(&github, &leaked).is_empty());

    let said = manager(&store)
        .export_to_remote("t3code", forced(false))
        .expect_err("refused")
        .to_string();
    assert!(said.contains("would put back"), "{said}");
    assert!(anywhere_on(&github, &leaked).is_empty());

    let dry = manager(&store)
        .export_to_remote(
            "t3code",
            RemoteExportOptions {
                force: true,
                dry_run: true,
                ..RemoteExportOptions::default()
            },
        )
        .expect("a dry run says what --force would put back");
    assert!(!dry.pushed && dry.restored > 0, "{dry:?}");
    assert!(anywhere_on(&github, &leaked).is_empty());

    let out = manager(&store)
        .export_to_remote("t3code", forced(true))
        .expect("--force --republish makes it again");
    assert!(out.pushed && out.restored == dry.restored, "{out:?}");
    assert!(on_target(&github, &leaked));
}

/// Normal work after a reset: two targets, purge followed, the other
/// target purged with `export --force`; then a pull request merges on
/// `github`, is pulled, merged, and exported to both.
#[test]
fn normal_work_after_a_reset_still_exports() {
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
    force_push(
        &temp,
        "purge",
        &github,
        &clean,
        &[("src/key.ts", "rotated\n")],
        "rotate the key",
    );
    manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export github after reset");
    let said = manager(&store)
        .export_to_remote("other", RemoteExportOptions::default())
        .expect_err("other is on the old line")
        .to_string();
    assert!(said.contains("--force"), "{said}");
    let forced_other = manager(&store).export_to_remote("other", forced(false));
    forced_other.expect("--force purges the second target");
    assert!(!on_target(&other, &leaked));

    // Ordinary life afterwards.
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "a pull request",
    );
    manager(&store)
        .pull_remote("t3code")
        .expect("plain pull --remote after an ordinary merge");
    manager(&store).merge("main", "main").expect("merge main");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export github");
    manager(&store)
        .export_to_remote("other", RemoteExportOptions::default())
        .expect("export other carries the PR");
    work(
        &store,
        "after",
        2_050_000_000,
        &[("apps/t3code/src/after.ts", "after\n")],
        "t3code: after",
    );
    manager(&store)
        .export_to_remote("t3code", from_push())
        .expect("push export");
    manager(&store)
        .export_to_remote("other", from_push())
        .expect("push export other");
}

/// No reset, no rewrite: a pull request merged on the target, plain
/// export brings it in (or names the pull), plain pull works, export works;
/// and one merged while `main` also moved.
#[test]
fn merged_pr_without_any_rewrite_still_exports() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "a pull request",
    );
    work(
        &store,
        "mine",
        1_800_000_000,
        &[("apps/t3code/src/mine.ts", "mine\n")],
        "t3code: mine",
    );
    let said = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("diverged")
        .to_string();
    assert!(!said.contains("rewritten"), "{said}");
    manager(&store).pull_remote("t3code").expect("pull");
    manager(&store).merge("main", "main").expect("merge main");
    let out = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert!(out.pushed && out.restored == 0);
    // --force with nothing rewritten needs no --republish.
    work(
        &store,
        "more",
        1_900_000_000,
        &[("apps/t3code/src/more.ts", "more\n")],
        "t3code: more",
    );
    let out = manager(&store)
        .export_to_remote("t3code", forced(false))
        .expect("force, nothing rewritten");
    assert!(out.pushed && out.restored == 0);
}

/// `republish` without `force` (the CLI forbids it; the core is asked
/// directly) does not put anything back.
#[test]
fn republish_without_force_is_inert() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let (clean, leaked) = leak(&store);
    rewind(&github, &clean);
    for options in [
        RemoteExportOptions {
            republish: true,
            ..RemoteExportOptions::default()
        },
        RemoteExportOptions {
            republish: true,
            from_push: true,
            ..RemoteExportOptions::default()
        },
    ] {
        let _ = manager(&store).export_to_remote("t3code", options);
        assert!(!on_target(&github, &leaked), "republish alone put it back");
    }
}
