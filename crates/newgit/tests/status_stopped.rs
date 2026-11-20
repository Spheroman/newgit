//! `newgit status` says `stopped` for an instance `newgit stop` took down,
//! not `ready`, and `status --json` lists each resource with its state, as
//! the RESOURCES column does.

mod evict_support;

use evict_support::*;
use serde_json::Value;

fn instance(repo: &camino::Utf8Path) -> Value {
    let run = newgit_ok(repo, &["status", "x", "--json"]);
    let out: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}", run.stdout));
    out["instances"][0].clone()
}

fn state(instance: &Value, resource: &str) -> String {
    instance["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|entry| entry["name"] == resource)
        .unwrap_or_else(|| panic!("no `{resource}` in {instance}"))["state"]
        .as_str()
        .expect("state")
        .to_owned()
}

#[test]
fn a_stopped_instance_reads_stopped_and_its_resources_are_listed() {
    let (_guard, temp) = tempdir("status-stopped");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    std::fs::write(
        store.paths().resources.join("web.toml"),
        "ownership = \"branch\"\n\n[actions.serve]\ncommand = \"exec sleep 600\"\n\
         long_running = true\nautostart = true\n",
    )
    .expect("write resource");
    newgit_ok(&repo, &["spawn", "x"]);

    let up = instance(&repo);
    assert_eq!(up["status"], "ready", "{up}");
    assert_eq!(state(&up, "web"), "running", "{up}");

    newgit_ok(&repo, &["stop", "x"]);
    let down = instance(&repo);
    assert_eq!(down["status"], "stopped", "{down}");
    assert_eq!(state(&down, "web"), "stopped", "{down}");
    assert_eq!(down["problems"], serde_json::json!([]), "{down}");
    let table = newgit_ok(&repo, &["status", "x"]);
    assert!(table.stdout.contains("stopped"), "{}", table.stdout);
    assert!(!table.stdout.contains(" ready "), "{}", table.stdout);

    newgit_ok(&repo, &["start", "x"]);
    let back = instance(&repo);
    assert_eq!(back["status"], "ready", "{back}");
    newgit_ok(&repo, &["remove", "x", "--purge"]);
}
