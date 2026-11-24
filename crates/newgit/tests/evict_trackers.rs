//! `newgit stop --evict` and the `start` after it with trackers outside
//! Git: an encrypted one on the remote (#338), a fork bound in the
//! instance, one kept only here, one the instance dropped, and a checkpoint
//! the remote refuses to take.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"))
}

fn tracker(fixture: &Fixture, args: &[&str]) {
    let mut full = vec!["tracker"];
    full.extend(args);
    fixture.ok(&fixture.store, &full);
}

/// Whether any blob anywhere in `repo` holds `needle`.
fn any_blob_holds(repo: &Utf8Path, needle: &str) -> bool {
    let objects = git(repo, &["rev-list", "--all", "--objects"]);
    objects.lines().any(|line| {
        let oid = line.split(' ').next().unwrap_or_default();
        let kind = git(repo, &["cat-file", "-t", oid]);
        kind == "blob"
            && String::from_utf8_lossy(&git_output(repo, &["cat-file", "blob", oid]).stdout)
                .contains(needle)
    })
}

/// The encrypted tracker's content is in the checkpoint, leaves the machine
/// only as ciphertext, and comes back decrypted with this machine's key
/// even when the plaintext cache is gone. Alongside it, a tracker kept only
/// here and one the instance dropped come back as they were.
#[test]
fn trackers_outside_git_come_back_and_the_encrypted_one_stays_encrypted() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    tracker(&fixture, &["create", "env", "--storage", "remote"]);
    tracker(&fixture, &["track", "env", ".env.shared"]);
    tracker(&fixture, &["create", "scratch"]);
    tracker(&fixture, &["track", "scratch", "scratch"]);
    tracker(&fixture, &["create", "notes"]);
    tracker(&fixture, &["track", "notes", "notes"]);
    fixture.ok(&fixture.store, &["spawn", "a"]);
    let ws = workspace_of(&fixture, "a");
    std::fs::write(ws.join(".env.shared"), SECRET).expect("write");
    std::fs::create_dir_all(ws.join("scratch")).expect("mkdir");
    std::fs::write(ws.join("scratch/plan.md"), "plan\n").expect("write");
    std::fs::create_dir_all(ws.join("notes")).expect("mkdir");
    std::fs::write(ws.join("notes/n.md"), "note\n").expect("write");
    fixture.ok(&ws, &["checkpoint"]);
    fixture.ok(&fixture.store, &["tracker", "drop", "notes", "a"]);
    assert!(!ws.join("notes/n.md").exists());

    let evicted = fixture.ok(&fixture.store, &["stop", "a", "--evict", "--json"]);
    let evicted: Value = serde_json::from_str(&evicted).expect("json");
    assert_eq!(evicted["evicted"]["discarded"], json!([]), "{evicted}");
    assert!(!ws.join(".env.shared").exists());

    // On the remote, ciphertext only; here, the path is held.
    assert!(!any_blob_holds(
        &fixture.remote.repo("jack.store.t.env"),
        "hunter2"
    ));
    let held = read(&fixture.store.join(".newgit/local/sealed-paths.json"));
    assert!(held.contains(".env.shared"), "{held}");

    // Nothing this machine remembers in plaintext: the key decrypts it.
    std::fs::remove_dir_all(fixture.store.join(".newgit/local/tracker-plain")).expect("rm");
    fixture.ok(&fixture.store, &["start", "a"]);
    let file = ws.join(".env.shared");
    assert_eq!(read(&file), SECRET);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "decrypted, so its owner's alone");
    }
    assert_eq!(read(&ws.join("scratch/plan.md")), "plan\n");

    // Still dropped, and its work is where the drop left it.
    assert!(!ws.join("notes/n.md").exists());
    fixture.ok(&fixture.store, &["tracker", "use", "notes", "a"]);
    assert_eq!(read(&ws.join("notes/n.md")), "note\n");
}

/// A fork of a tracker, bound in the instance in its upstream's place,
/// comes back with its own content and still bound.
#[test]
fn a_bound_fork_comes_back_bound() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    tracker(&fixture, &["create", "env", "--role", "env"]);
    tracker(&fixture, &["track", "env", ".env.dev"]);
    fixture.ok(&fixture.store, &["spawn", "seed"]);
    let seed = workspace_of(&fixture, "seed");
    std::fs::write(seed.join(".env.dev"), "BASE=1\n").expect("write");
    fixture.ok(&seed, &["checkpoint"]);
    fixture.ok(&seed, &["tracker", "merge", "env"]);
    tracker(&fixture, &["create", "env-mine", "--from", "env"]);
    fixture.ok(&fixture.store, &["spawn", "a"]);
    fixture.ok(&fixture.store, &["tracker", "use", "env-mine", "a"]);
    let ws = workspace_of(&fixture, "a");
    std::fs::write(ws.join(".env.dev"), "MINE=1\n").expect("write");
    let before = fixture.ok(&fixture.store, &["tracker", "list", "--json"]);

    fixture.ok(&fixture.store, &["stop", "a", "--evict"]);
    fixture.ok(&fixture.store, &["start", "a"]);
    assert_eq!(read(&ws.join(".env.dev")), "MINE=1\n");
    assert_eq!(
        fixture.ok(&fixture.store, &["tracker", "list", "--json"]),
        before
    );
}

/// The remote failing to take the checkpoint does not stop the eviction:
/// the checkpoint is whole in this machine's store, which the eviction does
/// not touch, and the restore reads it from there.
#[test]
fn a_checkpoint_the_remote_did_not_take_is_still_restored_here() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    tracker(&fixture, &["create", "env", "--storage", "remote"]);
    tracker(&fixture, &["track", "env", ".env.shared"]);
    fixture.ok(&fixture.store, &["spawn", "a"]);
    let ws = workspace_of(&fixture, "a");
    std::fs::write(ws.join(".env.shared"), SECRET).expect("write");
    std::fs::write(ws.join("draft.txt"), "draft\n").expect("write");

    fixture
        .remote
        .state
        .lock()
        .expect("state")
        .fail_checkpoint_writes = 1;
    let evicted = fixture.newgit(&fixture.store, &["stop", "a", "--evict"]);
    assert!(evicted.status.success());
    let said = String::from_utf8_lossy(&evicted.stderr);
    assert!(
        said.contains("the only copy of `a`'s work is now in the project store on this machine"),
        "{said}"
    );

    fixture.ok(&fixture.store, &["start", "a"]);
    assert_eq!(read(&ws.join(".env.shared")), SECRET);
    assert_eq!(read(&ws.join("draft.txt")), "draft\n");
}
