// Merges built by hand over `newgit serve`'s API, so one can be begun from a
// base the branch has since moved on from: the server's own merge (#411).
// `include!`d after `support/serve.rs`.

use std::collections::BTreeMap;

type List = BTreeMap<String, String>;

/// `server` started with faults read from `faults` (`delay-merge-by-<login>`
/// holds that login's merge between its merge and its recheck).
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn start_with_faults(temp: &Utf8Path, faults: &Utf8Path) -> Server {
    let data = temp.join("serve");
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args([
            "serve",
            "--data",
            data.as_str(),
            "--listen",
            "127.0.0.1:0",
            "--allow",
            "*",
        ])
        .env("NEWGIT_SERVE_DEV_LOGIN_SECRET", SECRET)
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("newgit serve starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("it says where it listens");
    let url = line.trim().rsplit(' ').next().expect("a URL").to_owned();
    Server { child, url, data }
}

/// A project `jack/store` whose `blog` is public and whose root (members)
/// holds `README.md`, `notes.md`, and `docs/guide.md`, merged once.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn merge_project(temp: &Utf8Path, home: &Utf8Path, server: &Server) -> Utf8PathBuf {
    let store = temp.join("store");
    std::fs::create_dir_all(store.join("blog")).expect("mkdir");
    std::fs::create_dir_all(store.join("docs")).expect("mkdir");
    git(home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "one\ntwo\nthree\n").expect("write");
    std::fs::write(
        store.join("notes.md"),
        "first line\nsecond line\nthird line\nfourth line\nfifth line\n",
    )
    .expect("write");
    std::fs::write(store.join("docs/guide.md"), "how to\n").expect("write");
    std::fs::write(store.join("blog/post.md"), "hello\n").expect("write");
    git(home, &store, &["add", "."]);
    git(home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(home, &store, &["add", "-A"]);
    git(home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(home, &store, &["remote", "add", "--url", &server.url]);
    git(home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        home,
        &store,
        &[
            "tracker", "create", "blog", "--git", "--audience", "public", "--path", "blog",
        ],
    );
    newgit(home, &store, &["merge", "main"]);
    store
}

/// A branch's list, as the API answers it.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn list_of(server: &Server, token: &str, branch: &str) -> List {
    let bearer = format!("Bearer {token}");
    let (status, found) = http(
        &server.url,
        "GET",
        &format!("/v1/projects/jack/store/branches/{branch}"),
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{found}");
    serde_json::from_value(found["list"].clone()).expect("a list")
}

/// The project as the API answers it.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn project_json(server: &Server, token: &str) -> Value {
    let bearer = format!("Bearer {token}");
    let (status, found) = http(
        &server.url,
        "GET",
        "/v1/projects/jack/store",
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{found}");
    found
}

/// A clone of tracker `tracker`'s repo, read as `token`'s caller.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn clone_tracker(server: &Server, token: &str, temp: &Utf8Path, tracker: &str, name: &str) -> Utf8PathBuf {
    let dir = temp.join(name);
    let url = format!("{}/_git/jack.store.t.{tracker}.git", server.url);
    let cloned = git_as(token, temp, &["clone", "-q", &url, dir.as_str()]);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    dir
}

/// A commit on `parent` in clone `dir`: each file written (or removed, for
/// `None`), each pin set, with `message`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn commit_on(
    home: &Utf8Path,
    dir: &Utf8Path,
    parent: &str,
    files: &[(&str, Option<&str>)],
    pins: &[(&str, &str)],
    message: &str,
) -> String {
    git(home, dir, &["checkout", "-q", "--detach", parent]);
    for (path, content) in files {
        let at = dir.join(path);
        match content {
            Some(content) => {
                if let Some(parent) = at.parent() {
                    std::fs::create_dir_all(parent).expect("mkdir");
                }
                std::fs::write(&at, content).expect("write");
                git(home, dir, &["add", path]);
            }
            None => {
                git(home, dir, &["rm", "-q", "-r", path]);
            }
        }
    }
    for (name, commit) in pins {
        git(
            home,
            dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{commit},.newgit/pins/{name}"),
            ],
        );
    }
    git(home, dir, &["commit", "-q", "-m", message]);
    git(home, dir, &["rev-parse", "HEAD"])
}

/// One tracker's new commit, and the clone that has it.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
struct Sent<'a> {
    tracker: &'a str,
    dir: &'a Utf8Path,
    commit: String,
}

/// Merge `list` onto `base` as `token`'s caller: begin, push each sent
/// commit to its landing ref with the password begin hands out, and finish
/// with `moves`. The finish's status and answer; begin's when it refuses.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn merge_by_hand(
    server: &Server,
    token: &str,
    branch: &str,
    base: &List,
    list: &List,
    sent: &[Sent<'_>],
    moves: Value,
) -> (u16, Value) {
    let (status, begun) = begin_by_hand(server, token, branch, base, list, sent);
    if status != 200 {
        return (status, begun);
    }
    finish_by_hand(server, token, branch, base, list, moves)
}

/// A merge begun, and each sent commit pushed to its landing ref with the
/// password begin hands out: begin's status and answer.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn begin_by_hand(
    server: &Server,
    token: &str,
    branch: &str,
    base: &List,
    list: &List,
    sent: &[Sent<'_>],
) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    let auth = [("authorization", bearer.as_str())];
    let (status, begun) = http(
        &server.url,
        "POST",
        &format!("/v1/projects/jack/store/branches/{branch}/merge"),
        &auth,
        &serde_json::json!({ "base": base, "list": list, "uploaded": false }).to_string(),
    );
    if status != 200 {
        return (status, begun);
    }
    for one in sent {
        let credential = begun["credentials"]
            .as_array()
            .and_then(|all| all.iter().find(|found| found["tracker"] == one.tracker))
            .unwrap_or_else(|| panic!("a password for {}: {begun}", one.tracker))
            .clone();
        let header = format!(
            "http.extraHeader=Authorization: Bearer {}",
            credential["password"].as_str().expect("password")
        );
        let pushed = run(
            "git",
            one.dir,
            one.dir,
            &[
                "-c",
                "credential.helper=",
                "-c",
                &header,
                "push",
                "-q",
                credential["remote"].as_str().expect("remote"),
                &format!("{}:refs/newgit/landing/{}", one.commit, one.commit),
            ],
        );
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
    }
    (status, begun)
}

/// The finish of a merge begun and sent already.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn finish_by_hand(
    server: &Server,
    token: &str,
    branch: &str,
    base: &List,
    list: &List,
    moves: Value,
) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    http(
        &server.url,
        "PUT",
        &format!("/v1/projects/jack/store/branches/{branch}"),
        &[("authorization", bearer.as_str())],
        &serde_json::json!({ "base": base, "list": list, "moves": moves }).to_string(),
    )
}

/// `git` in the bare store at `dir`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn in_store(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    let mut all = vec!["--git-dir", dir.as_str()];
    all.extend_from_slice(args);
    git(home, dir, &all)
}

#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn store_has(dir: &Utf8Path, object: &str) -> bool {
    run(
        "git",
        dir,
        dir,
        &["--git-dir", dir.as_str(), "cat-file", "-e", object],
    )
    .status
    .success()
}

/// A commit's parents, read from the store at `dir`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn parents_of(home: &Utf8Path, dir: &Utf8Path, commit: &str) -> Vec<String> {
    in_store(home, dir, &["rev-list", "--parents", "-n", "1", commit])
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect()
}

/// File `path` at `commit`, read from the store at `dir`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn file_at(home: &Utf8Path, dir: &Utf8Path, commit: &str, path: &str) -> Option<String> {
    let output = run(
        "git",
        dir,
        dir,
        &[
            "--git-dir",
            dir.as_str(),
            "cat-file",
            "blob",
            &format!("{commit}:{path}"),
        ],
    );
    let _ = home;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
