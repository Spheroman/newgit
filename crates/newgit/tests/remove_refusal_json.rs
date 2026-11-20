//! `newgit remove --json` says why it refused. A caller (the T3 Code fork
//! deleting a thread) has to tell a removal `newgit start` would unblock,
//! because the server whose dump failed is stopped, from one only `--force`
//! gets past, and it printed nothing on a refusal.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;
use serde_json::{Value, json};

fn parsed(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// A project whose resource `db` keeps its data in the workspace, runs a
/// server newgit supervises, and whose dump always fails; and an instance
/// `x` of it.
fn spawned(tag: &str) -> (TempDir, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(repo.join(".gitignore"), "target/\npgdata/\n").expect("write");
    git(&repo, &["commit", "-qam", "ignore pgdata"]);
    std::fs::write(
        store.paths().resources.join("db.toml"),
        "ownership = \"workspace\"\n\n[actions.prepare]\ncommand = \"mkdir -p pgdata\"\n\n\
         [actions.serve]\ncommand = \"exec sleep 600\"\nlong_running = true\nautostart = true\n\n\
         [checkpoint]\nmode = \"command\"\ncommand = \"exit 3\"\n",
    )
    .expect("write resource");
    newgit_ok(&repo, &["spawn", "x"]);
    (guard, repo)
}

#[test]
fn a_refusal_names_its_reason_and_start_only_when_the_server_is_down() {
    let (_guard, repo) = spawned("remove-refusal-json");

    // Its server is running, so starting it would change nothing.
    let run = refused(&repo, &["remove", "x", "--json"], "not removing `x`");
    let out = parsed(&run);
    assert_eq!(out["instance"], "x", "{out}");
    assert_eq!(out["removed"], false, "{out}");
    let refusal = &out["refused"];
    assert_eq!(refusal["reason"], "capture_failed", "{out}");
    assert_eq!(refusal["resources"], json!(["db"]), "{out}");
    assert!(
        refusal["checkpoint"]
            .as_str()
            .is_some_and(|id| id.starts_with("ckpt_")),
        "{out}"
    );
    assert_eq!(refusal["next"], Value::Null, "{out}");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|message| message.contains("not removing `x`")),
        "{out}"
    );

    // Stopped, as a settled thread's is: `start` is what brings it back.
    newgit_ok(&repo, &["stop", "x"]);
    let out = parsed(&refused(
        &repo,
        &["remove", "x", "--json"],
        "not removing `x`",
    ));
    assert_eq!(out["refused"]["reason"], "capture_failed", "{out}");
    assert_eq!(out["refused"]["next"], "newgit start x", "{out}");

    // A removal that goes through says so in the same key.
    let out = parsed(&newgit_ok(&repo, &["remove", "x", "--force", "--json"]));
    assert_eq!(out["removed"], true, "{out}");
    assert!(out.get("refused").is_none(), "{out}");
}

#[test]
fn a_refusal_for_another_reason_names_that_one() {
    let (_guard, repo) = spawned("remove-refusal-json-unborn");
    let workspace = repo.parent().expect("temp").join("workspaces/x");
    // A Git repository with no commit, which no checkpoint can take.
    std::fs::create_dir_all(workspace.join("nested")).expect("mkdir");
    git(&workspace.join("nested"), &["init", "-q"]);

    let out = parsed(&refused(
        &repo,
        &["remove", "x", "--json"],
        "not removing `x`",
    ));
    assert_eq!(out["refused"]["reason"], "unborn_repos", "{out}");
    assert_eq!(out["refused"]["next"], Value::Null, "{out}");
    newgit_ok(&repo, &["remove", "x", "--force", "--purge"]);
}
