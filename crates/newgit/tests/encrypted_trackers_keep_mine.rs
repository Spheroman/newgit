//! Recovering from a merge refused because the remote's `main` moved on
//! (#338): `newgit tracker pull <tracker> --keep-mine` takes mine on top of
//! theirs, so the next merge publishes it without overwriting theirs.

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn all(output: &Output) -> String {
    format!(
        "status={:?}\n--stdout--\n{}\n--stderr--\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read(path: &Utf8Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read {path}"))
}

/// Machine A and machine B (a fresh clone) both change `env`; A merges
/// first, so B's merge is refused. B follows the refusal: `pull
/// --keep-mine`, then `merge`. B's file wins, A's other file stays, and
/// A's merge is under B's on the remote, not overwritten.
#[test]
fn keep_mine_after_a_refused_merge_publishes_mine_on_top_of_theirs() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.extra"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "a"]);
    let a = workspace_of(&fixture, "a");
    std::fs::write(a.join(".env.shared"), "API_KEY=one\n").expect("write");
    fixture.ok(&a, &["checkpoint"]);
    fixture.ok(&a, &["tracker", "merge", "env"]);

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
    fixture.ok(&other, &["spawn", "b"]);
    let b = Utf8PathBuf::from(fixture.ok(&other, &["status", "b", "--path"]).trim());
    std::fs::write(b.join(".env.shared"), "API_KEY=b\n").expect("write");
    fixture.ok(&b, &["checkpoint"]);

    // A merges again first: a new value and a file B never had.
    std::fs::write(a.join(".env.shared"), "API_KEY=a\n").expect("write");
    std::fs::write(a.join(".env.extra"), "EXTRA=a\n").expect("write");
    fixture.ok(&a, &["checkpoint"]);
    fixture.ok(&a, &["tracker", "merge", "env"]);
    let theirs = git(
        &fixture.remote.repo("jack.store.t.env"),
        &["rev-parse", "refs/heads/main"],
    );

    let merge = fixture.newgit(&b, &["tracker", "merge", "env"]);
    assert!(!merge.status.success(), "{}", all(&merge));
    let said = String::from_utf8_lossy(&merge.stderr).into_owned();
    assert!(
        said.contains("newgit tracker pull env --keep-mine"),
        "{said}"
    );

    // Exactly what the refusal says to run.
    let kept = fixture.ok(&b, &["tracker", "pull", "env", "--keep-mine"]);
    assert!(kept.contains("REPLACED"), "{kept}");
    assert_eq!(read(&b.join(".env.shared")), "API_KEY=b\n", "{kept}");
    assert_eq!(read(&b.join(".env.extra")), "EXTRA=a\n", "theirs stays");
    let merged = fixture.newgit(&b, &["tracker", "merge", "env"]);
    assert!(merged.status.success(), "{}", all(&merged));

    // On the remote: theirs under mine, so nothing of A's was overwritten.
    let repo = fixture.remote.repo("jack.store.t.env");
    let main = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(main, theirs);
    let contained = git_output(&repo, &["merge-base", "--is-ancestor", &theirs, &main]);
    assert!(contained.status.success(), "A's merge is under B's");

    // And A pulls B's value with its own extra file.
    fixture.ok(&a, &["tracker", "pull", "env"]);
    assert_eq!(read(&a.join(".env.shared")), "API_KEY=b\n");
    assert_eq!(read(&a.join(".env.extra")), "EXTRA=a\n");
}
