//! `newgit remove` run unattended (review of #465): removing an evicted
//! instance names the checkpoint that holds it; a workspace whose Git is
//! broken, a resource whose capture fails, and a Git repository nested in
//! the workspace never strand a removal or lose work in silence; `--purge`
//! names what a tracker outside Git held uncaptured; and disposable build
//! output is listed as it goes.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// A project with resources `(name, definition)` and trackers outside Git
/// `(name, path)` besides the sleeping one, and an instance `x`.
fn spawned(
    tag: &str,
    resources: &[(&str, &str)],
    trackers: &[(&str, &str)],
) -> (TempDir, Utf8PathBuf, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    for (name, definition) in resources {
        std::fs::write(
            store.paths().resources.join(format!("{name}.toml")),
            definition,
        )
        .expect("write resource");
    }
    let root = store.paths().project_root.clone();
    for (name, path) in trackers {
        newgit_ok(&root, &["tracker", "create", name]);
        newgit_ok(&root, &["tracker", "track", name, path]);
    }
    newgit_ok(&root, &["spawn", "x"]);
    (guard, root, temp.join("workspaces/x"))
}

fn lost(out: &Value) -> Vec<String> {
    out["lost"]
        .as_array()
        .expect("lost")
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn removing_an_evicted_instance_names_the_checkpoint_that_holds_it() {
    let (_guard, root, ws) = spawned("remove-evicted", &[], &[]);
    std::fs::write(ws.join("work.txt"), "w\n").expect("write");
    let evicted = newgit_ok(&root, &["stop", "--evict", "x", "--json"]);
    let id = json(&evicted)["evicted"]["checkpoint"]
        .as_str()
        .expect("evict checkpoint")
        .to_owned();

    let text = newgit_ok(&root, &["remove", "x"]);
    assert!(
        text.stdout.contains(&format!("kept:      checkpoint {id}"))
            && text
                .stdout
                .contains(&format!("`newgit spawn x --from-checkpoint x/{id}`")),
        "says what keeps it and how to bring it back:\n{}",
        text.stdout
    );
    let archived = std::fs::read_dir(root.join(".newgit/branches/archived"))
        .expect("archived")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read"))
        .collect::<String>();
    assert!(
        archived.contains(&format!("removed_to = \"{id}\"")),
        "{archived}"
    );

    // And in `--json`.
    let (_guard, root, ws) = spawned("remove-evicted-json", &[], &[]);
    std::fs::write(ws.join("work.txt"), "w\n").expect("write");
    newgit_ok(&root, &["stop", "--evict", "x"]);
    let out = json(&newgit_ok(&root, &["remove", "x", "--json"]));
    let id = out["checkpoint"].as_str().expect("checkpoint").to_owned();
    newgit_ok(
        &root,
        &["spawn", "x", "--from-checkpoint", &format!("x/{id}")],
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("work.txt")).ok().as_deref(),
        Some("w\n")
    );
}

#[test]
fn a_workspace_whose_git_is_broken_is_removed_with_force() {
    let (_guard, root, ws) = spawned("remove-broken-git", &[], &[]);
    std::fs::write(ws.join(".git/HEAD"), "garbage\n").expect("corrupt");

    let run = refused(&root, &["remove", "x"], "not removing `x`");
    assert!(
        run.stderr.contains("--force"),
        "names the way through:\n{}",
        run.stderr
    );
    assert!(ws.exists(), "nothing was deleted");

    let out = json(&newgit_ok(&root, &["remove", "x", "--force", "--json"]));
    assert!(out["checkpoint"].is_null(), "{out}");
    assert!(
        lost(&out)
            .iter()
            .any(|what| what.starts_with(&format!("could not inspect `{ws}`"))),
        "says it could not look: {out}"
    );
    assert!(!ws.exists(), "removed");
}

#[test]
fn a_workspace_whose_git_is_broken_is_removed_with_purge() {
    let (_guard, root, ws) = spawned("remove-broken-git-purge", &[], &[]);
    std::fs::write(ws.join(".git/HEAD"), "garbage\n").expect("corrupt");
    let out = json(&newgit_ok(&root, &["remove", "x", "--purge", "--json"]));
    assert!(
        lost(&out)
            .iter()
            .any(|what| what.starts_with("could not inspect")),
        "{out}"
    );
    assert!(!ws.exists(), "removed");
}

#[test]
fn purge_names_what_a_tracker_outside_git_held_uncaptured() {
    let (_guard, root, ws) = spawned("remove-purge-tracker", &[], &[("scratch", "scratch")]);
    std::fs::create_dir_all(ws.join("scratch")).expect("mkdir");
    std::fs::write(ws.join("scratch/plan.md"), "plan\n").expect("write");
    let out = json(&newgit_ok(&root, &["remove", "x", "--purge", "--json"]));
    assert!(
        lost(&out)
            .iter()
            .any(|what| what.contains("tracker `scratch`") && what.contains("`scratch/plan.md`")),
        "{out}"
    );
}

#[test]
fn purge_says_nothing_of_a_tracker_whose_content_was_captured() {
    let (_guard, root, ws) = spawned("remove-purge-captured", &[], &[("scratch", "scratch")]);
    std::fs::create_dir_all(ws.join("scratch")).expect("mkdir");
    std::fs::write(ws.join("scratch/plan.md"), "plan\n").expect("write");
    newgit_ok(&root, &["checkpoint", "x", "-m", "kept"]);
    let out = json(&newgit_ok(&root, &["remove", "x", "--purge", "--json"]));
    assert!(
        !lost(&out)
            .iter()
            .any(|what| what.contains("tracker `scratch`")),
        "{out}"
    );
}

#[test]
fn a_nested_repository_is_named_as_lost() {
    let (_guard, root, ws) = spawned("remove-nested", &[], &[]);
    let nested = ws.join("vendor/lib");
    std::fs::create_dir_all(&nested).expect("mkdir");
    git(&nested, &["init", "-q", "-b", "main"]);
    std::fs::write(nested.join("patch.txt"), "v1\n").expect("write");
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-q", "-m", "c"]);
    std::fs::write(nested.join("patch.txt"), "local edit\n").expect("write");

    let out = json(&newgit_ok(&root, &["remove", "x", "--json"]));
    let named = lost(&out);
    assert!(
        named.iter().any(
            |what| what.starts_with("`vendor/lib/`, a Git repository of its own")
                && what.contains("1 uncommitted change")
                && what.contains("1 commit on no remote")
        ),
        "{out}"
    );
}

#[test]
fn a_nested_repository_with_no_commit_refuses_unless_forced() {
    let (_guard, root, ws) = spawned("remove-nested-unborn", &[], &[]);
    let nested = ws.join("vendor/lib");
    std::fs::create_dir_all(&nested).expect("mkdir");
    git(&nested, &["init", "-q", "-b", "main"]);
    std::fs::write(nested.join("patch.txt"), "local patch\n").expect("write");

    let run = refused(&root, &["remove", "x"], "with no commit");
    assert!(run.stderr.contains("`vendor/lib`"), "{}", run.stderr);
    assert!(run.stderr.contains("--force"), "{}", run.stderr);
    assert!(nested.join("patch.txt").exists(), "nothing was deleted");

    let out = json(&newgit_ok(&root, &["remove", "x", "--force", "--json"]));
    assert!(
        lost(&out)
            .iter()
            .any(|what| what.starts_with("`vendor/lib/`") && what.contains("no commit")),
        "{out}"
    );
    assert!(!ws.exists(), "removed");
}

#[test]
fn a_resource_whose_capture_fails_does_not_refuse_the_removal() {
    let (_guard, root, ws) = spawned(
        "remove-capture-fails",
        &[(
            "snap",
            "ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\ncommand = \"exit 3\"\n",
        )],
        &[],
    );
    std::fs::write(ws.join("user.txt"), "untracked work\n").expect("write");

    let out = json(&newgit_ok(&root, &["remove", "x", "--json"]));
    let id = out["checkpoint"].as_str().expect("checkpoint").to_owned();
    assert!(
        lost(&out)
            .iter()
            .any(|what| what.contains("resource `snap`") && what.contains("capture failed")),
        "{out}"
    );
    // Fixed: the database is back.
    std::fs::write(
        root.join(".newgit/resources/snap.toml"),
        "ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\ncommand = \"true\"\n",
    )
    .expect("write resource");
    newgit_ok(
        &root,
        &["spawn", "x", "--from-checkpoint", &format!("x/{id}")],
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("user.txt")).ok().as_deref(),
        Some("untracked work\n"),
        "the rest of the workspace is in the checkpoint"
    );
}

#[test]
fn remove_lists_the_disposable_output_it_deletes() {
    let (_guard, root, ws) = spawned(
        "remove-disposable",
        &[(
            "build-output",
            "ownership = \"workspace\"\ndisposable = [\"target\"]\n",
        )],
        &[],
    );
    std::fs::create_dir_all(ws.join("target/debug")).expect("mkdir");
    std::fs::write(ws.join("target/debug/a"), "a\n").expect("write");
    std::fs::write(ws.join("target/debug/b"), "b\n").expect("write");

    let run = newgit_ok(&root, &["remove", "x"]);
    assert!(
        run.stdout.contains(
            "dropped:   `target` (2 files, `build-output`), declared disposable (its build \
             refills it)"
        ),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("lost:      `target"), "{}", run.stdout);
}
