//! What an encrypted tracker (#338) does not take from the remote's word:
//! a definition now saying `public` does not turn encryption off until the
//! developer confirms it on their machine, and the remote does not decide
//! who `user` is. And a fresh clone pulls a tracker's `main` first.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

fn env_repo(fixture: &Fixture) -> Utf8PathBuf {
    fixture.remote.repo("jack.store.t.env")
}

/// `env` (project-devs, synced) owning `.env.shared`, and `feature-a` with
/// a secret in it, checkpointed.
fn checkpointed(fixture: &Fixture) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    workspace
}

fn second_machine(fixture: &Fixture) -> Utf8PathBuf {
    let other = fixture.temp.join("machine-b");
    fixture.ok(
        &fixture.temp,
        &[
            "clone",
            "jack/store",
            other.as_str(),
            "--url",
            &fixture.remote.url,
        ],
    );
    other
}

fn all(output: &Output) -> String {
    format!(
        "status={:?}\n--stdout--\n{}\n--stderr--\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Whether any object in `repo` holds `needle` in plaintext.
fn in_plaintext(repo: &Utf8Path, needle: &str) -> bool {
    if !repo.exists() {
        return false;
    }
    let objects = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    objects.lines().any(|object| {
        String::from_utf8_lossy(&git_output(repo, &["cat-file", "-p", object]).stdout)
            .contains(needle)
    })
}

/// The definition of `env` changed to `public` (as it would arrive through
/// `access` or `merge main`): nothing is captured or sent in plaintext
/// until `newgit tracker allow-plaintext env` on this machine.
#[test]
fn a_definition_turned_public_is_refused_until_confirmed_here() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    let definition = fixture.store.join(".newgit/trackers/env.toml");
    let text = std::fs::read_to_string(&definition).expect("definition");
    let public = if text.contains("audience") {
        text.replace("audience = \"project-devs\"", "audience = \"public\"")
    } else {
        format!("audience = \"public\"\n{text}")
    };
    assert!(public.contains("audience = \"public\""), "{public}");
    std::fs::write(&definition, public).expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env is public"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    // The workspace takes the new definition; whether `pull` itself is
    // refused depends on whether it captures, which this test leaves open.
    let _ = fixture.newgit(&workspace, &["pull"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=rotated\n").expect("write");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(!checkpoint.status.success(), "{}", all(&checkpoint));
    let said = all(&checkpoint);
    assert!(said.contains("`env`"), "{said}");
    assert!(said.contains("audience = \"public\""), "{said}");
    assert!(
        said.contains("newgit tracker allow-plaintext env"),
        "{said}"
    );
    let capture = fixture.newgit(&workspace, &["tracker", "capture", "env"]);
    assert!(!capture.status.success(), "{}", all(&capture));
    assert!(
        !in_plaintext(&env_repo(&fixture), "rotated"),
        "nothing of it in plaintext on the remote"
    );

    let allowed = fixture.ok(&workspace, &["tracker", "allow-plaintext", "env"]);
    assert!(allowed.contains("plaintext"), "{allowed}");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
    assert!(
        in_plaintext(&env_repo(&fixture), "API_KEY=rotated"),
        "confirmed: stored as its definition says"
    );
}

/// A tracker this machine never encrypted is not held back.
#[test]
fn a_public_tracker_never_encrypted_needs_no_confirmation() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "site",
            "--storage",
            "remote",
            "--audience",
            "public",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "site", "index.html"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "site"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "web"]);
    let workspace = workspace_of(&fixture, "web");
    std::fs::write(workspace.join("index.html"), "<h1>hi</h1>\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    let said = fixture.ok(&workspace, &["tracker", "allow-plaintext", "site"]);
    assert!(said.contains("plaintext"), "{said}");
    let env = fixture.newgit(&fixture.store, &["tracker", "allow-plaintext", "README"]);
    assert!(!env.status.success(), "no such tracker: {}", all(&env));
}

/// A remote that answers `GET /v1/me` with another login does not get a
/// per-user tracker encrypted to that login's keys.
#[test]
fn a_remote_lying_about_who_you_are_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    publish_ssh_key(&temp, "eve", &temp.join("eve/.ssh/id_ed25519"));
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "mine",
            "--audience",
            "user",
            "--storage",
            "remote",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "mine", ".env.mine"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "mine"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "solo"]);
    let solo = workspace_of(&fixture, "solo");

    fixture.remote.state.lock().expect("state").me_as = Some("eve".to_owned());
    std::fs::write(solo.join(".env.mine"), SECRET).expect("write");
    let capture = fixture.newgit(&solo, &["tracker", "capture", "mine"]);
    assert!(!capture.status.success(), "{}", all(&capture));
    let said = all(&capture);
    assert!(said.contains("`eve`"), "{said}");
    assert!(said.contains("logged in as `jack`"), "{said}");

    // Answering truthfully again, it is encrypted to jack.
    fixture.remote.state.lock().expect("state").me_as = None;
    let capture = fixture.newgit(&solo, &["tracker", "capture", "mine"]);
    assert!(capture.status.success(), "{}", all(&capture));
    assert!(String::from_utf8_lossy(&capture.stderr).contains("jack"));
}

/// On a fresh clone, spawn brings the tracker's `main` from the remote, and
/// `newgit tracker pull` brings a `main` merged since.
#[test]
fn a_fresh_clone_spawns_with_main_and_pulls_first() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    let other = second_machine(&fixture);
    fixture.ok(&other, &["spawn", "b"]);
    let b = Utf8PathBuf::from(fixture.ok(&other, &["status", "b", "--path"]).trim());
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("spawned with main"),
        SECRET
    );

    // Merged on the first machine since.
    std::fs::write(workspace.join(".env.shared"), "API_KEY=two\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let pulled = fixture.newgit(&b, &["tracker", "pull", "env"]);
    assert!(pulled.status.success(), "{}", all(&pulled));
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("read"),
        "API_KEY=two\n"
    );
}

/// A merge refused because the remote's `main` moved says that pulling
/// replaces this instance's files, and which rev keeps its version and how
/// to get it back; that command does get it back.
#[test]
fn a_refused_merge_says_what_the_pull_replaces_and_how_to_get_it_back() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    let other = second_machine(&fixture);
    fixture.ok(&other, &["spawn", "b"]);
    let b = Utf8PathBuf::from(fixture.ok(&other, &["status", "b", "--path"]).trim());
    std::fs::write(b.join(".env.shared"), "API_KEY=b\n").expect("write");
    fixture.ok(&b, &["checkpoint"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=a\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    let merge = fixture.newgit(&b, &["tracker", "merge", "env"]);
    assert!(!merge.status.success(), "{}", all(&merge));
    let said = String::from_utf8_lossy(&merge.stderr).into_owned();
    assert!(said.contains("newgit tracker pull env"), "{said}");
    assert!(said.contains("REPLACES"), "{said}");
    let rev = said
        .split("Your version stays at ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .expect("names the rev")
        .to_owned();
    assert!(
        said.contains(&format!("newgit tracker checkout env --rev {rev}")),
        "{said}"
    );

    fixture.ok(&b, &["tracker", "pull", "env"]);
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("read"),
        "API_KEY=a\n"
    );
    fixture.ok(&b, &["tracker", "checkout", "env", "--rev", &rev]);
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("read"),
        "API_KEY=b\n"
    );
}
