//! What a removed member can read of an encrypted tracker (#338), and
//! `members add/remove` failing partway.
//!
//! Nothing encrypted to a set of recipients that is no longer the audience
//! reaches the remote unless the remote already had it: commits the remote
//! lacks are made again, encrypted to the members as they are now, before
//! anything is sent. A removed member keeps what the remote held while they
//! were a member, and nothing else.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

fn env_repo(fixture: &Fixture) -> Utf8PathBuf {
    fixture.remote.repo("jack.store.t.env")
}

/// `env` (project-devs, synced) owning `files`, and `feature-a` with the
/// secret in each, checkpointed.
fn checkpointed(fixture: &Fixture, tracker: &str, files: &[&str]) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", tracker, "--storage", "remote"],
    );
    for file in files {
        fixture.ok(&fixture.store, &["tracker", "track", tracker, file]);
    }
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", tracker]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    for file in files {
        std::fs::write(workspace.join(file), SECRET).expect("write");
    }
    fixture.ok(&workspace, &["checkpoint"]);
    workspace
}

/// The project cloned onto another machine (the same login).
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

/// `bytes` decrypted with the SSH key at `private`, if it opens them.
fn opens(private: &Utf8Path, bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let text = std::fs::read_to_string(private).expect("key");
    let identity = age::ssh::Identity::from_buffer(text.as_bytes(), None).expect("ssh key");
    let decryptor = age::Decryptor::new(bytes).ok()?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .ok()?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Every plaintext the key at `private` opens among the age files `repo`
/// holds, whatever ref (or none) reaches them.
fn readable_by(repo: &Utf8Path, private: &Utf8Path) -> Vec<String> {
    let objects = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    let mut out = Vec::new();
    for line in objects.lines() {
        let Some((oid, "blob")) = line.split_once(' ') else {
            continue;
        };
        let bytes = git_output(repo, &["cat-file", "blob", oid]).stdout;
        if !bytes.starts_with(b"age-encryption.org/v1\n") {
            continue;
        }
        if let Some(plain) = opens(private, &bytes) {
            out.push(String::from_utf8_lossy(&plain).into_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn all(output: &Output) -> String {
    format!(
        "status={:?}\n--stdout--\n{}\n--stderr--\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Captured while the keys cannot be read (so encrypted to the members this
/// machine last read, ana among them) after ana was removed on another
/// machine, then checkpointed and merged: ana's key opens only what the
/// remote held while ana was a member.
#[test]
fn a_capture_from_the_cache_after_a_removal_never_reaches_the_remote_for_the_removed_member() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "env", &[".env.shared"]);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let ana = temp.join("ana/.ssh/id_ed25519");
    assert_eq!(readable_by(&env_repo(&fixture), &ana), vec![SECRET]);

    let other = second_machine(&fixture);
    fixture.ok(&other, &["members", "remove", "ana"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=after-removal\n").expect("write");
    let offline = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(workspace.as_str())
        .args(["tracker", "capture", "env"])
        .env("HOME", temp.join("home").as_str())
        // Keys unreachable: the capture falls back to what this machine
        // last read, which still names ana.
        .env("NEWGIT_KEYS_URL", "http://127.0.0.1:1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .expect("runs");
    assert!(offline.status.success(), "{}", all(&offline));
    assert!(
        String::from_utf8_lossy(&offline.stderr).contains("last read"),
        "{}",
        all(&offline)
    );

    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
    assert!(
        String::from_utf8_lossy(&checkpoint.stderr).contains("- ana"),
        "the removal is said: {}",
        all(&checkpoint)
    );
    assert_eq!(
        readable_by(&env_repo(&fixture), &ana),
        vec![SECRET],
        "ana opens nothing captured after the removal"
    );
    // The workspace still holds the new value, and jack reads it back.
    assert_eq!(
        std::fs::read_to_string(workspace.join(".env.shared")).expect("read"),
        "API_KEY=after-removal\n"
    );
    let jack = temp.join("home/.ssh/id_ed25519");
    assert!(
        readable_by(&env_repo(&fixture), &jack).contains(&"API_KEY=after-removal\n".to_owned())
    );

    let merge = fixture.newgit(&workspace, &["tracker", "merge", "env"]);
    assert!(merge.status.success(), "{}", all(&merge));
    assert_eq!(readable_by(&env_repo(&fixture), &ana), vec![SECRET]);
}

/// Captured while ana was a member and never sent; ana removed on another
/// machine; then checkpointed: the capture is made again for the members
/// as they are now, and ana's key never opens it.
#[test]
fn an_unsent_capture_from_before_a_removal_is_encrypted_again_before_it_is_sent() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "env", &[".env.shared"]);
    std::fs::write(workspace.join(".env.shared"), "API_KEY=never-sent\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", "env"]);
    // A second capture on top, also never sent: both are made again.
    std::fs::write(workspace.join(".env.shared"), "API_KEY=never-sent-2\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", "env"]);

    let other = second_machine(&fixture);
    fixture.ok(&other, &["members", "remove", "ana"]);

    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
    let ana = temp.join("ana/.ssh/id_ed25519");
    assert_eq!(
        readable_by(&env_repo(&fixture), &ana),
        vec![SECRET],
        "ana opens only what the remote had while ana was a member"
    );
    let jack = temp.join("home/.ssh/id_ed25519");
    let readable = readable_by(&env_repo(&fixture), &jack);
    assert!(
        readable.contains(&"API_KEY=never-sent\n".to_owned())
            && readable.contains(&"API_KEY=never-sent-2\n".to_owned()),
        "both captures are kept, made again: {readable:?}"
    );
    // On the remote, the instance branch's history holds no commit
    // encrypted to ana that the remote did not have before.
    let instance = git(
        &env_repo(&fixture),
        &["rev-parse", "refs/trackers/env/instances/feature-a"],
    );
    let log = git(&env_repo(&fixture), &["log", "--format=%s", &instance]);
    assert!(log.lines().count() >= 3, "{log}");
}

/// `members add` with one tracker whose `main` no key on this machine
/// opens: it says which were and were not re-encrypted, and fails.
#[test]
fn members_add_failing_partway_names_what_was_and_was_not_reencrypted() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture, "env", &[".env.shared"]);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);

    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env2", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env2", ".env.two"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env2"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&workspace, &["pull"]);
    std::fs::write(workspace.join(".env.two"), SECRET).expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    fixture.ok(&workspace, &["tracker", "merge", "env2"]);

    // env2's `main` on the remote: a commit only ana's key opens.
    let repo = fixture.remote.repo("jack.store.t.env2");
    let ana_public = std::fs::read_to_string(temp.join("ana/.ssh/id_ed25519.pub")).expect("pub");
    let recipient: age::ssh::Recipient = ana_public.trim().parse().expect("recipient");
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .expect("encryptor");
    let mut sealed = Vec::new();
    {
        use std::io::Write as _;
        let mut writer = encryptor.wrap_output(&mut sealed).expect("wrap");
        writer.write_all(b"API_KEY=ana-only\n").expect("write");
        writer.finish().expect("finish");
    }
    let blob_path = temp.join("blob");
    std::fs::write(&blob_path, &sealed).expect("write");
    let blob = git(&repo, &["hash-object", "-w", blob_path.as_str()]);
    let index = repo.join("forge-index");
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .arg(format!("--git-dir={repo}"))
            .args(args)
            .env("GIT_INDEX_FILE", index.as_str())
            .env("GIT_AUTHOR_NAME", "ana")
            .env("GIT_AUTHOR_EMAIL", "ana@example.com")
            .env("GIT_COMMITTER_NAME", "ana")
            .env("GIT_COMMITTER_EMAIL", "ana@example.com")
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    run(&["read-tree", "refs/heads/main"]);
    run(&[
        "update-index",
        "--cacheinfo",
        &format!("100644,{blob},.env.two"),
    ]);
    let tree = run(&["write-tree"]);
    let commit = run(&["commit-tree", &tree, "-p", "refs/heads/main", "-m", "ana's"]);
    run(&["update-ref", "refs/heads/main", &commit]);

    publish_ssh_key(&temp, "carol", &temp.join("carol/.ssh/id_ed25519"));
    let add = fixture.newgit(&fixture.store, &["members", "add", "carol"]);
    let stdout = String::from_utf8_lossy(&add.stdout);
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(!add.status.success(), "{}", all(&add));
    assert!(stdout.contains("`env`: main re-encrypted"), "{}", all(&add));
    assert!(stdout.contains("`env2`: NOT re-encrypted"), "{}", all(&add));
    assert!(stderr.contains("Re-encrypted: `env`."), "{}", all(&add));
    assert!(
        stderr.contains("NOT re-encrypted") && stderr.contains("`env2`"),
        "{}",
        all(&add)
    );
    assert!(
        !stdout.contains("on this machine only"),
        "no such outcome: {}",
        all(&add)
    );
    // carol opens env's `main` and nothing of env2.
    let carol = temp.join("carol/.ssh/id_ed25519");
    assert!(readable_by(&env_repo(&fixture), &carol).contains(&SECRET.to_owned()));
    assert!(readable_by(&repo, &carol).is_empty());
}
