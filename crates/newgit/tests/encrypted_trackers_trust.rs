//! What an encrypted tracker (#338) must not trust, and must not overwrite.
//!
//! Recipients are computed from the remote's member list and the keys
//! GitHub publishes, never read back from a rev: a remote that adds an
//! attacker's key to a rev's `.newgit/recipients` gets nothing encrypted to
//! it. And publishing `main` never overwrites what the publisher has not
//! seen: membership changed on another machine is picked up before anything
//! is sent, and a `main` merged elsewhere is not forced over.

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
    // Committed and merged, so a clone of the project has it too.
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    workspace
}

/// Make `reference` in the bare `repo` a commit on top of it whose record
/// adds `line`: what a remote that wants to read the secret would serve.
/// Returns the new commit.
fn forge_record(repo: &Utf8Path, reference: &str, line: &str) -> String {
    let record = git(repo, &["show", &format!("{reference}:.newgit/recipients")]);
    let forged = format!("{record}\n{line}\n");
    let index = repo.join("forge-index");
    let run = |args: &[&str], stdin: Option<&str>| -> String {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg(format!("--git-dir={repo}"))
            .args(args)
            .env("GIT_INDEX_FILE", index.as_str())
            .env("GIT_AUTHOR_NAME", "mallory")
            .env("GIT_AUTHOR_EMAIL", "m@example.com")
            .env("GIT_COMMITTER_NAME", "mallory")
            .env("GIT_COMMITTER_EMAIL", "m@example.com")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        if let Some(stdin) = stdin {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(stdin.as_bytes())
                .expect("write");
        }
        let output = child.wait_with_output().expect("git");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let blob = run(&["hash-object", "-w", "--stdin"], Some(&forged));
    run(&["read-tree", reference], None);
    run(
        &[
            "update-index",
            "--cacheinfo",
            &format!("100644,{blob},.newgit/recipients"),
        ],
        None,
    );
    let tree = run(&["write-tree"], None);
    let commit = run(
        &[
            "commit-tree",
            &tree,
            "-p",
            reference,
            "-m",
            "encrypted to its audience",
        ],
        None,
    );
    let _ = std::fs::remove_file(&index);
    commit
}

/// An attacker's age key, and the record line that names it the way an
/// instance key was once named.
fn attacker() -> (age::x25519::Identity, String) {
    let identity = age::x25519::Identity::generate();
    let line = format!("instance:feature-a {}", identity.to_public());
    (identity, line)
}

/// Every age file in the remote repo: none of them opens with `identity`.
fn assert_unreadable_to(repo: &Utf8Path, identity: &age::x25519::Identity) {
    let objects = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    let mut sealed = 0;
    for line in objects.lines() {
        let Some((oid, "blob")) = line.split_once(' ') else {
            continue;
        };
        let bytes = git_output(repo, &["cat-file", "blob", oid]).stdout;
        if !bytes.starts_with(b"age-encryption.org/v1\n") {
            assert!(
                !String::from_utf8_lossy(&bytes).contains("hunter"),
                "plaintext on the remote"
            );
            continue;
        }
        sealed += 1;
        let decryptor = age::Decryptor::new(&bytes[..]).expect("an age file");
        assert!(
            decryptor
                .decrypt(std::iter::once(identity as &dyn age::Identity))
                .is_err(),
            "blob {oid} is encrypted to the attacker's key"
        );
        for stanza in String::from_utf8_lossy(&bytes).lines() {
            assert!(!stanza.starts_with("-> X25519"), "{oid}: {stanza}");
        }
    }
    assert!(sealed > 0, "there was ciphertext to check");
}

#[test]
fn members_add_never_encrypts_to_a_key_the_remote_wrote_into_main() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    // The remote serves a `main` whose record names the attacker.
    let repo = env_repo(&fixture);
    let (identity, line) = attacker();
    let forged = forge_record(&repo, "refs/heads/main", &line);
    git(&repo, &["update-ref", "refs/heads/main", &forged]);

    publish_ssh_key(&temp, "carol", &temp.join("carol/.ssh/id_ed25519"));
    let added = fixture.ok(&fixture.store, &["members", "add", "carol"]);
    assert!(added.contains("main re-encrypted at"), "{added}");
    assert!(!added.contains("instance:"), "{added}");

    let main = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(main, forged, "re-encrypted on top of what was served");
    let record = git(&repo, &["show", "refs/heads/main:.newgit/recipients"]);
    assert!(!record.contains("age1"), "{record}");
    assert!(record.contains("user:carol "), "{record}");
    assert_unreadable_to(&repo, &identity);
}

#[test]
fn spawn_from_checkpoint_never_encrypts_to_a_key_the_remote_wrote_into_its_rev() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    checkpointed(&fixture);

    // The remote serves the checkpoint with a rev of its own making: the
    // same files, a record naming the attacker.
    let repo = env_repo(&fixture);
    let (identity, line) = attacker();
    let pinned = {
        let state = fixture.remote.state.lock().expect("state");
        let record = state
            .checkpoints
            .get(&("feature-a".to_owned(), "ckpt_001".to_owned()))
            .expect("synced");
        record["tracker_states"]
            .as_array()
            .expect("states")
            .iter()
            .find(|state| state["name"] == "env")
            .and_then(|state| state["content_rev"].as_str())
            .expect("env's rev")
            .to_owned()
    };
    let forged = forge_record(&repo, &format!("refs/trackers/env/pins/{pinned}"), &line);
    let forged_rev = forged[..pinned.len()].to_owned();
    git(
        &repo,
        &[
            "update-ref",
            &format!("refs/trackers/env/pins/{forged_rev}"),
            &forged,
        ],
    );
    {
        let mut state = fixture.remote.state.lock().expect("state");
        let record = state
            .checkpoints
            .get_mut(&("feature-a".to_owned(), "ckpt_001".to_owned()))
            .expect("synced");
        for tracker in record["tracker_states"].as_array_mut().expect("states") {
            if tracker["name"] == "env" {
                tracker["content_rev"] = Value::String(forged_rev.clone());
            }
        }
    }
    only_on_the_remote(&fixture, "feature-a");

    fixture.ok(
        &fixture.store,
        &["spawn", "review", "--from-checkpoint", "feature-a/ckpt_001"],
    );
    let review = workspace_of(&fixture, "review");
    assert_eq!(
        std::fs::read_to_string(review.join(".env.shared")).expect("read"),
        SECRET
    );
    std::fs::write(review.join(".env.shared"), "API_KEY=hunter3\n").expect("write");
    fixture.ok(&review, &["checkpoint"]);

    let sent = git(
        &repo,
        &[
            "show",
            "refs/trackers/env/instances/review:.newgit/recipients",
        ],
    );
    assert!(!sent.contains("age1"), "{sent}");
    assert_unreadable_to(&repo, &identity);
}

#[test]
fn removing_an_instance_leaves_the_remotes_main_alone() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    // And something of its own, never merged, that this machine decrypted.
    std::fs::write(workspace.join(".env.shared"), "API_KEY=only-a\n").expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    let cache = fixture.store.join(".newgit/local/tracker-plain");
    let cached = |content: &str| {
        std::fs::read_dir(&cache).map_or(0, |entries| {
            entries
                .flatten()
                .filter(|entry| {
                    std::fs::read(entry.path()).is_ok_and(|bytes| bytes == content.as_bytes())
                })
                .count()
        })
    };
    assert_eq!(cached("API_KEY=only-a\n"), 1);
    let repo = env_repo(&fixture);
    let before = git(&repo, &["rev-parse", "refs/heads/main"]);
    let requests = fixture.remote.requests().len();

    let removed = fixture.ok(&fixture.store, &["remove", "feature-a"]);
    assert!(!removed.contains("warning"), "{removed}");
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), before);
    assert!(
        !fixture.remote.requests()[requests..]
            .iter()
            .any(|(_, path, _)| path.contains("t.env.git/git-receive-pack")),
        "nothing pushed to env"
    );
    // And this machine keeps no plaintext of what only it held; `main`'s
    // stays cached.
    assert_eq!(cached("API_KEY=only-a\n"), 0);
    assert_eq!(cached(SECRET), 1);
}

/// A second machine of jack's: a `newgit clone` of the project.
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

/// Whether the private key at `private` opens the age file `bytes`.
fn opens(private: &Utf8Path, bytes: &[u8]) -> bool {
    let text = std::fs::read_to_string(private).expect("key");
    let identity = age::ssh::Identity::from_buffer(text.as_bytes(), None).expect("ssh key");
    let decryptor = age::Decryptor::new(bytes).expect("an age file");
    decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .is_ok()
}

#[test]
fn a_member_removed_on_another_machine_is_out_of_this_ones_next_checkpoint_and_merge() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let carol_key = temp.join("carol/.ssh/id_ed25519");
    publish_ssh_key(&temp, "carol", &carol_key);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    fixture.ok(&fixture.store, &["members", "add", "carol"]);
    // An instance bound to `main` as encrypted to carol too.
    fixture.ok(&fixture.store, &["spawn", "feature-b"]);
    let feature_b = workspace_of(&fixture, "feature-b");
    let repo = env_repo(&fixture);
    assert!(opens(
        &carol_key,
        &git_output(&repo, &["cat-file", "blob", "refs/heads/main:.env.shared"]).stdout
    ));

    // On the other machine, carol is removed.
    let other = second_machine(&fixture);
    let removed = fixture.ok(&other, &["members", "remove", "carol"]);
    assert!(removed.contains("main re-encrypted at"), "{removed}");
    let reencrypted = git(&repo, &["rev-parse", "refs/heads/main"]);

    // This machine never heard. Its next checkpoint is not for carol.
    std::fs::write(feature_b.join(".env.shared"), "API_KEY=rotated\n").expect("write");
    fixture.ok(&feature_b, &["checkpoint"]);
    let sent = git_output(
        &repo,
        &[
            "cat-file",
            "blob",
            "refs/trackers/env/instances/feature-b:.env.shared",
        ],
    )
    .stdout;
    assert!(!opens(&carol_key, &sent), "the checkpoint is for carol");
    assert!(opens(&temp.join("home/.ssh/id_ed25519"), &sent));

    // Nor is its merge, which keeps the other machine's re-encryption
    // rather than forcing over it with a `main` for carol.
    fixture.ok(&feature_b, &["tracker", "merge", "env"]);
    let main = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(main, reencrypted);
    let published = git_output(&repo, &["cat-file", "blob", "refs/heads/main:.env.shared"]).stdout;
    assert!(!opens(&carol_key, &published), "main is for carol again");
    let record = git(&repo, &["show", "refs/heads/main:.newgit/recipients"]);
    assert!(!record.contains("user:carol"), "{record}");
}

#[test]
fn a_merge_never_overwrites_a_main_merged_elsewhere_it_has_not_seen() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    fixture.ok(&fixture.store, &["spawn", "feature-b"]);
    let feature_b = workspace_of(&fixture, "feature-b");

    // The other machine merges a change first.
    let other = second_machine(&fixture);
    fixture.ok(&other, &["spawn", "elsewhere"]);
    let elsewhere = Utf8PathBuf::from(
        fixture
            .ok(&other, &["status", "elsewhere", "--path"])
            .trim(),
    );
    let repo = env_repo(&fixture);
    let first = git(&repo, &["rev-parse", "refs/heads/main"]);
    // A fresh clone has never seen `main` either: the same refusal, and
    // `main` comes here for `tracker pull`.
    std::fs::write(elsewhere.join(".env.shared"), "API_KEY=blind\n").expect("write");
    fixture.ok(&elsewhere, &["checkpoint"]);
    let blind = fixture.newgit(&elsewhere, &["tracker", "merge", "env"]);
    assert!(!blind.status.success());
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), first);
    fixture.ok(&elsewhere, &["tracker", "pull", "env"]);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join(".env.shared")).expect("read"),
        SECRET
    );
    std::fs::write(elsewhere.join(".env.shared"), "API_KEY=theirs\n").expect("write");
    fixture.ok(&elsewhere, &["checkpoint"]);
    fixture.ok(&elsewhere, &["tracker", "merge", "env"]);
    let theirs = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(theirs, first);

    // This one, from what it had, is refused, and the remote keeps theirs.
    std::fs::write(feature_b.join(".env.shared"), "API_KEY=mine\n").expect("write");
    fixture.ok(&feature_b, &["checkpoint"]);
    let output = fixture.newgit(&feature_b, &["tracker", "merge", "env"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("newgit tracker pull env"), "{stderr}");
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), theirs);

    // Pulling brings theirs in; then the merge goes.
    fixture.ok(&feature_b, &["tracker", "pull", "env"]);
    assert_eq!(
        std::fs::read_to_string(feature_b.join(".env.shared")).expect("read"),
        "API_KEY=theirs\n"
    );
    std::fs::write(feature_b.join(".env.shared"), "API_KEY=mine\n").expect("write");
    fixture.ok(&feature_b, &["checkpoint"]);
    fixture.ok(&feature_b, &["tracker", "merge", "env"]);
    let main = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_eq!(
        git(&repo, &["merge-base", "--is-ancestor", &theirs, &main]),
        "",
        "theirs is in the new main"
    );
}

#[test]
fn a_per_user_main_merged_without_a_remote_is_refused_at_checkpoint_sync() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
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

    // Merged while the project had no remote: plaintext, here only.
    let config = fixture.store.join(".newgit/config.toml");
    let linked = std::fs::read_to_string(&config).expect("config");
    let start = linked.find("[remote]").expect("linked");
    let end = linked[start + 1..]
        .find("\n[")
        .map_or(linked.len(), |at| start + 1 + at);
    std::fs::write(&config, format!("{}{}", &linked[..start], &linked[end..])).expect("unlink");
    fixture.ok(&fixture.store, &["spawn", "solo"]);
    let solo = workspace_of(&fixture, "solo");
    std::fs::write(solo.join(".env.mine"), SECRET).expect("write");
    fixture.ok(&solo, &["tracker", "capture", "mine"]);
    fixture.ok(&solo, &["tracker", "merge", "mine"]);
    std::fs::write(&config, &linked).expect("relink");

    let output = fixture.newgit(&solo, &["checkpoint"]);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(all.contains("not on the remote"), "{all}");
    assert!(all.contains("in plaintext"), "{all}");
    let repo = fixture.remote.repo("jack.store.t.mine.jack");
    if repo.exists() {
        let objects = git(
            &repo,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        );
        for object in objects.lines() {
            assert!(
                !String::from_utf8_lossy(&git_output(&repo, &["cat-file", "-p", object]).stdout)
                    .contains("hunter2"),
                "plaintext reached the remote"
            );
        }
    }
}
