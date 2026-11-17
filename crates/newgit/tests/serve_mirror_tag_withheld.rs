// A tag made in the project root at a root commit that changed only what the
// mirror does not ship comes back from the mirror as `nothing` (#453): the
// mirror's commit stands for that root commit and for the earlier one whose
// view it first was, and the tag here is at the later one.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

/// GitHub's push event for tag `name` on `jack/mirror` now at `object`.
fn tag_event(name: &str, object: &str, pusher: &str) -> String {
    serde_json::json!({
        "ref": format!("refs/tags/{name}"),
        "after": object,
        "deleted": false,
        "repository": { "full_name": "jack/mirror", "private": false },
        "pusher": { "name": pusher },
    })
    .to_string()
}

fn api(server: &Server, token: &str, path: &str) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store{path}"),
        &[("authorization", bearer.as_str())],
        "",
    )
}

/// Root's commit on `main`.
fn main_root(server: &Server, token: &str) -> String {
    let (status, answer) = api(server, token, "/branches");
    assert_eq!(status, 200, "{answer}");
    answer["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|branch| branch["name"] == "main")
        .and_then(|branch| branch["list"]["root"].as_str())
        .unwrap_or_else(|| panic!("no root on main: {answer}"))
        .to_owned()
}

fn tag(server: &Server, token: &str, name: &str) -> Option<Value> {
    let (status, answer) = api(server, token, &format!("/tags/{name}"));
    (status == 200).then_some(answer)
}

fn tag_ended(state: &Value, name: &str) -> Value {
    state["ended"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|one| one["tag"] == name)
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn a_tag_at_a_root_commit_the_mirror_cannot_tell_apart_comes_back_as_nothing() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    let exported = mirror_main_until(&mirror, |tip| !tip.is_empty());
    let first = main_root(&server, &token);

    // Only `t3code` changes, which the mirror does not ship: a new root
    // commit on `main`, the same view on the mirror.
    std::fs::write(store.join("apps/t3code/app.ts"), "export const t = 2\n").expect("write");
    git(&home, &store, &["commit", "-q", "-am", "t3code only"]);
    newgit(&home, &store, &["merge", "main"]);
    let later = main_root(&server, &token);
    assert_ne!(later, first, "a root commit of its own");

    // Tagged there in the project root, both ways, and pushed: made on the
    // remote at the later root commit, and exported to the mirror naming
    // the commit the mirror already had.
    git(&home, &store, &["tag", "v1"]);
    git(&home, &store, &["tag", "-a", "v2", "-m", "from here"]);
    git(&home, &store, &["push", "-q", "origin", "v1", "v2"]);
    let made = tag(&server, &token, "v1").expect("the push made v1 on the remote");
    assert_eq!(made["list"]["root"], later.as_str(), "{made}");
    let made_v2 = tag(&server, &token, "v2").expect("the push made v2 on the remote");
    let on_mirror = git(&home, &mirror, &["rev-parse", "refs/tags/v1^{commit}"]);
    assert_eq!(on_mirror, exported, "the mirror cannot tell the two apart");

    // GitHub sends the mirror's new tags back: they are there already.
    for name in ["v1", "v2"] {
        let object = git(&home, &mirror, &["rev-parse", &format!("refs/tags/{name}")]);
        let (status, answer) = deliver(
            &server,
            "push",
            &format!("d-{name}"),
            &tag_event(name, &object, "jack"),
        );
        assert_eq!(status, 202, "{answer}");
    }
    let done = pulls_done(&server, &token);
    assert_eq!(tag_ended(&done, "v1")["outcome"], "nothing", "{done}");
    assert_eq!(tag_ended(&done, "v2")["outcome"], "nothing", "{done}");
    assert_eq!(tag(&server, &token, "v1").expect("v1"), made, "untouched");
    assert_eq!(
        tag(&server, &token, "v2").expect("v2"),
        made_v2,
        "untouched"
    );
    let status = newgit(&home, &store, &["status"]);
    assert!(!status.contains("was refused"), "{status}");
}
