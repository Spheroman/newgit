//! A tracker carved inside a mounted tracker in Git is that tracker's child
//! (#201): defined in its own `.newgit/trackers/`, pinned by its commits. So
//! it can stay out of a workspace while the parent's repository, exported on
//! its own, still holds everything it always did.
//!
//! The case this exists for: `t3code` mounted at `apps/t3code`, its
//! `.repos/` a vendored reference most instances never read.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

mod imports;

use camino::Utf8PathBuf;
use imports::*;
use newgit_core::manager::{BindingChoice, ExportReason, RemoteExportOptions, Uncommitted};
use newgit_core::store::MetadataStore;

const TARGET: &str = "t3code";

/// A project with `t3code` imported, holding `.repos/`, published, and
/// exported to a copy of its upstream.
fn imported(temp: &camino::Utf8Path) -> (MetadataStore, Utf8PathBuf, Utf8PathBuf) {
    let upstream = upstream(temp);
    commit_as(
        &upstream,
        "ana",
        1_690_000_000,
        &[
            (".repos/effect/LLMS.md", "effect\n"),
            (".repos/effect/src/index.ts", "index\n"),
        ],
        "Vendor effect",
    );
    let store = project(temp);
    let github = github_copy(temp, &upstream, TARGET);
    import(&store, &upstream);
    manager(&store).merge("main", "main").expect("publish");
    define_target(
        &store,
        TARGET,
        &format!("url = \"{github}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    );
    (store, upstream, github)
}

fn carve(store: &MetadataStore) {
    manager(store)
        .create_git_tracker(
            "repos",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/.repos")],
        )
        .expect("carve");
    manager(store).merge("main", "main").expect("publish");
}

fn export(store: &MetadataStore) {
    manager(store)
        .export_to_remote(TARGET, RemoteExportOptions::default())
        .expect("export");
}

fn tracker_main(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

fn tree(repo: &camino::Utf8Path, commit: &str) -> Vec<String> {
    git(repo, &["ls-tree", "-r", "--name-only", commit])
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn has(files: &[String], path: &str) -> bool {
    files.iter().any(|file| file == path)
}

#[test]
fn a_tracker_carved_inside_a_mount_is_its_parents_child() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);

    let definition = "apps/t3code/.newgit/trackers/repos.toml";
    assert!(repo.join(definition).is_file(), "defined in its parent");
    let text = std::fs::read_to_string(repo.join(definition)).expect("read");
    assert!(text.contains("mount = \".repos\""), "relative: {text}");

    let parent = tracker_main(&store, "t3code");
    let parent_files = tree(&repo, &parent);
    assert!(
        !parent_files.iter().any(|file| file.starts_with(".repos/")),
        "the parent gave its files up: {parent_files:?}"
    );
    assert!(has(&parent_files, ".newgit/pins/repos"), "{parent_files:?}");
    let child = tracker_main(&store, "t3code__repos");
    assert!(has(&tree(&repo, &child), "effect/LLMS.md"));

    let view = tree(&repo, "main");
    assert!(has(&view, "apps/t3code/.repos/effect/LLMS.md"), "{view:?}");
    assert!(
        !view.iter().any(|file| file.contains(".newgit/pins")),
        "{view:?}"
    );
}

#[test]
fn an_export_rooted_at_the_parent_still_holds_the_child() {
    let (_guard, temp) = tempdir();
    let (store, _, github) = imported(&temp);
    export(&store);
    let before = git(&github, &["rev-parse", "newgit"]);
    let files_before = tree(&github, "newgit");
    carve(&store);
    let exported = manager(&store)
        .export_to_remote(TARGET, RemoteExportOptions::default())
        .expect("export");
    let child = exported
        .trackers
        .iter()
        .find(|tracker| tracker.name == "t3code__repos")
        .expect("the report names the child");
    assert_eq!(child.reason, ExportReason::Mounted);
    assert_eq!(
        child.commit.as_deref(),
        Some(tracker_main(&store, "t3code__repos").as_str()),
        "the commit it ships"
    );
    assert!(!child.unmerged, "merged: there is nothing it left behind");

    let files = tree(&github, "newgit");
    assert!(has(&files, ".repos/effect/LLMS.md"), "{files:?}");
    assert!(has(&files, ".repos/effect/src/index.ts"), "{files:?}");
    assert!(
        !files.iter().any(|file| file.starts_with("apps/")),
        "{files:?}"
    );
    assert!(
        !files.iter().any(|file| file.starts_with(".newgit/pins")),
        "{files:?}"
    );
    // The one thing the carve adds is the definition saying where it went.
    let added: Vec<&String> = files
        .iter()
        .filter(|file| !files_before.contains(file))
        .collect();
    assert_eq!(added, [".newgit/trackers/repos.toml"]);
    assert!(
        files_before.iter().all(|file| files.contains(file)),
        "nothing the fork had is gone"
    );
    assert!(
        git_ok(&github, &["merge-base", "--is-ancestor", &before, "newgit"]),
        "a fast-forward of the fork"
    );

    let tip = git(&github, &["rev-parse", "newgit"]);
    export(&store);
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        tip,
        "a repeat is a no-op"
    );
}

/// `root` names `t3code`. Nobody chose to send a child only one person may
/// read, and the fork may well be public.
#[test]
fn an_export_refuses_a_child_narrower_than_its_root_naming_it() {
    let (_guard, temp) = tempdir();
    let (store, _, github) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    export(&store);
    let tip = git(&github, &["rev-parse", "newgit"]);
    let path = repo.join("apps/t3code/.newgit/trackers/repos.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(
        &path,
        text.replace("audience = \"project-devs\"", "audience = \"user:ana\""),
    )
    .expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "Narrow t3code__repos"]);
    manager(&store).merge("main", "main").expect("publish");

    let error = manager(&store)
        .export_to_remote(TARGET, RemoteExportOptions::default())
        .expect_err("refused");
    let text = error.to_string();
    assert!(text.contains("`t3code/repos`"), "{text}");
    assert!(text.contains("user:ana"), "{text}");
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        tip,
        "nothing was sent"
    );
}

#[test]
fn a_commit_on_the_target_goes_to_the_trackers_that_own_its_files() {
    let (_guard, temp) = tempdir();
    let (store, _, github) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    export(&store);
    let parent_before = tracker_main(&store, "t3code");
    let child_before = tracker_main(&store, "t3code__repos");

    let work = temp.join("github-work");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            work.as_str(),
        ],
    );
    commit_as(
        &work,
        "bob",
        1_800_000_000,
        &[
            (".repos/effect/LLMS.md", "effect, synced\n"),
            ("src/upstream.ts", "upstream\n"),
        ],
        "Sync from upstream",
    );
    git(&work, &["push", "-q", "origin", "newgit"]);

    manager(&store).pull_remote(TARGET).expect("pull");

    let view = "main";
    assert_eq!(
        git(
            &repo,
            &["show", &format!("{view}:apps/t3code/.repos/effect/LLMS.md")]
        ),
        "effect, synced"
    );
    assert_eq!(
        git(
            &repo,
            &["show", &format!("{view}:apps/t3code/src/upstream.ts")]
        ),
        "upstream"
    );
    manager(&store).merge("main", "main").expect("publish");

    let parent = tracker_main(&store, "t3code");
    let child = tracker_main(&store, "t3code__repos");
    assert_ne!(parent, parent_before);
    assert_ne!(child, child_before, "the child moved, not only the parent");
    assert_eq!(
        git(&repo, &["show", &format!("{child}:effect/LLMS.md")]),
        "effect, synced"
    );
    assert!(
        !tree(&repo, &parent)
            .iter()
            .any(|file| file.starts_with(".repos/")),
        "nothing hidden in the parent under the child's mount"
    );

    // Back where it started: the target has nothing new, and neither do we.
    let tip = git(&github, &["rev-parse", "newgit"]);
    export(&store);
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        tip,
        "an export after a pull is a no-op"
    );
    let again = manager(&store).pull_remote(TARGET).expect("pull again");
    assert_eq!(again.commits, 0, "a repeat pull finds nothing");
}

/// How the fork really takes upstream: `pingdotgg/t3code` merged into its
/// `newgit` branch. Upstream's commits know nothing of the child and hold
/// `.repos/` as plain files; the merge has the definition.
#[test]
fn an_upstream_merged_on_the_target_reaches_the_child() {
    let (_guard, temp) = tempdir();
    let (store, upstream, github) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    export(&store);
    let child_before = tracker_main(&store, "t3code__repos");

    commit_as(
        &upstream,
        "theo",
        1_800_000_000,
        &[
            (".repos/effect/LLMS.md", "effect, bumped upstream\n"),
            ("src/from-upstream.ts", "upstream\n"),
        ],
        "Bump effect",
    );
    let work = temp.join("github-work");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            work.as_str(),
        ],
    );
    git(&work, &["config", "user.name", "ana"]);
    git(&work, &["config", "user.email", "ana@example.com"]);
    git(&work, &["fetch", "-q", upstream.as_str(), "newgit"]);
    git(
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge upstream",
            "FETCH_HEAD",
        ],
    );
    git(&work, &["push", "-q", "origin", "newgit"]);

    manager(&store).pull_remote(TARGET).expect("pull");
    manager(&store).merge("main", "main").expect("publish");

    assert_eq!(
        git(&repo, &["show", "main:apps/t3code/.repos/effect/LLMS.md"]),
        "effect, bumped upstream"
    );
    assert_eq!(
        git(&repo, &["show", "main:apps/t3code/src/from-upstream.ts"]),
        "upstream"
    );
    let parent = tracker_main(&store, "t3code");
    let child = tracker_main(&store, "t3code__repos");
    assert_ne!(child, child_before, "the child took the bump");
    assert_eq!(
        git(&repo, &["show", &format!("{child}:effect/LLMS.md")]),
        "effect, bumped upstream"
    );
    assert!(
        !tree(&repo, &parent)
            .iter()
            .any(|file| file.starts_with(".repos/")),
        "nothing hidden in the parent under the child's mount"
    );
    let files = tree(&github, "newgit");
    let tip = git(&github, &["rev-parse", "newgit"]);
    export(&store);
    assert_eq!(
        git(&github, &["rev-parse", "newgit"]),
        tip,
        "an export after a pull is a no-op"
    );
    assert!(has(&files, ".repos/effect/LLMS.md"), "{files:?}");
}

#[test]
fn a_target_that_does_not_ship_the_parent_leaves_the_child_out_too() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    let mirror = temp.join("mirror.git");
    git(&temp, &["init", "-q", "--bare", mirror.as_str()]);
    define_target(
        &store,
        "mirror",
        &format!("url = \"{mirror}\"\nbranch = \"main\"\ntrackers = [\"root\", \"access\"]\n"),
    );
    manager(&store)
        .export_to_remote("mirror", RemoteExportOptions::default())
        .expect("export");
    let files = tree(&mirror, "main");
    assert!(
        !files.iter().any(|file| file.starts_with("apps/t3code")),
        "{files:?}"
    );
    assert!(has(
        &tree(&repo, "main"),
        "apps/t3code/.repos/effect/LLMS.md"
    ));
}

fn on_request(store: &MetadataStore) {
    let repo = store.paths().project_root.clone();
    let path = repo.join("apps/t3code/.newgit/trackers/repos.toml");
    let mut text = std::fs::read_to_string(&path).expect("read");
    text.push_str("priority = -1\n");
    std::fs::write(&path, text).expect("write");
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-q", "-m", "Bind t3code__repos only when asked"],
    );
    manager(store).merge("main", "main").expect("publish");
}

fn choice(uses: &[&str], drops: &[&str]) -> BindingChoice {
    BindingChoice {
        uses: uses.iter().map(ToString::to_string).collect(),
        drops: drops.iter().map(ToString::to_string).collect(),
        private: false,
    }
}

#[test]
fn a_child_can_stay_out_of_a_workspace_and_its_parents_work_merges_without_it() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    on_request(&store);

    let spawned = manager(&store)
        .spawn_binding("feature", None, &BindingChoice::default())
        .expect("spawn");
    assert!(spawned.branch.dropped.contains("t3code__repos"));
    assert!(!spawned.branch.dropped.contains("t3code"));
    let workspace = spawned.branch.workspace_path.clone();
    assert!(!workspace.join("apps/t3code/.repos").exists());
    assert!(
        workspace.join("apps/t3code/src").exists(),
        "the parent is here"
    );

    commit_as(
        &workspace,
        "ana",
        1_800_000_000,
        &[("apps/t3code/src/feature.ts", "feature\n")],
        "Work on t3code",
    );
    manager(&store)
        .checkpoint("feature", None)
        .expect("checkpoint");
    manager(&store).merge("feature", "main").expect("merge");
    let view = tree(&repo, "main");
    assert!(has(&view, "apps/t3code/src/feature.ts"), "{view:?}");
    assert!(
        has(&view, "apps/t3code/.repos/effect/LLMS.md"),
        "kept: {view:?}"
    );
}

fn parent_on_request(store: &MetadataStore) {
    let repo = store.paths().project_root.clone();
    let path = store.paths().trackers.join("t3code.toml");
    let mut text = std::fs::read_to_string(&path).expect("read");
    text.push_str("priority = -1\n");
    std::fs::write(&path, text).expect("write");
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-q", "-m", "Bind t3code only when asked"],
    );
    manager(store).merge("main", "main").expect("publish");
}

fn spawned_without(store: &MetadataStore, name: &str, choice: &BindingChoice) -> Vec<String> {
    let spawned = manager(store)
        .spawn_binding(name, None, choice)
        .expect("spawn");
    let mut dropped: Vec<String> = spawned.branch.dropped.into_iter().collect();
    dropped.retain(|tracker| tracker.starts_with("t3code"));
    dropped
}

#[test]
fn a_child_is_bound_only_where_its_parent_is() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);

    // Bound by default: it comes with its parent.
    assert!(spawned_without(&store, "both", &BindingChoice::default()).is_empty());
    // Parent left out: the child cannot exist without it.
    assert_eq!(
        spawned_without(&store, "neither", &choice(&[], &["t3code"])),
        ["t3code", "t3code__repos"]
    );
    // Asked for by name: it brings its parent.
    assert!(spawned_without(&store, "asked", &choice(&["t3code__repos"], &[])).is_empty());
    // Its own negative priority keeps it out while the parent is bound.
    on_request(&store);
    assert_eq!(
        spawned_without(&store, "parent-only", &BindingChoice::default()),
        ["t3code__repos"]
    );
    assert_eq!(
        spawned_without(&store, "parent-asked", &choice(&["t3code__repos"], &[])),
        Vec::<String>::new()
    );
}

/// The parent on request, the child ready to come with it.
#[test]
fn a_parent_on_request_brings_a_child_that_is_not() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    parent_on_request(&store);
    assert_eq!(
        spawned_without(&store, "default", &BindingChoice::default()),
        ["t3code", "t3code__repos"],
        "neither, though the child's own priority would bind it"
    );
    assert!(
        spawned_without(&store, "asked", &choice(&["t3code"], &[])).is_empty(),
        "both"
    );

    let used = manager(&store)
        .use_tracker("default", "t3code")
        .expect("use");
    assert_eq!(used.brought, ["t3code__repos"]);
    let branch = store.find_branch("default").expect("record");
    assert!(
        branch
            .workspace_path
            .join("apps/t3code/.repos/effect/LLMS.md")
            .exists()
    );
}

/// Both on request: the way to keep all of t3code out by default.
#[test]
fn with_both_on_request_a_parent_can_come_without_its_child() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    on_request(&store);
    parent_on_request(&store);
    assert_eq!(
        spawned_without(&store, "neither", &BindingChoice::default()),
        ["t3code", "t3code__repos"]
    );
    let used = manager(&store)
        .use_tracker("neither", "t3code")
        .expect("use");
    assert!(used.brought.is_empty(), "{:?}", used.brought);
    let branch = store.find_branch("neither").expect("record");
    assert!(branch.dropped.contains("t3code__repos"));
    assert!(branch.workspace_path.join("apps/t3code/src").exists());
    assert!(!branch.workspace_path.join("apps/t3code/.repos").exists());
}

#[test]
fn asking_for_a_child_brings_its_parent_and_dropping_the_parent_takes_it() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let spawned = manager(&store)
        .spawn_binding("x", None, &choice(&[], &["t3code"]))
        .expect("spawn");
    assert!(!spawned.branch.workspace_path.join("apps/t3code").exists());

    let used = manager(&store)
        .use_tracker("x", "t3code__repos")
        .expect("use");
    assert_eq!(used.brought, ["t3code"]);
    let branch = store.find_branch("x").expect("record");
    assert!(branch.dropped.is_empty(), "{:?}", branch.dropped);
    assert!(
        branch
            .workspace_path
            .join("apps/t3code/.repos/effect/LLMS.md")
            .exists()
    );
    assert!(branch.workspace_path.join("apps/t3code/src").exists());

    let dropped = manager(&store)
        .drop_tracker("x", "t3code", Uncommitted::Refuse)
        .expect("drop");
    assert_eq!(dropped.children, ["t3code__repos"], "says what else went");
    let branch = store.find_branch("x").expect("record");
    assert!(branch.dropped.contains("t3code") && branch.dropped.contains("t3code__repos"));
    assert!(!branch.workspace_path.join("apps/t3code").exists());
}

#[test]
fn a_spawn_asking_for_a_child_of_a_dropped_parent_is_refused_by_name() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let error = manager(&store)
        .spawn_binding("both", None, &choice(&["t3code__repos"], &["t3code"]))
        .expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("`t3code`") && text.contains("`t3code/repos`"),
        "{text}"
    );
}

fn define_in_t3code(store: &MetadataStore, name: &str, text: &str) {
    let dir = store
        .paths()
        .project_root
        .join("apps/t3code/.newgit/trackers");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(format!("{name}.toml")), text).expect("write");
}

fn definition_names(store: &MetadataStore) -> Vec<String> {
    store
        .load_tracker_definitions()
        .expect("a bad nested file stops nothing")
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

/// These files come in with other people's commits. One that cannot be a
/// child must not stop every command, the one that would fix it included.
#[test]
fn a_nested_file_that_cannot_be_a_child_is_skipped_not_fatal() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let in_git = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\n";
    define_in_t3code(
        &store,
        "outside",
        "audience = \"project-devs\"\nstorage = \"local\"\nmount = \"x\"\npaths = [\".\"]\n",
    );
    define_in_t3code(&store, "unmounted", &format!("{in_git}paths = [\".\"]\n"));
    define_in_t3code(&store, "broken", "this is not toml [");
    define_in_t3code(
        &store,
        "t3code",
        &format!("{in_git}mount = \"x\"\npaths = [\".\"]\n"),
    );
    let names = definition_names(&store);
    assert_eq!(names.iter().filter(|name| *name == "t3code").count(), 1);
    for skipped in ["outside", "unmounted", "broken"] {
        assert!(!names.iter().any(|name| name == skipped), "{names:?}");
    }
    manager(&store)
        .spawn("still-works", None)
        .expect("every command still runs");
}

/// A repository that is a newgit project itself, imported whole.
#[test]
fn another_projects_definitions_under_a_mount_are_not_this_projects() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let in_git = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\n";
    define_in_t3code(
        &store,
        "theirs",
        &format!("{in_git}mount = \"x\"\npaths = [\".\"]\n"),
    );
    std::fs::write(
        store
            .paths()
            .project_root
            .join("apps/t3code/.newgit/config.toml"),
        "[project]\nname = \"other\"\n",
    )
    .expect("write");
    let names = definition_names(&store);
    assert!(!names.iter().any(|name| name == "theirs"), "{names:?}");
}

#[test]
fn a_name_already_taken_cannot_be_carved_again() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let error = manager(&store)
        .create_git_tracker(
            "repos",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/.repos")],
        )
        .expect_err("refused");
    assert!(error.to_string().contains("`t3code/repos`"), "{error}");
}

/// The notes that recognise exported commits are local to the machine that
/// exported. Composing is deterministic, so a pull on any other machine
/// makes them again rather than splitting the whole history anew.
#[test]
fn a_pull_on_a_machine_that_never_exported_recognises_what_was_exported() {
    let (_guard, temp) = tempdir();
    let (store, _, github) = imported(&temp);
    let repo = store.paths().project_root.clone();
    carve(&store);
    export(&store);
    let parent_before = tracker_main(&store, "t3code");
    let work = temp.join("github-work");
    git(
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            work.as_str(),
        ],
    );
    commit_as(
        &work,
        "bob",
        1_800_000_000,
        &[("src/later.ts", "later\n")],
        "Later",
    );
    git(&work, &["push", "-q", "origin", "newgit"]);

    git(&repo, &["update-ref", "-d", "refs/notes/newgit-composed"]);
    manager(&store).pull_remote(TARGET).expect("pull");
    manager(&store).merge("main", "main").expect("publish");

    let parent = tracker_main(&store, "t3code");
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{parent}^")]),
        parent_before,
        "one commit on top: nothing was split again"
    );
}

/// A tracker's path in the one namespace, as a filesystem reads one.
#[test]
fn a_tracker_is_named_as_a_path_from_where_you_stand() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let repo = store.paths().project_root.clone();
    let manager = manager(&store);
    let top = manager.namespace_root();
    assert_eq!(top, "/user/proj", "this project is on no remote");

    // From the top of the project.
    let id = |cwd: &camino::Utf8Path, given: &str| manager.resolve_tracker(cwd, given);
    assert_eq!(id(&repo, "t3code").expect("resolve"), "t3code");
    assert_eq!(id(&repo, "t3code/repos").expect("resolve"), "t3code__repos");
    assert_eq!(id(&repo, "root").expect("resolve"), "root");

    // From inside `t3code`.
    let inside = repo.join("apps/t3code/src");
    assert_eq!(manager.tracker_at(&inside), "t3code");
    assert_eq!(id(&inside, "repos").expect("resolve"), "t3code__repos");
    assert_eq!(id(&inside, "./repos").expect("resolve"), "t3code__repos");
    assert_eq!(id(&inside, "..").expect("resolve"), "root");
    assert_eq!(id(&inside, "../access").expect("resolve"), "access");

    // Inside the child, and in a workspace rather than the project root.
    assert_eq!(
        manager.tracker_at(&repo.join("apps/t3code/.repos/effect")),
        "t3code__repos"
    );
    let workspace = manager
        .spawn("x", None)
        .expect("spawn")
        .branch
        .workspace_path;
    assert_eq!(
        manager.tracker_at(&workspace.join("apps/t3code/.repos")),
        "t3code__repos"
    );
    assert_eq!(manager.tracker_at(&workspace), "root");

    // Absolute: the same from anywhere.
    let absolute = format!("{top}/t3code/repos");
    assert_eq!(id(&repo, &absolute).expect("resolve"), "t3code__repos");
    assert_eq!(id(&inside, &absolute).expect("resolve"), "t3code__repos");
    assert_eq!(id(&inside, &top).expect("resolve"), "root");

    // A name is not looked for further up: it means one thing where typed.
    let text = id(&inside, "access").expect_err("refused").to_string();
    assert!(text.contains(&format!("{top}/access")), "{text}");
    let text = id(&repo, "..").expect_err("refused").to_string();
    assert!(text.contains("above this project"), "{text}");
    let text = id(&repo, "/remote/someone/else/x")
        .expect_err("refused")
        .to_string();
    assert!(text.contains("not in this project"), "{text}");
}

/// A symlink on the way to a project spells the same directory another way:
/// the tracker you are in is found on disk, not by comparing strings.
#[test]
fn where_you_stand_does_not_depend_on_how_the_project_is_spelled() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let repo = store.paths().project_root.clone();
    let alias = temp.join("alias");
    std::os::unix::fs::symlink(&repo, &alias).expect("symlink");
    let spelled =
        newgit_core::manager::BranchManager::open(MetadataStore::at(&alias)).expect("manager");
    assert_eq!(spelled.tracker_at(&repo.join("apps/t3code/src")), "t3code");
    assert_eq!(
        spelled
            .resolve_tracker(&repo.join("apps/t3code/src"), "repos")
            .expect("resolve"),
        "t3code__repos"
    );
}

/// Two parents can each have a `docs`, as two directories can.
#[test]
fn names_are_unique_within_a_parent_not_across_a_project() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    commit_as(
        &repo,
        "ana",
        1_700_000_000,
        &[
            ("docs/index.md", "root docs\n"),
            ("apps/t3code/docs/index.md", "t3code docs\n"),
        ],
        "Docs twice",
    );
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .create_git_tracker("docs", "project-devs", &[Utf8PathBuf::from("docs")])
        .expect("root's");
    manager(&store)
        .create_git_tracker(
            "docs",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/docs")],
        )
        .expect("t3code's");
    manager(&store).merge("main", "main").expect("publish");
    assert_eq!(
        git(
            &repo,
            &[
                "show",
                &format!("{}:index.md", tracker_main(&store, "docs"))
            ]
        ),
        "root docs"
    );
    assert_eq!(
        git(
            &repo,
            &[
                "show",
                &format!("{}:index.md", tracker_main(&store, "t3code__docs"))
            ]
        ),
        "t3code docs"
    );
    let view = tree(&repo, "main");
    assert!(has(&view, "docs/index.md") && has(&view, "apps/t3code/docs/index.md"));
}

#[test]
fn a_full_path_must_say_where_path_puts_it_and_the_separator_is_reserved() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    manager(&store)
        .create_git_tracker(
            "t3code/repos",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/.repos")],
        )
        .expect("the full path is the same tracker");
    let error = manager(&store)
        .create_git_tracker(
            "elsewhere/src",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/src")],
        )
        .expect_err("refused");
    assert!(error.to_string().contains("t3code/src"), "{error}");
    manager(&store)
        .create_git_tracker(
            "a__b",
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/src")],
        )
        .expect_err("reserved");
}

/// On a snapshot clone, what a rooted view needs is fetched by id: under a
/// bare name nothing is fetched, and the view silently lacks the child.
#[test]
fn a_rooted_view_asks_for_its_childrens_commits_by_id() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    carve(&store);
    let repo = store.paths().project_root.clone();
    let parent = tracker_main(&store, "t3code");
    git(&repo, &["update-ref", "refs/heads/t3code-tip", &parent]);
    // Holds the parent's commit but not its child's.
    let snapshot = temp.join("snapshot");
    git(&temp, &["init", "-q", "--bare", snapshot.as_str()]);
    git(
        &snapshot,
        &[
            "fetch",
            "-q",
            "--depth=1",
            repo.as_str(),
            "refs/heads/t3code-tip",
        ],
    );
    let mut asked = Vec::new();
    newgit_core::snapshots::fetch_pins(
        &snapshot,
        "t3code",
        &[parent],
        &std::collections::BTreeMap::new(),
        &mut |tracker| {
            asked.push(tracker.to_owned());
            Ok(None)
        },
    )
    .expect("fetch");
    assert_eq!(asked, ["t3code__repos"]);
}

/// A repository that is a newgit project itself, committed under a mount: its
/// definitions are none of this project's, on either side of a split.
#[test]
fn a_split_carves_nothing_out_of_another_projects_definitions() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    let in_git = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\n";
    commit_as(
        &repo,
        "ana",
        1_700_000_000,
        &[
            (
                "apps/t3code/.newgit/config.toml",
                "[project]\nname = \"other\"\n",
            ),
            (
                "apps/t3code/.newgit/trackers/lib.toml",
                &format!("{in_git}mount = \"src\"\npaths = [\".\"]\n"),
            ),
        ],
        "Another project's metadata",
    );
    manager(&store).merge("main", "main").expect("publish");
    let parent = tracker_main(&store, "t3code");
    assert!(
        has(&tree(&repo, &parent), "src/app.ts"),
        "nothing carved out"
    );
    assert!(store_ref(&store, "refs/trackers/t3code__lib/main").is_none());
}

/// A child only `git = false` or unparseable is no child, so it refuses
/// nothing: its paths are its parent's.
#[test]
fn a_nested_definition_the_project_ignores_is_ignored_by_a_split_too() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let repo = store.paths().project_root.clone();
    commit_as(
        &repo,
        "ana",
        1_700_000_000,
        &[
            (
                "apps/t3code/.newgit/trackers/env.toml",
                "audience = \"project-devs\"\nstorage = \"local\"\npaths = [\".env\"]\n",
            ),
            ("apps/t3code/.newgit/trackers/broken.toml", "not toml ["),
            ("apps/t3code/.env", "A=1\n"),
        ],
        "Things that are not children",
    );
    manager(&store).merge("main", "main").expect("publish");
    assert!(has(&tree(&repo, &tracker_main(&store, "t3code")), ".env"));
}

#[test]
fn every_way_of_naming_a_tracker_refuses_a_name_that_cannot_be_an_id() {
    let (_guard, temp) = tempdir();
    let (store, upstream, _) = imported(&temp);
    for bad in ["a__b", "trailing_"] {
        manager(&store)
            .create_git_tracker(bad, "project-devs", &[Utf8PathBuf::from("apps/t3code/src")])
            .expect_err("create --git");
        manager(&store)
            .import_git_tracker(
                bad,
                "project-devs",
                camino::Utf8Path::new("apps/other"),
                upstream.as_str(),
                Some("newgit"),
            )
            .expect_err("import");
        manager(&store)
            .create_tracker(
                bad,
                "project-devs",
                newgit_core::tracker::Storage::Local,
                false,
            )
            .expect_err("create");
    }
    // Written by hand: skipped, so it cannot share a child's refs.
    let hand = store.paths().trackers.join("a__b.toml");
    std::fs::write(
        &hand,
        "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \"h\"\npaths = [\".\"]\n",
    )
    .expect("write");
    assert!(!definition_names(&store).iter().any(|name| name == "a__b"));
    std::fs::remove_file(hand).expect("remove");
    let long = "x".repeat(40);
    let error = manager(&store)
        .create_git_tracker(
            &long,
            "project-devs",
            &[Utf8PathBuf::from("apps/t3code/src")],
        )
        .map(|_| ())
        .and_then(|()| {
            manager(&store).create_git_tracker(
                &"y".repeat(40),
                "project-devs",
                &[Utf8PathBuf::from("apps/t3code/src/deep")],
            )
        });
    assert!(error.is_err(), "too long for a remote repo's name");
}

/// `requires` is a path from the parent's tree, like everything else there.
#[test]
fn a_nested_requires_names_a_sibling_as_a_path() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    let in_git = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\n";
    define_in_t3code(
        &store,
        "needs",
        &format!("{in_git}mount = \"n\"\npaths = [\".\"]\nrequires = [\"vendor\", \"../site\"]\n"),
    );
    define_in_t3code(
        &store,
        "vendor",
        &format!("{in_git}mount = \"v\"\npaths = [\".\"]\n"),
    );
    let definitions = store.load_tracker_definitions().expect("definitions");
    let needs = definitions
        .iter()
        .find(|definition| definition.name == "t3code__needs")
        .expect("needs");
    assert_eq!(needs.requires, ["t3code__vendor", "../site"]);
}

/// An export file says `t3code/repos`, not an id.
#[test]
fn an_export_target_names_trackers_as_paths() {
    let (_guard, temp) = tempdir();
    let (store, _, _) = imported(&temp);
    define_target(
        &store,
        "paths",
        "url = \"/nowhere\"\nbranch = \"main\"\ntrackers = [\"root\", \"t3code/repos\"]\n",
    );
    let targets = store.load_export_targets().expect("targets");
    let target = targets
        .iter()
        .find(|target| target.name == "paths")
        .expect("target");
    assert_eq!(target.trackers, ["root", "t3code__repos"]);
}
