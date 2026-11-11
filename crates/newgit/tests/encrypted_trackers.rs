//! Trackers outside Git that sync and are not `public` are encrypted to
//! their audience before content leaves the machine (#338): the remote's
//! repos hold age ciphertext and a record of the recipients, the workspace
//! holds plaintext. Keys come from a directory standing in for
//! `https://github.com/<login>.keys`; jack's private key is in `~/.ssh`.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

fn tracker_store(fixture: &Fixture) -> Utf8PathBuf {
    fixture.store.join(".newgit/local/trackers.git")
}

/// `env` (synced, `audience`) owning `.env.shared`, and an instance
/// `feature-a` that has written and checkpointed a secret into it.
fn checkpointed(fixture: &Fixture, audience: &str) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "env",
            "--storage",
            "remote",
            "--audience",
            audience,
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    workspace
}

/// A blob of the remote's `env` repo, as bytes.
fn remote_blob(fixture: &Fixture, spec: &str) -> Vec<u8> {
    let output = git_output(
        &fixture.remote.repo("jack.store.t.env"),
        &["cat-file", "blob", spec],
    );
    assert!(
        output.status.success(),
        "{spec}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn the_remote_holds_ciphertext_and_who_it_is_for_while_the_workspace_holds_plaintext() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");
    // The first encryption says who, by fingerprint.
    let output = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`env` is encrypted to:"), "{stderr}");
    for login in ["ana", "jack"] {
        assert!(
            stderr.contains(&format!("{login}: SHA256:")),
            "{login} by fingerprint:\n{stderr}"
        );
    }

    let branch = "refs/trackers/env/instances/feature-a";
    let sent = remote_blob(&fixture, &format!("{branch}:.env.shared"));
    assert!(sent.starts_with(b"age-encryption.org/v1\n"), "an age file");
    assert!(!contains(&sent, "hunter2"), "no plaintext on the remote");

    // Who it is for, recorded beside it, for people to read.
    let recipients = String::from_utf8(remote_blob(
        &fixture,
        &format!("{branch}:.newgit/recipients"),
    ))
    .expect("utf8");
    for who in ["user:ana ssh-ed25519 ", "user:jack ssh-ed25519 "] {
        assert!(recipients.contains(who), "{who} in:\n{recipients}");
    }
    assert!(!recipients.contains("instance:"), "{recipients}");
    let jack = std::fs::read_to_string(temp.join("published-keys/jack.keys")).expect("key");
    let jack_key: Vec<&str> = jack.split_whitespace().take(2).collect();
    assert!(recipients.contains(&jack_key.join(" ")), "{recipients}");

    // The workspace never sees ciphertext.
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.shared")).expect("read"),
        SECRET
    );

    // `tracker list` says so.
    let list = fixture.ok(&fixture.store, &["tracker", "list"]);
    let row = list.lines().find(|line| line.contains("env")).expect("row");
    assert!(row.contains("project-devs, encrypted"), "{list}");
}

#[test]
fn a_new_instance_decrypts_main_with_the_ssh_key_owner_only_and_an_unchanged_capture_commits_nothing()
 {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "project-devs");
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let main = remote_blob(&fixture, "refs/heads/main:.env.shared");
    assert!(
        !contains(&main, "hunter2"),
        "main on the remote is ciphertext"
    );

    // Nothing this machine remembers: the next instance reads `main` with
    // jack's SSH key alone.
    std::fs::remove_dir_all(fixture.store.join(".newgit/local/tracker-plain")).expect("rm");
    fixture.ok(&fixture.store, &["spawn", "feature-b"]);
    let second = workspace_of(&fixture, "feature-b");
    let file = second.join(".env.shared");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), SECRET);
    // Decrypted, so its owner's alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // `main` itself, bound as it is: status does not call it diverged,
    // and an unchanged capture is the rev it already had.
    let status = fixture.ok(&second, &["status"]);
    assert!(!status.contains("differs"), "{status}");
    let store = tracker_store(&fixture);
    let tip = || {
        git(
            &store,
            &["rev-parse", "refs/trackers/env/instances/feature-b"],
        )
    };
    fixture.ok(&second, &["tracker", "capture", "env"]);
    let before = tip();
    fixture.ok(&second, &["tracker", "capture", "env"]);
    assert_eq!(tip(), before, "nothing changed, nothing new");
}

#[test]
fn a_public_tracker_syncs_plaintext() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "public");
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    assert_eq!(
        remote_blob(&fixture, "refs/heads/main:.env.shared"),
        SECRET.as_bytes()
    );
    let list = fixture.ok(&fixture.store, &["tracker", "list"]);
    let row = list.lines().find(|line| line.contains("env")).expect("row");
    assert!(!row.contains("encrypted"), "{list}");
}

#[test]
fn a_plaintext_tracker_is_encrypted_on_its_next_capture_as_a_new_history_that_replaces_main() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    // Public, so plaintext, and merged: the remote's `main` is plaintext.
    let workspace = checkpointed(&fixture, "public");
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let store = tracker_store(&fixture);
    let plain = git(
        &store,
        &["rev-parse", "refs/trackers/env/instances/feature-a"],
    );

    // Then it is for the project's members only.
    let path = fixture.store.join(".newgit/trackers/env.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(
        &path,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write");

    fixture.ok(&workspace, &["checkpoint"]);
    let sealed = git(
        &store,
        &["rev-parse", "refs/trackers/env/instances/feature-a"],
    );
    assert_ne!(sealed, plain, "the same content, encrypted, is a new rev");
    assert_eq!(
        git(&store, &["rev-list", "--count", &sealed]),
        "1",
        "with no parent: pushing it carries no plaintext rev with it"
    );
    let sent = remote_blob(
        &fixture,
        "refs/trackers/env/instances/feature-a:.env.shared",
    );
    assert!(!contains(&sent, "hunter2"));

    // Merging it replaces the plaintext `main` it was made from: the one
    // history replacement there is.
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let main = remote_blob(&fixture, "refs/heads/main:.env.shared");
    assert!(main.starts_with(b"age-encryption.org/v1\n"));
    assert!(!contains(&main, "hunter2"));
}

#[test]
fn nobody_in_the_audience_with_a_key_refuses_the_capture() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    std::fs::remove_dir_all(temp.join("published-keys")).expect("rm");
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(&fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");

    let output = fixture.newgit(&workspace, &["tracker", "capture", "env"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nobody in its audience (ana, jack)"),
        "{stderr}"
    );
    assert!(stderr.contains("`public`"), "{stderr}");
    assert!(!fixture.remote.repo("jack.store.t.env").exists());
}

#[test]
fn members_add_warns_about_a_member_without_a_key_lists_fingerprints_and_remove_says_to_rotate() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "project-devs");
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    let added = fixture.ok(&fixture.store, &["members", "add", "bob"]);
    assert!(
        added.contains("WARNING: bob has no ssh-ed25519 or ssh-rsa key"),
        "{added}"
    );
    assert!(added.contains("`env`"), "{added}");

    publish_ssh_key(&temp, "carol", &temp.join("carol/.ssh/id_ed25519"));
    let added = fixture.ok(&fixture.store, &["members", "add", "carol"]);
    assert!(added.contains("main re-encrypted at"), "{added}");
    assert!(added.contains("CHANGED"), "said loudly:\n{added}");
    assert!(added.contains("+ carol SHA256:"), "{added}");
    assert!(added.contains("    carol: SHA256:"), "{added}");
    let recipients = String::from_utf8(remote_blob(&fixture, "refs/heads/main:.newgit/recipients"))
        .expect("utf8");
    assert!(recipients.contains("user:carol "), "{recipients}");
    assert!(!contains(
        &remote_blob(&fixture, "refs/heads/main:.env.shared"),
        "hunter2"
    ));

    let removed = fixture.ok(&fixture.store, &["members", "remove", "carol"]);
    assert!(
        removed.contains("ROTATE: carol can still read everything"),
        "{removed}"
    );
    assert!(removed.contains("- carol SHA256:"), "{removed}");
    let recipients = String::from_utf8(remote_blob(&fixture, "refs/heads/main:.newgit/recipients"))
        .expect("utf8");
    assert!(!recipients.contains("user:carol"), "{recipients}");

    // And the content is the same, read back with jack's key alone.
    std::fs::remove_dir_all(fixture.store.join(".newgit/local/tracker-plain")).expect("rm");
    fixture.ok(&fixture.store, &["spawn", "feature-c"]);
    assert_eq!(
        std::fs::read_to_string(workspace_of(&fixture, "feature-c").join(".env.shared"))
            .expect("read"),
        SECRET
    );
}
