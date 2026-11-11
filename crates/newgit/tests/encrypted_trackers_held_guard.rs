//! The held-paths guard (#338) end to end: instances checkpointing at once
//! all record their paths; a held-paths file that does not parse stops the
//! send instead of turning the guard off; a claim spelled in another case
//! on a case-insensitive filesystem is the same path; and `tracker pull
//! --keep-mine` keeps the developer's deletions.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

fn all(output: &Output) -> String {
    format!(
        "status={:?}\n--stdout--\n{}\n--stderr--\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn commit_definitions(fixture: &Fixture, message: &str) {
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", message]);
    fixture.ok(&fixture.store, &["merge", "main"]);
}

fn definition_dir(fixture: &Fixture) -> Utf8PathBuf {
    fixture.store.join(".newgit/trackers")
}

fn held_file(fixture: &Fixture) -> Utf8PathBuf {
    fixture.store.join(".newgit/local/sealed-paths.json")
}

fn held(fixture: &Fixture) -> Vec<String> {
    let text = std::fs::read_to_string(held_file(fixture)).expect("held paths");
    serde_json::from_str(&text).expect("parses")
}

/// `env` (project-devs, synced) owning `paths`, with `files` written in
/// `feature-a`, checkpointed and merged.
fn merged(fixture: &Fixture, paths: &[&str], files: &[&str]) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    for path in paths {
        fixture.ok(&fixture.store, &["tracker", "track", "env", path]);
    }
    commit_definitions(fixture, "env");
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    for file in files {
        let path = workspace.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, SECRET).expect("write");
    }
    fixture.ok(&workspace, &["checkpoint"]);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    workspace
}

/// Every repo on the remote holding `needle` in plaintext, in any object.
fn plaintext_on_remote(fixture: &Fixture, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![fixture.temp.join("artifacts")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = Utf8PathBuf::from_path_buf(entry.path()).expect("utf8");
            if path.join("objects").is_dir() {
                let objects = git(
                    &path,
                    &[
                        "cat-file",
                        "--batch-all-objects",
                        "--batch-check=%(objectname)",
                    ],
                );
                let found = objects.lines().any(|object| {
                    String::from_utf8_lossy(&git_output(&path, &["cat-file", "-p", object]).stdout)
                        .contains(needle)
                });
                if found {
                    hits.push(path.to_string());
                }
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }
    hits
}

/// The reviewer's reproduction, three times over: eight instances each
/// write a new file under `secrets/` and checkpoint at the same moment.
/// Every checkpoint succeeds and every path is recorded.
#[test]
fn eight_instances_checkpointing_at_once_all_record_their_paths() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", "secrets"]);
    commit_definitions(&fixture, "env");
    let workspaces: Vec<Utf8PathBuf> = (0..8)
        .map(|n| {
            let name = format!("i{n}");
            fixture.ok(&fixture.store, &["spawn", &name]);
            workspace_of(&fixture, &name)
        })
        .collect();

    let mut expected = Vec::new();
    for round in 0..3 {
        for (n, workspace) in workspaces.iter().enumerate() {
            let file = format!("secrets/f{n}-r{round}");
            std::fs::create_dir_all(workspace.join("secrets")).expect("mkdir");
            std::fs::write(workspace.join(&file), SECRET).expect("write");
            expected.push(file);
        }
        let results: Vec<Output> = std::thread::scope(|scope| {
            let running: Vec<_> = workspaces
                .iter()
                .map(|workspace| {
                    let fixture = &fixture;
                    scope.spawn(move || fixture.newgit(workspace, &["checkpoint"]))
                })
                .collect();
            running
                .into_iter()
                .map(|thread| thread.join().expect("checkpoint"))
                .collect()
        });
        for output in &results {
            assert!(output.status.success(), "round {round}: {}", all(output));
        }
        let recorded = held(&fixture);
        for file in &expected {
            assert!(
                recorded.contains(file),
                "round {round}: `{file}` missing from {recorded:?}"
            );
        }
    }
}

/// A held-paths file cut short (the reviewer truncated three bytes) used to
/// read as empty, turning the guard off. Now the checkpoint is refused,
/// naming the file, and nothing reaches the remote in plaintext.
#[test]
fn a_truncated_held_paths_file_refuses_instead_of_turning_the_guard_off() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);
    let path = definition_dir(&fixture).join("env.toml");
    let text = std::fs::read_to_string(&path).expect("env");
    std::fs::write(
        &path,
        text.replace("audience = \"project-devs\"", "audience = \"public\""),
    )
    .expect("write");
    commit_definitions(&fixture, "env is public");
    fixture.ok(&workspace, &["pull"]);

    let file = held_file(&fixture);
    let held = std::fs::read_to_string(&file).expect("held");
    std::fs::write(&file, &held[..held.len() - 3]).expect("truncate");
    std::fs::write(workspace.join(".env.shared"), "API_KEY=truncatedleak\n").expect("write");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    let said = all(&checkpoint);
    assert!(!checkpoint.status.success(), "{said}");
    assert!(said.contains("sealed-paths.json"), "{said}");
    assert!(said.contains("does not parse"), "{said}");
    assert!(
        plaintext_on_remote(&fixture, "truncatedleak").is_empty(),
        "{said}"
    );
}

/// Whether `dir` is on a filesystem that ignores case (macOS by default).
fn case_insensitive(dir: &Utf8Path) -> bool {
    let probe = dir.join("case-probe");
    std::fs::write(&probe, "").expect("probe");
    let insensitive = dir.join("CASE-PROBE").exists();
    std::fs::remove_file(&probe).expect("rm probe");
    insensitive
}

/// The reviewer's reproduction: `.env.shared` captured encrypted, `env`
/// deleted, a `public` tracker claiming `.ENV.SHARED`, which on a
/// case-insensitive filesystem is the same file. Refused, naming the claim
/// as spelled; released by that spelling. Only where the filesystem ignores
/// case; the comparison itself is tested on every filesystem in
/// `newgit-core`'s `held_sealed_paths`.
#[test]
fn a_claim_in_another_case_is_the_same_held_path() {
    let (_guard, temp) = tempdir();
    if !case_insensitive(&temp) {
        eprintln!("skipped: this filesystem tells `.env.shared` and `.ENV.SHARED` apart");
        return;
    }
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);
    std::fs::remove_file(definition_dir(&fixture).join("env.toml")).expect("rm");
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "open",
            "--storage",
            "remote",
            "--audience",
            "public",
        ],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "open", ".ENV.SHARED"]);
    commit_definitions(&fixture, "env replaced by open, public, in capitals");
    fixture.ok(&workspace, &["pull"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=caseleak\n").expect("write");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    let said = all(&checkpoint);
    assert!(!checkpoint.status.success(), "{said}");
    assert!(said.contains("held content encrypted"), "{said}");
    assert!(said.contains("allow-plaintext"), "{said}");
    assert!(
        plaintext_on_remote(&fixture, "caseleak").is_empty(),
        "{said}"
    );

    let released = fixture.ok(&workspace, &["tracker", "allow-plaintext", ".ENV.SHARED"]);
    assert!(released.contains("Released `.env.shared`"), "{released}");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
}

/// `allow-plaintext <directory>` releases every held file under it and
/// lists them.
#[test]
fn allow_plaintext_on_a_directory_releases_the_files_under_it() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &["secrets"], &["secrets/a", "secrets/deep/b"]);
    let released = fixture.ok(&workspace, &["tracker", "allow-plaintext", "secrets/"]);
    assert!(released.contains("Released `secrets/a`"), "{released}");
    assert!(released.contains("Released `secrets/deep/b`"), "{released}");
    assert!(!released.contains("was not held back"), "{released}");
    let left = held(&fixture);
    assert!(
        !left.iter().any(|path| path.starts_with("secrets/")),
        "{left:?}"
    );
}

/// Machine B deletes `.env.extra` and checkpoints; machine A changes it and
/// merges first. B's `pull --keep-mine` keeps B's whole version: the file B
/// deleted is deleted again, not brought back from theirs, and the merge
/// publishes that.
#[test]
fn keep_mine_keeps_a_file_you_deleted_deleted() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let a = merged(
        &fixture,
        &[".env.shared", ".env.extra"],
        &[".env.shared", ".env.extra"],
    );

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
    assert!(b.join(".env.extra").exists());
    std::fs::remove_file(b.join(".env.extra")).expect("rm");
    std::fs::write(b.join(".env.shared"), "API_KEY=b\n").expect("write");
    fixture.ok(&b, &["checkpoint"]);

    std::fs::write(a.join(".env.extra"), "EXTRA=a\n").expect("write");
    fixture.ok(&a, &["checkpoint"]);
    fixture.ok(&a, &["tracker", "merge", "env"]);

    let merge = fixture.newgit(&b, &["tracker", "merge", "env"]);
    assert!(!merge.status.success(), "{}", all(&merge));
    let kept = fixture.ok(&b, &["tracker", "pull", "env", "--keep-mine"]);
    assert!(kept.contains("deleted `.env.extra` again"), "{kept}");
    assert!(!kept.contains("BACK from `main`"), "{kept}");
    assert!(!b.join(".env.extra").exists(), "{kept}");
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("read"),
        "API_KEY=b\n"
    );
    let merged = fixture.newgit(&b, &["tracker", "merge", "env"]);
    assert!(merged.status.success(), "{}", all(&merged));

    fixture.ok(&a, &["tracker", "pull", "env"]);
    assert!(!a.join(".env.extra").exists(), "B's deletion reached A");
    assert_eq!(
        std::fs::read_to_string(a.join(".env.shared")).expect("read"),
        "API_KEY=b\n"
    );
}
