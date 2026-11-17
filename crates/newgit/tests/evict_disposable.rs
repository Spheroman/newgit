//! Build output a resource declares `disposable` (#447): a plain `stop
//! --evict` drops it with the workspace instead of refusing, says so in
//! text, `--json`, and `status`, and the `start` after it brings back
//! everything else. The refusal without the declaration names it.

mod evict_support;

use evict_support::*;
use serde_json::Value;

const BUILD_OUTPUT: &str = "ownership = \"workspace\"\ndisposable = [\"target\"]\n";

#[test]
fn an_undeclared_target_refuses_and_names_the_declaration() {
    let (_guard, temp) = tempdir("evict-disposable-refuse");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::create_dir_all(ws.join("target/debug")).expect("mkdir");
    std::fs::write(ws.join("target/debug/app"), "binary\n").expect("write");

    let run = refused(&repo, &["stop", "--evict", "x"], "not evicting `x`");
    assert!(
        run.stderr.contains("disposable = [\"target\"]"),
        "names the fix:\n{}",
        run.stderr
    );
    assert!(ws.join("target/debug/app").exists(), "nothing was deleted");
}

#[test]
fn declared_build_output_is_dropped_and_said() {
    let (_guard, temp) = tempdir("evict-disposable");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::create_dir_all(ws.join("target/debug")).expect("mkdir");
    std::fs::write(ws.join("target/debug/app"), "binary\n").expect("write");
    std::fs::write(ws.join("target/.rustc_info.json"), "{}\n").expect("write");
    std::fs::write(ws.join("notes.txt"), "mine\n").expect("write");
    // Declared after the spawn: a declaration with no lifecycle holds for
    // the instances that already exist.
    std::fs::write(
        store.paths().resources.join("build-output.toml"),
        BUILD_OUTPUT,
    )
    .expect("write resource");

    let run = newgit_ok(&repo, &["stop", "--evict", "x", "--json"]);
    let out: Value = serde_json::from_str(&run.stdout).expect("json");
    assert_eq!(
        out["evicted"]["disposed"],
        serde_json::json!([{ "path": "target", "resource": "build-output", "files": 2 }]),
        "{}",
        run.stdout
    );
    assert_eq!(out["evicted"]["discarded"], serde_json::json!([]));
    assert!(!ws.join("target").exists());

    let status = newgit_ok(&repo, &["status"]);
    assert!(
        status.stdout.contains(
            "dropped as disposable, for its build to refill: `target` (2 files, `build-output`)"
        ),
        "{}",
        status.stdout
    );

    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(
        std::fs::read_to_string(ws.join("notes.txt")).expect("back"),
        "mine\n"
    );
    assert!(!ws.join("target").exists(), "nothing brings it back");
    let status = newgit_ok(&repo, &["status"]);
    assert!(
        !status.stdout.contains("dropped as disposable"),
        "{}",
        status.stdout
    );
}

#[test]
fn disposable_covers_only_what_it_names() {
    let (_guard, temp) = tempdir("evict-disposable-narrow");
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("build-output.toml"),
        BUILD_OUTPUT,
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::create_dir_all(ws.join("target")).expect("mkdir");
    std::fs::write(ws.join("target/app"), "binary\n").expect("write");
    // A stray ignored file is still refused: only `target` is disposable.
    let exclude = git_out(&ws, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude = ws.join(exclude.trim());
    let mut rules = std::fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str(".env.local\n");
    std::fs::write(&exclude, rules).expect("write exclude");
    std::fs::write(ws.join(".env.local"), "TOKEN=1\n").expect("write");

    let run = refused(&repo, &["stop", "--evict", "x"], "`.env.local`");
    assert!(!run.stderr.contains("`target"), "{}", run.stderr);
    assert!(ws.join(".env.local").exists());
}

#[test]
fn a_disposable_path_is_workspace_relative() {
    let (_guard, temp) = tempdir("evict-disposable-path");
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("bad.toml"),
        "ownership = \"workspace\"\ndisposable = [\"../elsewhere\"]\n",
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    refused(&repo, &["spawn", "x"], "`disposable` path `../elsewhere`");
}
