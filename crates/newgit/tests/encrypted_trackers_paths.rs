//! The guard against a definition turning encryption off is keyed by path
//! (#338): a workspace path at which this machine has held encrypted
//! content is not captured or sent unencrypted, whatever tracker claims it
//! now (renamed, replaced, narrowed, or in Git), until `newgit tracker
//! allow-plaintext <path>` on this machine. A refused sync fails the
//! checkpoint command; an unreachable remote does not.

#![allow(dead_code)]

include!("support/fake_remote.rs");

const SECRET: &str = "API_KEY=hunter2\n";

/// `env` (project-devs, synced) owning `paths`, and `feature-a` with the
/// secret in each, checkpointed and merged.
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

/// Commit the store's definitions and make them `main`'s, as a teammate's
/// change arrives.
fn commit_definitions(fixture: &Fixture, message: &str) {
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", message]);
    fixture.ok(&fixture.store, &["merge", "main"]);
}

fn all(output: &Output) -> String {
    format!(
        "status={:?}\n--stdout--\n{}\n--stderr--\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
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

fn definition_dir(fixture: &Fixture) -> Utf8PathBuf {
    fixture.store.join(".newgit/trackers")
}

/// `env.toml`'s text with `audience = "public"`.
fn public_text(fixture: &Fixture) -> String {
    let text = std::fs::read_to_string(definition_dir(fixture).join("env.toml")).expect("env");
    assert!(text.contains("audience = \"project-devs\""), "{text}");
    text.replace("audience = \"project-devs\"", "audience = \"public\"")
}

/// The checkpoint is refused, as an error naming the path and the exact
/// command, and kept here; nothing of `needle` reaches the remote.
fn assert_refused(fixture: &Fixture, workspace: &Utf8Path, path: &str, needle: &str) -> String {
    let checkpoint = fixture.newgit(workspace, &["checkpoint"]);
    let said = all(&checkpoint);
    assert!(!checkpoint.status.success(), "{said}");
    assert!(
        said.contains(&format!("newgit tracker allow-plaintext {path}")),
        "{said}"
    );
    assert!(said.contains("held content encrypted"), "{said}");
    assert!(
        plaintext_on_remote(fixture, needle).is_empty(),
        "plaintext on the remote: {said}"
    );
    said
}

/// `env` renamed to `vars`, `public`, over the same path: the reviewer's
/// reproduction. Refused until the path is released, then stored as the
/// definition says.
#[test]
fn a_renamed_public_definition_over_an_encrypted_path_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);

    let public = public_text(&fixture);
    std::fs::remove_file(definition_dir(&fixture).join("env.toml")).expect("rm");
    std::fs::write(definition_dir(&fixture).join("vars.toml"), public).expect("write");
    commit_definitions(&fixture, "env is vars now, public");
    fixture.ok(&workspace, &["pull"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=renamedleak\n").expect("write");
    let said = assert_refused(&fixture, &workspace, ".env.shared", "renamedleak");
    assert!(said.contains("`vars`"), "{said}");
    assert!(said.contains("audience = \"public\""), "{said}");
    let capture = fixture.newgit(&workspace, &["tracker", "capture", "vars"]);
    assert!(!capture.status.success(), "{}", all(&capture));

    let released = fixture.ok(&workspace, &["tracker", "allow-plaintext", ".env.shared"]);
    assert!(released.contains("Released `.env.shared`"), "{released}");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
    assert!(
        !plaintext_on_remote(&fixture, "renamedleak").is_empty(),
        "released: stored as its definition says"
    );
}

/// The definition deleted (by a teammate's commit; `tracker remove` here
/// refuses while an instance binds it), and a new `public` one made over
/// the same path.
#[test]
fn a_deleted_definition_replaced_by_a_public_one_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);

    std::fs::remove_file(definition_dir(&fixture).join("env.toml")).expect("rm");
    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "shared",
            "--storage",
            "remote",
            "--audience",
            "public",
        ],
    );
    fixture.ok(
        &fixture.store,
        &["tracker", "track", "shared", ".env.shared"],
    );
    commit_definitions(&fixture, "env replaced by shared, public");
    fixture.ok(&workspace, &["pull"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=replacedleak\n").expect("write");
    let said = assert_refused(&fixture, &workspace, ".env.shared", "replacedleak");
    assert!(said.contains("`shared`"), "{said}");
}

/// A narrower `public` tracker claiming one file the encrypted one owned:
/// that file is held back; the encrypted one's other files stay encrypted.
#[test]
fn a_narrower_public_tracker_over_one_encrypted_path_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &["secrets"], &["secrets/a", "secrets/b"]);

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
    fixture.ok(&fixture.store, &["tracker", "track", "open", "secrets/b"]);
    commit_definitions(&fixture, "secrets/b is public");
    fixture.ok(&workspace, &["pull"]);

    std::fs::write(workspace.join("secrets/b"), "API_KEY=narrowleak\n").expect("write");
    let said = assert_refused(&fixture, &workspace, "secrets/b", "narrowleak");
    assert!(said.contains("`open`"), "{said}");
    assert!(
        !said.contains("secrets/a"),
        "only the path it claims: {said}"
    );
}

/// A definition turned `git = true`: the path stays in the workspace's
/// `.git/info/exclude` and the store's `.gitignore` (written when it was a
/// tracker outside Git), which is why a plain checkpoint commits nothing
/// of it. That is not the guard: committed anyway (`git add -f`), the push
/// gate refuses it by path, and nothing reaches the remote. The refusal is
/// at sync time: the checkpoint is kept here, and the command fails.
#[test]
fn a_definition_turned_git_true_is_refused_at_the_push_gate() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);

    let path = definition_dir(&fixture).join("env.toml");
    let text = std::fs::read_to_string(&path).expect("env");
    let flipped = if text.contains("git = false") {
        text.replace("git = false", "git = true")
    } else {
        format!("git = true\n{text}")
    };
    std::fs::write(&path, flipped).expect("write");
    commit_definitions(&fixture, "env in git");
    fixture.ok(&workspace, &["pull"]);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=gitleak\n").expect("write");
    // Not swept in by `git add -A`: still ignored.
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    assert!(checkpoint.status.success(), "{}", all(&checkpoint));
    assert!(plaintext_on_remote(&fixture, "gitleak").is_empty());

    git(&workspace, &["add", "-f", ".env.shared"]);
    git(&workspace, &["commit", "-q", "-m", "commit the env file"]);
    let said = assert_refused(&fixture, &workspace, ".env.shared", "gitleak");
    assert!(said.contains("tracker in Git"), "{said}");
    assert!(said.contains("Checkpoint ckpt_"), "kept here: {said}");
    assert!(said.contains("not synced: refused"), "{said}");
}

/// A fresh clone whose spawn decrypts the encrypted `main` holds that
/// content too: with the definition now `public`, its checkpoint is
/// refused, and the refusal says how this machine came to hold it.
#[test]
fn a_fresh_clone_that_decrypted_the_content_holds_it_back_too() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let _workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);
    let public = public_text(&fixture);
    std::fs::write(definition_dir(&fixture).join("env.toml"), public).expect("write");
    commit_definitions(&fixture, "env is public");

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
    assert_eq!(
        std::fs::read_to_string(b.join(".env.shared")).expect("decrypted"),
        SECRET
    );
    std::fs::write(b.join(".env.shared"), "API_KEY=freshleak\n").expect("write");
    let said = assert_refused(&fixture, &b, ".env.shared", "freshleak");
    assert!(said.contains("decrypted into a workspace here"), "{said}");
}

/// A sync that cannot reach the remote is a warning and exit 0, as before:
/// only a refusal fails the command.
#[test]
fn an_unreachable_remote_is_still_a_warning() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = merged(&fixture, &[".env.shared"], &[".env.shared"]);
    fixture.remote.state.lock().expect("state").fail_credentials = true;
    std::fs::write(workspace.join(".env.shared"), "API_KEY=later\n").expect("write");
    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    let said = all(&checkpoint);
    assert!(checkpoint.status.success(), "{said}");
    assert!(said.contains("not synced (the warning above"), "{said}");
}
