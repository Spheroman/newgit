//! `newgit spawn --from-checkpoint` ends in an undo to the record it
//! imports, so a resource its author's checkpoint went ahead without — not
//! up, and its capture failed — has no state to restore there either. That
//! undo ran the resource's `[restore] command` all the same, with
//! `{{state_ref}}` unrendered, against the new instance's freshly prepared
//! resource. The record crosses the remote as JSON, so this goes through it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn a_resource_the_checkpoint_left_out_keeps_what_the_spawn_bound() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let restored = temp.join("restored.txt");
    // Comes up for `review`, not for the checkpoint's author, and can be
    // dumped only while it is up.
    std::fs::write(
        fixture.store.join(".newgit/resources/db.toml"),
        format!(
            r#"ownership = "branch"

[actions.prepare]
command = "test {{{{branch.slug}}}} = review"

[checkpoint]
mode = "command"
command = "test {{{{branch.slug}}}} = review && echo dump-of-review"

[restore]
mode = "command"
command = "echo state_ref={{{{state_ref}}}} >> {restored}"
"#
        ),
    )
    .expect("resource");
    let author = fixture.newgit(&fixture.store, &["spawn", "feature-a"]);
    assert_eq!(author.status.code(), Some(1), "`db` does not come up");
    let checkpoint = fixture.ok(&workspace_of(&fixture, "feature-a"), &["checkpoint"]);
    assert!(
        checkpoint.contains("degraded: `db` not captured"),
        "{checkpoint}"
    );

    let output = fixture.newgit(
        &fixture.store,
        &[
            "spawn",
            "review",
            "--from-checkpoint",
            "feature-a/ckpt_001",
            "--json",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });

    assert_eq!(
        std::fs::read_to_string(&restored).unwrap_or_default(),
        "",
        "a restore with nothing to restore from must never run"
    );
    let db = json["resources"]
        .as_array()
        .and_then(|resources| resources.iter().find(|resource| resource["name"] == "db"))
        .unwrap_or_else(|| panic!("no `resources` entry for `db`:\n{stdout}"));
    assert_eq!(
        db["status"], "ready",
        "as `review`'s bind left it:\n{stdout}"
    );
    assert_eq!(db["action"], Value::Null, "nothing was restored:\n{stdout}");
    assert_eq!(db["ok"], Value::Null, "{stdout}");
    let warnings: Vec<&str> = json["warnings"]
        .as_array()
        .map(|warnings| warnings.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("holds no state for resource `db`")),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(0), "{stdout}");
}
