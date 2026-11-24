//! `newgit pull` after the #426 review: an instance named where a tracker
//! goes says so, a target rooted at a child names the pull that takes it, a
//! file of a tracker outside Git on a target is never taken in whichever way
//! that tracker's paths are written, two instances pulling the same two tips
//! make one merge, the unbound and skipped children are listed only when
//! they matter, and export after a pull in the project root says to merge.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

mod imports;

use camino::{Utf8Path, Utf8PathBuf};
use imports::*;
use newgit_core::manager::{BindingChoice, RemoteExportOptions};
use newgit_core::store::MetadataStore;

/// `t3code` imported at `apps/t3code`, holding `.repos/`, published, and
/// exported to a bare copy of its upstream (branch `newgit`) rooted at it,
/// or to `gone.git`, which does not exist, when `reachable` is false.
/// Returns the store and the target.
fn exported(temp: &Utf8Path, reachable: bool) -> (MetadataStore, Utf8PathBuf) {
    let upstream = upstream(temp);
    commit_as(
        &upstream,
        "ana",
        1_690_000_000,
        &[(".repos/effect/LLMS.md", "effect\n")],
        "Vendor effect",
    );
    let store = project(temp);
    let github = if reachable {
        github_copy(temp, &upstream, "t3code")
    } else {
        temp.join("gone.git")
    };
    import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&github, "t3code", "branch = \"newgit\"")),
    );
    commit_all(&store, "export target");
    manager(&store).merge("main", "main").expect("publish");
    if reachable {
        manager(&store)
            .export_to_remote("t3code", RemoteExportOptions::default())
            .expect("export");
    }
    (store, github)
}

fn commit_all(store: &MetadataStore, message: &str) {
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", message]);
}

/// `apps/t3code/.repos` carved out as `t3code`'s child `repos`.
fn carve_repos(store: &MetadataStore) {
    manager(store)
        .create_git_tracker(
            "repos",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/.repos")],
        )
        .expect("carve");
    manager(store).merge("main", "main").expect("publish");
}

/// Elsewhere: one commit adding `file` on `tracker`'s `main`, alone.
/// Returns the commit `main` is at now.
fn advance(temp: &Utf8Path, store: &MetadataStore, tracker: &str, file: &str) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    let elsewhere = temp.join(format!("elsewhere-{tracker}-{}", file.replace('/', "-")));
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-q", "-b", "main"]);
    git(&elsewhere, &["config", "commit.gpgSign", "false"]);
    git(
        &elsewhere,
        &[
            "fetch",
            "-q",
            tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/remotes/store/main"),
        ],
    );
    git(
        &elsewhere,
        &["reset", "-q", "--hard", "refs/remotes/store/main"],
    );
    let path = elsewhere.join(file);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "x\n").expect("write");
    git(&elsewhere, &["add", "-A"]);
    git(
        &elsewhere,
        &["commit", "-q", "-m", &format!("{tracker}: {file}")],
    );
    git(
        &elsewhere,
        &[
            "push",
            "-q",
            tracker_store.as_str(),
            &format!("HEAD:refs/trackers/{tracker}/main"),
        ],
    );
    git(&elsewhere, &["rev-parse", "HEAD"])
}

fn pin_of(store: &MetadataStore, commit: &str, name: &str) -> String {
    let tracker_store = store.paths().tracker_store.clone();
    git(
        &store.paths().project_root,
        &[
            &format!("--git-dir={tracker_store}"),
            "rev-parse",
            &format!("{commit}:.newgit/pins/{name}"),
        ],
    )
}

fn main_of(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

#[test]
fn an_instance_named_where_a_tracker_goes_gives_the_instance_form() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp, true);
    manager(&store).spawn("feature", None).expect("spawn");
    let error = manager(&store)
        .pull(None, &["t3code".to_owned(), "feature".to_owned()])
        .expect_err("an instance is not a tracker");
    assert_eq!(
        error.to_string(),
        "`feature` is an instance: `newgit pull t3code --instance feature`"
    );
}

#[test]
fn pull_remote_on_a_childs_target_names_the_pull_that_takes_it() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp, true);
    carve_repos(&store);
    // `repos` exports to a target of its own, declared where `t3code`
    // defines it.
    let target = temp.join("repos.git");
    git(&temp, &["init", "-q", "--bare", target.as_str()]);
    let repo = store.paths().project_root.clone();
    let definition = repo.join("apps/t3code/.newgit/trackers/repos.toml");
    // In place of the `export` it has, before any table the file ends with.
    let text: String = std::fs::read_to_string(&definition)
        .expect("child definition")
        .lines()
        .filter(|line| !line.starts_with("export "))
        .map(|line| format!("{line}\n"))
        .collect();
    let text = format!(
        "export = {}\n{text}",
        export_to(&target, "repos", "branch = \"main\"")
    );
    std::fs::write(&definition, text).expect("write");
    commit_all(&store, "repos export target");
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("repos", RemoteExportOptions::default())
        .expect("export repos");
    merge_pr(
        &temp,
        &target,
        "main",
        &[("effect/pr.md", "pr\n")],
        "repos: from a PR",
    );

    let error = manager(&store)
        .pull_remote("repos")
        .expect_err("root does not pin it");
    let message = error.to_string();
    assert!(
        message.contains("`newgit pull t3code/repos`"),
        "names the pull that works: {message}"
    );
    assert!(!message.contains("t3code__repos"), "{message}");

    let pulled = manager(&store)
        .pull(None, &["t3code__repos".to_owned()])
        .expect("pull t3code/repos");
    assert_eq!(pulled.pulled.len(), 1, "{pulled:?}");
    assert!(repo.join("apps/t3code/.repos/effect/pr.md").is_file());
}

/// `env`, a tracker outside Git, owning `apps/t3code/.env` as `definition`
/// writes it, and the secret there.
fn guard_env(store: &MetadataStore, definition: &str) {
    let repo = store.paths().project_root.clone();
    std::fs::write(repo.join(".newgit/trackers/env.toml"), definition).expect("write env");
    let ignore = repo.join(".gitignore");
    let mut text = std::fs::read_to_string(&ignore).unwrap_or_default();
    text.push_str("/apps/t3code/.env\n");
    std::fs::write(&ignore, text).expect("write .gitignore");
    commit_all(store, "env");
    manager(store).merge("main", "main").expect("publish");
    std::fs::write(repo.join("apps/t3code/.env"), "SECRET=local\n").expect("write secret");
}

fn a_target_adding_an_outside_git_file_is_not_taken_in(definition: &str) {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp, true);
    guard_env(&store, definition);
    let repo = store.paths().project_root.clone();
    let pin = pin_of(&store, "refs/trackers/root/instances/main", "t3code");
    let main = main_of(&store, "t3code");
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[(".env", "SECRET=leaked\n")],
        "t3: an env file",
    );

    let pulled = manager(&store).pull(None, &[]).expect("pull");
    assert_eq!(pulled.view, None, "{pulled:?}");
    let [(tracker, target, reason)] = pulled.target_failed.as_slice() else {
        panic!("the target is refused: {pulled:?}");
    };
    assert_eq!((tracker.as_str(), target.as_str()), ("t3code", "t3code"));
    assert!(
        reason.contains("adds `apps/t3code/.env`, which belongs to `env`, a tracker outside Git"),
        "{reason}"
    );
    assert!(reason.contains("it is not taken in"), "{reason}");
    assert!(!reason.contains("refusing to send"), "{reason}");

    let error = manager(&store)
        .pull_remote("t3code")
        .expect_err("pull --remote refuses it too");
    let message = error.to_string();
    assert!(message.contains("belongs to `env`"), "{message}");
    assert!(message.contains("it is not taken in"), "{message}");
    assert!(!message.contains("refusing to send"), "{message}");

    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/.env")).expect("read"),
        "SECRET=local\n",
        "the local secret stays"
    );
    assert_eq!(
        pin_of(&store, "refs/trackers/root/instances/main", "t3code"),
        pin
    );
    assert_eq!(main_of(&store, "t3code"), main);
    let tracker_store = store.paths().tracker_store.clone();
    let history = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "log",
            "--glob=refs/trackers/*",
            "--format=%H",
            "--",
            ".env",
        ],
    );
    assert_eq!(history, "", "no tracker's history holds it");
}

#[test]
fn a_target_adding_a_file_of_a_tracker_with_project_relative_paths_is_not_taken_in() {
    a_target_adding_an_outside_git_file_is_not_taken_in(
        "audience = \"user\"\nstorage = \"local\"\npaths = [\"apps/t3code/.env\"]\n",
    );
}

#[test]
fn a_target_adding_a_file_of_a_tracker_with_a_mount_is_not_taken_in() {
    a_target_adding_an_outside_git_file_is_not_taken_in(
        "audience = \"user\"\nstorage = \"local\"\nmount = \"apps/t3code\"\npaths = [\".env\"]\n",
    );
}

#[test]
fn two_instances_pulling_the_same_tips_make_one_merge() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp, true);
    manager(&store).spawn("one", None).expect("spawn one");
    manager(&store).spawn("two", None).expect("spawn two");
    let ours = advance(&temp, &store, "t3code", "src/here.ts");
    let theirs = merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: there",
    );

    manager(&store).pull(Some("one"), &[]).expect("pull one");
    // A second apart, so a merge stamped with the time it was made differs.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    manager(&store).pull(Some("two"), &[]).expect("pull two");
    let one = pin_of(&store, "refs/trackers/root/instances/one", "t3code");
    let two = pin_of(&store, "refs/trackers/root/instances/two", "t3code");
    let repo = store.paths().project_root.clone();
    let tracker_store = store.paths().tracker_store.clone();
    let in_store = |args: &[&str]| {
        let mut all = vec![format!("--git-dir={tracker_store}")];
        all.extend(args.iter().map(|arg| (*arg).to_owned()));
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        git(&repo, &all)
    };
    assert_eq!(
        in_store(&["log", "-1", "--format=%P", &one]),
        format!("{ours} {theirs}"),
        "a real merge"
    );
    assert_eq!(one, two, "the same merge commit in both");
    // newgit's, not whoever merged the pull request on GitHub (#426
    // review): dated as the later parent.
    assert_eq!(
        in_store(&["log", "-1", "--format=%an <%ae>|%cn <%ce>", &one]),
        "newgit <newgit@localhost>|newgit <newgit@localhost>"
    );
    let later = in_store(&["log", "-1", "--format=%ct", &theirs]).max(in_store(&[
        "log",
        "-1",
        "--format=%ct",
        &ours,
    ]));
    assert_eq!(in_store(&["log", "-1", "--format=%ct", &one]), later);

    manager(&store).merge("one", "main").expect("merge one");
    manager(&store).merge("two", "main").expect("merge two");
    let main = main_of(&store, "t3code");
    assert_eq!(main, one);
    let merges = in_store(&["rev-list", "--merges", &main, "--not", &ours, &theirs]);
    assert_eq!(merges.lines().count(), 1, "one merge on `main`: {merges}");
}

#[test]
fn an_unbound_child_that_is_not_behind_is_not_listed() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp, true);
    carve_repos(&store);
    manager(&store)
        .spawn_binding(
            "without",
            None,
            &BindingChoice {
                uses: Vec::new(),
                drops: vec!["t3code".to_owned()],
                private: false,
            },
        )
        .expect("spawn");

    let quiet = manager(&store).pull(Some("without"), &[]).expect("pull");
    assert_eq!(quiet.unbound, [], "nothing would have moved: {quiet:?}");

    let named = manager(&store)
        .pull(Some("without"), &["t3code__repos".to_owned()])
        .expect("pull it by name");
    assert_eq!(
        named.unbound,
        [("t3code__repos".to_owned(), "t3code".to_owned())],
        "named, it is listed"
    );
}

#[test]
fn a_child_named_beside_a_parent_whose_target_fails_is_listed_as_skipped() {
    let (_guard, temp) = tempdir();
    let (store, _) = exported(&temp, false);
    carve_repos(&store);
    advance(&temp, &store, "t3code__repos", "effect/new.md");

    let pulled = manager(&store)
        .pull(None, &["t3code".to_owned(), "t3code__repos".to_owned()])
        .expect("pull");
    assert_eq!(pulled.view, None, "{pulled:?}");
    assert_eq!(pulled.target_failed.len(), 1, "{pulled:?}");
    assert_eq!(
        pulled.with_parent,
        [("t3code__repos".to_owned(), "t3code".to_owned())]
    );
}

#[test]
fn export_after_a_pull_in_the_project_root_says_to_merge() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp, true);
    merge_pr(
        &temp,
        &github,
        "newgit",
        &[("src/pr.ts", "pr\n")],
        "t3: from a PR",
    );
    manager(&store).pull(None, &[]).expect("pull");

    let error = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect_err("main does not have them yet");
    let message = error.to_string();
    assert!(message.contains("`newgit merge main`"), "{message}");
    assert!(!message.contains("pull --remote"), "{message}");

    manager(&store).merge("main", "main").expect("merge");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export after the merge");
}
