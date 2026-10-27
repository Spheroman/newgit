//! #209 for `newgit spawn --from-checkpoint --json`, which reports resources
//! through the same `resource_json` as a plain `spawn --json`: a dependent
//! with no `prepare` is recorded `ready` when its dependency fails, and must
//! still be reported `blocked`, as `status` and `action` treat it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions, rather than appended to an existing integration test file.

#![allow(dead_code)]

include!("support/fake_remote.rs");

#[test]
fn spawn_from_checkpoint_json_calls_a_dependent_without_prepare_blocked() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    fixture.ok(&workspace, &["checkpoint"]);
    // Defined after the checkpoint, so the spawn's own bind is what decides
    // them, and `feature-a` never had to come up with `base` failing.
    std::fs::write(
        fixture.store.join(".newgit/resources/base.toml"),
        r#"ownership = "branch"

[actions.prepare]
command = "exit 1"
"#,
    )
    .expect("resource");
    std::fs::write(
        fixture.store.join(".newgit/resources/dep.toml"),
        r#"ownership = "branch"
depends_on = ["base"]

[actions.start]
command = "sleep 1000"
long_running = true
"#,
    )
    .expect("resource");

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
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let dep = json["resources"]
        .as_array()
        .and_then(|resources| resources.iter().find(|resource| resource["name"] == "dep"))
        .unwrap_or_else(|| panic!("no `resources` entry for `dep`:\n{stdout}"));
    assert_eq!(dep["status"], "blocked", "{stdout}");
    assert_eq!(dep["blocked_by"], json!(["base"]), "{stdout}");
}
