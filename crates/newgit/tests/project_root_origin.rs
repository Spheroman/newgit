//! A linked project root's `origin` is the remote's Git endpoint, as a
//! workspace's is: a mirror that withholds a tracker has ids of its own, so
//! raw GitHub could no longer be fetched. `git pull` brings in what landed.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn endpoint(fixture: &Fixture) -> String {
    let project = fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .keys()
        .next()
        .expect("a project")
        .clone();
    format!("{}/{project}.git", fixture.remote.url)
}

/// Run git in `dir` the way someone with newgit installed does: the
/// credential helper finds `newgit` by name, and their login under `HOME`.
fn git_as_user(fixture: &Fixture, dir: &Utf8Path, args: &[&str]) -> Output {
    let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
    let path = format!(
        "{}:{}",
        binary.parent().expect("a directory"),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("HOME", fixture.temp.join("home").as_str())
        .env("PATH", path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

#[test]
fn remote_add_makes_origin_the_endpoint() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    assert_eq!(
        git(&fixture.store, &["remote", "get-url", "origin"]),
        endpoint(&fixture)
    );
    assert_eq!(
        git(&fixture.store, &["config", "branch.main.remote"]),
        "origin",
        "a bare `git pull` knows where to go"
    );
}

#[test]
fn a_project_linked_before_this_is_moved_when_newgit_next_touches_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    git(
        &fixture.store,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/widget.git",
        ],
    );
    git(
        &fixture.store,
        &[
            "config",
            "remote.origin.pushurl",
            "https://github.com/acme/widget.git",
        ],
    );
    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_eq!(
        git(&fixture.store, &["remote", "get-url", "origin"]),
        endpoint(&fixture)
    );
    assert_eq!(
        git(&fixture.store, &["remote", "get-url", "--push", "origin"]),
        fixture
            .store
            .join(".newgit/local/publish/outbox.git")
            .as_str(),
        "a push no longer goes straight to the mirror"
    );
}

#[test]
fn origin_fetches_from_the_endpoint_with_the_login() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let fetched = git_as_user(&fixture, &fixture.store, &["fetch", "origin"]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(
        git(&fixture.store, &["rev-parse", "origin/main"]),
        git(&fixture.store, &["rev-parse", "HEAD"]),
        "the endpoint serves the project's own commits"
    );
}

#[test]
fn a_branches_target_is_a_remote_for_gh_and_refuses_a_straight_push() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let mirror = temp.join("mirror.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    let dir = fixture.store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("github.toml"),
        format!("url = \"{mirror}\"\ntrackers = [\"root\", \"access\"]\nbranches = true\n"),
    )
    .expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "target"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    assert_eq!(
        git(&fixture.store, &["remote", "get-url", "github"]),
        mirror.as_str()
    );
    assert_eq!(
        git(&fixture.store, &["config", "remote.github.gh-resolved"]),
        "base"
    );
    let pushed = git_as_user(&fixture, &fixture.store, &["push", "github", "main"]);
    assert!(!pushed.status.success());
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(stderr.contains("`git push origin main`"), "{stderr}");
    assert!(
        !git_output(&mirror, &["rev-parse", "--verify", "main"])
            .status
            .success(),
        "nothing reached it"
    );
}

#[test]
fn git_pull_brings_in_a_change_that_landed_on_the_mirror() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let store = &fixture.store;

    // A tracker the mirror does not ship, so its ids are not the project's.
    let upstream = temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
    fixture.ok(
        store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--mount",
            "apps/t3code",
            "--from",
            upstream.as_str(),
        ],
    );
    let mirror = temp.join("mirror.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    let dir = store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("github.toml"),
        format!("url = \"{mirror}\"\ntrackers = [\"root\", \"access\"]\n"),
    )
    .expect("write");
    git(store, &["add", "-A"]);
    git(store, &["commit", "-q", "-m", "target"]);
    fixture.ok(store, &["merge", "main"]);
    fixture.ok(store, &["export", "--remote", "github"]);

    // A pull request merged on the mirror.
    let clone = temp.join("pr");
    git(&temp, &["clone", "-q", mirror.as_str(), clone.as_str()]);
    std::fs::write(clone.join("pr.txt"), "pr\n").expect("write");
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "from GitHub"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);

    // The webhook's work: the container lands it, the Worker moves the list.
    let project = endpoint(&fixture)
        .trim_start_matches(&format!("{}/", fixture.remote.url))
        .trim_end_matches(".git")
        .to_owned();
    let list = fixture.remote.state.lock().expect("state").branches[&project]["main"].clone();
    let prefix = project.replace('/', ".");
    let mut repos = serde_json::Map::new();
    for tracker in list.as_object().expect("a list").keys() {
        repos.insert(
            tracker.clone(),
            json!({
                "url": fixture.remote.repos.join(format!("{prefix}.t.{tracker}.git")),
                "auth": "Authorization: Basic eDpw",
            }),
        );
    }
    let plan = json!({
        "dir": temp.join("container"),
        "repos": repos,
        "list": list,
        "writable": ["root", "access"],
        "target": "github",
        "mirror": { "url": mirror, "branch": "main", "commit": theirs },
    });
    let mut pull = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .arg("endpoint-pull")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("newgit runs");
    pull.stdin
        .take()
        .expect("stdin")
        .write_all(plan.to_string().as_bytes())
        .expect("write the plan");
    let pulled = pull.wait_with_output().expect("pull");
    assert!(pulled.status.success());
    let answer: Value = serde_json::from_slice(&pulled.stdout).expect("json");
    assert_eq!(answer["outcome"], "landed", "{answer}");
    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .branches
        .get_mut(&project)
        .expect("project")
        .insert("main".to_owned(), answer["list"].clone());
    // And, as the Worker does once the list lands, root's `main` follows.
    git(
        &fixture.remote.repo("jack.store.t.root"),
        &[
            "update-ref",
            "refs/heads/main",
            answer["list"]["root"].as_str().expect("root"),
        ],
    );

    let before = git(store, &["rev-parse", "HEAD"]);
    let output = git_as_user(&fixture, store, &["pull", "--ff-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.join("pr.txt").is_file(), "the change came down");
    assert!(
        store.join("apps/t3code/app.ts").is_file(),
        "the mount stayed"
    );
    assert_ne!(git(store, &["rev-parse", "HEAD"]), before);
    assert_ne!(
        git(store, &["rev-parse", "HEAD"]),
        theirs,
        "the project's commit, not the mirror's"
    );
    // And `newgit` agrees with what was pulled: nothing to split or publish.
    fixture.ok(store, &["merge", "main"]);
    fixture.ok(store, &["export", "--remote", "github"]);
    assert_eq!(git(&mirror, &["rev-parse", "main"]), theirs);
}
