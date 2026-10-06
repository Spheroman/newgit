//! A project linked to a newgit remote, through the real binary. The remote
//! is a fake on 127.0.0.1 that speaks the Worker's JSON API, with local bare
//! repositories standing in for Artifacts repos — so the binary's own HTTP
//! client, its credential handling, and the out-of-process push hook are all
//! exercised. `HOME` is a temp directory, so no real login is ever read.

include!("support/fake_remote.rs");

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn remote_add_creates_the_project_with_a_repo_per_code_tracker_and_main() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let root = fixture.remote.repo("jack.store.t.root");
    let project = fixture.remote.repo("jack.store.t.project");
    assert!(files_at(&root, "main").contains("README.md"));
    assert!(
        !files_at(&root, "main").contains(".newgit"),
        "root holds what no other tracker claims"
    );
    assert!(files_at(&project, "main").contains(".newgit/config.toml"));

    // `main` names each tracker's commit, and they are what was pushed.
    let state = fixture.remote.state.lock().expect("state");
    let list = &state.branches["jack/store"]["main"];
    assert_eq!(list["root"], git(&root, &["rev-parse", "main"]));
    assert_eq!(list["project"], git(&project, &["rev-parse", "main"]));
    drop(state);

    let config =
        std::fs::read_to_string(fixture.store.join(".newgit/config.toml")).expect("config");
    assert!(config.contains("project = \"jack/store\""), "{config}");
}

#[test]
fn spawn_registers_the_instance_forks_nothing_and_gives_the_workspace_its_own_token() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    let token_path = workspace.join(".git/newgit/token");
    assert_eq!(
        std::fs::read_to_string(&token_path).expect("token"),
        fake_token("tok_instance_feature-a")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&token_path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the workspace token is readable by its owner only"
        );
    }
    // Never committed: it lives inside .git/.
    assert!(git(&workspace, &["status", "--porcelain"]).is_empty());

    // `origin` fetches the project's Git endpoint.
    assert_eq!(
        git(&workspace, &["config", "remote.origin.url"]),
        format!("{}/jack/store.git", fixture.remote.url)
    );
    // Nothing forked yet: an instance forks a tracker when it changes it.
    assert!(
        !fixture.remote.repo("jack.store.i.feature-a.root").exists(),
        "spawning forks nothing"
    );
    let record = std::fs::read_to_string(fixture.store.join(".newgit/branches/feature-a.toml"))
        .expect("record");
    assert!(record.contains("project = \"jack/store\""), "{record}");
}

#[test]
fn a_push_forks_the_tracker_it_changed_and_lands_there_as_the_instance() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");

    std::fs::write(workspace.join("change.txt"), "agent work\n").expect("write");
    git(&workspace, &["add", "change.txt"]);
    git(&workspace, &["commit", "-q", "-m", "agent work"]);
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    let push = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(
        push.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // Root changed, so root was forked; the work is there, split from the
    // commit the agent made.
    let fork = fixture.remote.repo("jack.store.i.feature-a.root");
    assert_eq!(split_source(&fork, "refs/heads/feature-a"), head);
    assert!(files_at(&fork, "refs/heads/feature-a").contains("change.txt"));
    assert!(
        !fixture
            .remote
            .repo("jack.store.i.feature-a.project")
            .exists(),
        "the tracker it did not change was not forked"
    );
    let root = fixture.remote.repo("jack.store.t.root");
    assert!(
        !files_at(&root, "main").contains("change.txt"),
        "root's own history moves only by merge"
    );

    // The fork and its password were asked for on the workspace's own token.
    let instance_token = fake_token("tok_instance_feature-a");
    let requests = fixture.remote.requests();
    assert!(requests.iter().any(|(m, p, t)| m == "POST"
        && p.ends_with("/instances/feature-a/trackers/root/fork")
        && *t == instance_token));
    assert!(
        requests
            .iter()
            .any(|(m, p, t)| m == "POST" && p.ends_with("/credentials") && *t == instance_token)
    );
}

#[test]
fn remove_takes_the_instance_and_its_forks_off_the_remote() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join("change.txt"), "agent work\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    assert!(fixture.remote.repo("jack.store.i.feature-a.root").exists());
    fixture.ok(&fixture.store, &["remove", "feature-a"]);
    assert!(
        !fixture.remote.repo("jack.store.i.feature-a.root").exists(),
        "its fork is gone"
    );
    let login = fake_token(LOGIN_TOKEN_ID);
    assert!(
        fixture
            .remote
            .requests()
            .iter()
            .any(|(m, p, t)| m == "DELETE" && p.ends_with("/instances/feature-a") && *t == login)
    );
}

#[test]
fn spawn_still_works_when_the_remote_is_unreachable() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // Point the project at a port nothing listens on.
    let config = fixture.store.join(".newgit/config.toml");
    let text = std::fs::read_to_string(&config)
        .expect("config")
        .replace(&fixture.remote.url, "http://127.0.0.1:9");
    std::fs::write(&config, text).expect("write");
    let output = fixture.newgit(&fixture.store, &["spawn", "offline"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all.contains("not on the remote"),
        "the spawn says so:\n{all}"
    );
}

#[test]
fn a_checkpoint_syncs_changed_trackers_remote_trackers_and_then_the_record() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // One tracker synced to the remote, one kept on this machine.
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "runtime-env",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "scratch",
            "--audience",
            "user",
            "--storage",
            "local",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "scratch", "notes.txt"],
    );
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join(".env.local"), "KEY=jack\n").expect("write");
    std::fs::write(workspace.join("notes.txt"), "private\n").expect("write");
    std::fs::write(workspace.join("wip.txt"), "uncommitted\n").expect("write");

    let output = fixture.ok(&workspace, &["checkpoint"]);
    assert!(!output.contains("not on the remote"), "{output}");

    // The uncommitted work is in root's fork, at the checkpoint's ref — and
    // no secret tracker's file is.
    let fork = fixture.remote.repo("jack.store.i.feature-a.root");
    let files = files_at(&fork, "refs/newgit/checkpoints/ckpt_001");
    assert!(files.contains("wip.txt"), "{files}");
    assert!(!files.contains(".env.local") && !files.contains("notes.txt"));

    // The remote tracker's rev is in jack's own repo for it; the local one is nowhere.
    let lane = fixture.remote.repo("jack.store.t.runtime-env.jack");
    let lane_refs = git(
        &lane,
        &["for-each-ref", "--format=%(refname)", "refs/trackers/"],
    );
    assert!(
        lane_refs.contains("refs/trackers/runtime-env/pins/")
            && lane_refs.contains("refs/trackers/runtime-env/instances/feature-a"),
        "{lane_refs}"
    );
    assert!(
        !temp
            .join("artifacts/jack.store.t.scratch.jack.git")
            .exists(),
        "local trackers stay local"
    );

    // The record names each code tracker's commits, and was written last.
    let state = fixture.remote.state.lock().expect("state");
    let record = &state.checkpoints[&("feature-a".to_owned(), "ckpt_001".to_owned())];
    assert!(record["code"]["tip"]["root"].is_string(), "{record}");
    assert_ne!(
        record["code"]["tip"]["root"],
        record["code"]["head"]["root"]
    );
    drop(state);
    let requests = fixture.remote.requests();
    let put = requests
        .iter()
        .position(|(m, p, _)| m == "PUT" && p.ends_with("/checkpoints/feature-a/ckpt_001"));
    let last_credential = requests
        .iter()
        .rposition(|(_, p, _)| p.ends_with("/credentials"));
    assert!(
        put.is_some() && put > last_credential,
        "record after the refs it names"
    );
}

#[test]
fn merge_moves_the_remote_branch_and_brings_new_trackers_with_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join("feature.txt"), "work\n").expect("write");
    git(&workspace, &["add", "feature.txt"]);
    git(&workspace, &["commit", "-q", "-m", "feature"]);
    let head = git(&workspace, &["rev-parse", "HEAD"]);

    let merged = fixture.ok(&fixture.store, &["merge", "feature-a"]);
    assert!(merged.contains("fast-forward"), "{merged}");
    let root = fixture.remote.repo("jack.store.t.root");
    assert_eq!(
        split_source(&root, "main"),
        head,
        "root's repo took the work"
    );
    let list = fixture.remote.state.lock().expect("state").branches["jack/store"]["main"].clone();
    assert_eq!(
        list["root"],
        git(&root, &["rev-parse", "main"]),
        "and main's list says so"
    );

    // A code tracker carved after `remote add` reaches the remote by merging
    // the project root, which made it.
    std::fs::create_dir_all(fixture.store.join("docs")).expect("mkdir");
    std::fs::write(fixture.store.join("docs/guide.md"), "guide\n").expect("write");
    git(&fixture.store, &["add", "docs"]);
    git(&fixture.store, &["commit", "-q", "-m", "docs"]);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "docs", "--code", "--path", "docs"],
    );
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "docs is a tracker"]);
    let published = fixture.ok(&fixture.store, &["merge", "main"]);
    assert!(published.contains("new"), "{published}");
    let docs = fixture.remote.repo("jack.store.t.docs");
    assert!(files_at(&docs, "main").contains("docs/guide.md"));
    let list = fixture.remote.state.lock().expect("state").branches["jack/store"]["main"].clone();
    assert_eq!(list["docs"], git(&docs, &["rev-parse", "main"]));
}

/// A secret tracker owning `.env.local`, its definition committed.
fn with_env_tracker(fixture: &Fixture) {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--audience", "user"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.local"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env tracker"]);
}

fn force_commit_secret(dir: &Utf8Path) {
    std::fs::write(dir.join(".env.local"), "SECRET=leak\n").expect("write");
    git(dir, &["add", "-f", ".env.local"]);
    git(dir, &["commit", "-q", "-m", "oops"]);
}

fn remote_root_files(fixture: &Fixture) -> String {
    files_at(&fixture.remote.repo("jack.store.t.root"), "main")
}

#[test]
fn a_merge_never_carries_what_the_push_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_env_tracker(&fixture);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    force_commit_secret(&workspace);
    let push = git_output(&workspace, &["push", "-q", "origin", "feature-a"]);
    assert!(!push.status.success(), "the push is refused");

    let merge = fixture.newgit(&fixture.store, &["merge", "feature-a"]);
    assert!(!merge.status.success(), "and so is the merge");
    assert!(!remote_root_files(&fixture).contains(".env.local"));
}

#[test]
fn merging_the_project_root_never_carries_a_secret_it_committed() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_env_tracker(&fixture);
    force_commit_secret(&fixture.store);
    let merge = fixture.newgit(&fixture.store, &["merge", "main"]);
    assert!(!merge.status.success());
    assert!(
        String::from_utf8_lossy(&merge.stderr).contains(".env.local"),
        "{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(!remote_root_files(&fixture).contains(".env.local"));
}

#[test]
fn a_checkpoint_after_a_merge_spawns_elsewhere() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join("feature.txt"), "work\n").expect("write");
    git(&workspace, &["add", "feature.txt"]);
    git(&workspace, &["commit", "-q", "-m", "feature"]);
    fixture.ok(&fixture.store, &["merge", "feature-a"]);
    // Nothing changed since the merge: the fork has no commit for this one.
    fixture.ok(&workspace, &["checkpoint"]);
    let spawned = fixture.ok(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_002"],
    );
    let review = workspace_of(&fixture, "review");
    assert!(review.join("feature.txt").exists(), "{spawned}");
}

#[test]
fn a_merge_brings_the_project_root_along_so_the_next_spawn_has_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join("feature.txt"), "work\n").expect("write");
    git(&workspace, &["add", "feature.txt"]);
    git(&workspace, &["commit", "-q", "-m", "feature"]);
    let merged = fixture.ok(&fixture.store, &["merge", "feature-a"]);
    assert!(merged.contains("fast-forwarded"), "{merged}");
    assert!(fixture.store.join("feature.txt").exists());
    fixture.ok(&fixture.store, &["spawn", "feature-b"]);
    assert!(
        workspace_of(&fixture, "feature-b")
            .join("feature.txt")
            .exists()
    );
}

#[test]
fn remote_add_never_publishes_a_secret_committed_after_init() {
    let (_guard, temp) = tempdir();
    let repos = temp.join("artifacts");
    std::fs::create_dir_all(&repos).expect("mkdir");
    let remote = FakeRemote::start(repos);
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);
    let home = temp.join("home");
    save_login(&home, &remote.url, "jack", LOGIN_TOKEN_ID);
    newgit_as(&home, &store, &["init"]);
    newgit_as(
        &home,
        &store,
        &["tracker", "create", "env", "--audience", "user"],
    );
    newgit_as(&home, &store, &["tracker", "track", "env", ".env.local"]);
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "newgit"]);
    force_commit_secret(&store);
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(store.as_str())
        .args(["remote", "add", "--url", &remote.url])
        .env("HOME", home.as_str())
        .output()
        .expect("newgit runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(".env.local"));
    assert!(
        !temp.join("artifacts/jack.store.t.root.git").exists()
            || !files_at(&temp.join("artifacts/jack.store.t.root.git"), "main")
                .contains(".env.local")
    );
}

/// Run newgit as a given user: their own HOME, so their own saved login.
fn newgit_as(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .env("HOME", home.as_str())
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("newgit runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "newgit {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn save_login(home: &Utf8Path, url: &str, login: &str, token_id: &str) {
    std::fs::create_dir_all(home.join(".newgit")).expect("mkdir");
    std::fs::write(
        home.join(".newgit/auth.toml"),
        format!(
            "[remotes.\"{url}\"]\nlogin = \"{login}\"\ntoken = \"{}\"\ntoken_id = \"{token_id}\"\n",
            fake_token(token_id)
        ),
    )
    .expect("write auth");
}

const DB_RESOURCE: &str = r#"ownership = "branch"

[checkpoint]
mode = "command"
command = "echo jacks-rows > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "cp {{state_ref}} restored.sql"
"#;

#[test]
fn a_teammate_spawns_a_checkpoint_running_with_their_own_keys() {
    let (_guard, temp) = tempdir();
    let remote = FakeRemote::start(temp.join("artifacts"));
    std::fs::create_dir_all(temp.join("artifacts")).expect("mkdir");
    let (jack_home, ana_home) = (temp.join("home-jack"), temp.join("home-ana"));
    save_login(&jack_home, &remote.url, "jack", LOGIN_TOKEN_ID);
    save_login(&ana_home, &remote.url, "ana", ANA_TOKEN_ID);

    // Jack's machine: the project, its trackers and a database resource,
    // committed so a teammate's clone has the same definitions.
    let jack = temp.join("store");
    std::fs::create_dir_all(&jack).expect("mkdir");
    git(&jack, &["init", "-q", "-b", "main"]);
    std::fs::write(jack.join("README.md"), "hello\n").expect("write");
    git(&jack, &["add", "."]);
    git(&jack, &["commit", "-q", "-m", "initial"]);
    newgit_as(&jack_home, &jack, &["init"]);
    let config = jack.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    // Name the remote first, so trackers may be remote-stored; `remote add`
    // below creates the project and seeds trunk with all of this.
    text.push_str(&format!("\n[remote]\nurl = \"{}\"\n", remote.url));
    std::fs::write(&config, text).expect("write config");
    newgit_as(
        &jack_home,
        &jack,
        &[
            "tracker",
            "create",
            "runtime-env",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    newgit_as(
        &jack_home,
        &jack,
        &["tracker", "track", "runtime-env", ".env.local"],
    );
    newgit_as(
        &jack_home,
        &jack,
        &[
            "tracker",
            "create",
            "db-snapshots",
            "--audience",
            "project-devs",
            "--storage",
            "remote",
        ],
    );
    std::fs::write(jack.join(".newgit/resources/db.toml"), DB_RESOURCE).expect("resource");
    git(&jack, &["add", "-A"]);
    git(&jack, &["commit", "-q", "-m", "newgit definitions"]);
    newgit_as(&jack_home, &jack, &["remote", "add"]);

    // Jack's agent works: a commit, uncommitted work, Jack's key, a database.
    newgit_as(&jack_home, &jack, &["spawn", "feature-a"]);
    let jack_ws =
        Utf8PathBuf::from(newgit_as(&jack_home, &jack, &["status", "feature-a", "--path"]).trim());
    std::fs::write(jack_ws.join("feature.txt"), "committed work\n").expect("write");
    git(&jack_ws, &["add", "feature.txt"]);
    git(&jack_ws, &["commit", "-q", "-m", "feature"]);
    let jack_head = git(&jack_ws, &["rev-parse", "HEAD"]);
    std::fs::write(jack_ws.join("wip.txt"), "uncommitted work\n").expect("write");
    std::fs::write(jack_ws.join(".env.local"), "STRIPE_KEY=jacks-key\n").expect("write");
    let checkpointed = newgit_as(&jack_home, &jack_ws, &["checkpoint"]);
    assert!(
        !checkpointed.contains("not on the remote"),
        "{checkpointed}"
    );

    // Ana's machine: a clone composed from the trackers she may read, her
    // own key, never Jack's.
    let ana = temp.join("ana-store");
    newgit_as(
        &ana_home,
        &temp,
        &["clone", "jack/store", ana.as_str(), "--url", &remote.url],
    );
    assert_eq!(
        git(&ana, &["rev-parse", "HEAD"]),
        git(&jack, &["rev-parse", "HEAD"]),
        "the clone is Jack's history, commit for commit"
    );
    std::fs::write(ana.join(".env.local"), "STRIPE_KEY=anas-key\n").expect("write");
    newgit_as(
        &ana_home,
        &ana,
        &["tracker", "capture", "runtime-env", "--from-store"],
    );

    let spawned = newgit_as(
        &ana_home,
        &ana,
        &[
            "spawn",
            "review",
            "--from-checkpoint",
            "feature-a/ckpt_001",
            "--json",
        ],
    );
    let outcome: Value = serde_json::from_str(spawned.trim()).expect("json");
    assert_eq!(outcome["author"], "jack");
    let ana_ws = Utf8PathBuf::from(outcome["workspace"].as_str().expect("workspace"));
    assert_eq!(
        git(&ana_ws, &["rev-parse", "HEAD"]),
        jack_head,
        "the same commit ids on both machines"
    );

    let read = |file: &str| {
        std::fs::read_to_string(ana_ws.join(file)).unwrap_or_else(|_| panic!("{file} missing"))
    };
    assert_eq!(
        read("feature.txt"),
        "committed work\n",
        "source came across"
    );
    assert_eq!(
        read("wip.txt"),
        "uncommitted work\n",
        "uncommitted work came across, still uncommitted"
    );
    assert!(git(&ana_ws, &["status", "--porcelain"]).contains("wip.txt"));
    assert_eq!(
        read(".env.local"),
        "STRIPE_KEY=anas-key\n",
        "Ana runs it with her own key"
    );
    assert_eq!(
        read("restored.sql"),
        "jacks-rows\n",
        "the database came across from its dump"
    );

    // And Jack's key is nowhere on Ana's machine.
    let ana_lanes = ana.join(".newgit/local/trackers.git");
    let objects = git(
        &ana_lanes,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for object in objects.lines() {
        assert!(!git(&ana_lanes, &["cat-file", "-p", object]).contains("jacks-key"));
    }
}
