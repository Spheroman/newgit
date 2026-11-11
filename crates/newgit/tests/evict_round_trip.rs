//! `newgit stop --evict` then `newgit start` gives back exactly what was
//! there, compared as a whole-workspace snapshot: modes, symlinks, empty
//! directories, `git status`, refs, the stash, the local config, hooks,
//! and notes; what is not kept is said before anything is deleted; a second
//! evict says it is evicted; `start` keeps the directory a shell is in; and
//! `cleanup` keeps an evicted instance's install store entries.

mod evict_support;

use camino::Utf8Path;
use evict_support::*;

fn spawn(
    temp: &Utf8Path,
) -> (
    newgit_core::store::MetadataStore,
    camino::Utf8PathBuf,
    camino::Utf8PathBuf,
) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["tracker", "create", "local", "--audience", "user"]);
    newgit_ok(&repo, &["tracker", "track", "local", ".env.local"]);
    newgit_ok(&repo, &["tracker", "track", "local", "secrets"]);
    newgit_ok(&repo, &["spawn", "x"]);
    (store, repo, temp.join("workspaces/x"))
}

/// What Git and the tracker store do not carry of a file comes back too: a
/// `0600` key and a `0700` script, untracked; a `0600` secret in a `user`
/// tracker outside Git; a symlink; empty directories at the top and inside
/// a tracker's path. And what Git keeps only in `.git/`: local config
/// (`user.*`, another remote and its refs, a branch's upstream and
/// description), a hook, notes, the stash, and what is staged.
#[test]
fn everything_a_workspace_is_comes_back() {
    let (_guard, temp) = tempdir("evict-round");
    let (_store, repo, ws) = spawn(&temp);

    std::fs::write(ws.join("key.pem"), "private\n").expect("write");
    chmod(&ws.join("key.pem"), 0o600);
    std::fs::write(ws.join("run.sh"), "#!/bin/sh\necho hi\n").expect("write");
    chmod(&ws.join("run.sh"), 0o700);
    std::fs::write(ws.join(".env.local"), "TOKEN=secret\n").expect("write");
    chmod(&ws.join(".env.local"), 0o600);
    std::fs::create_dir_all(ws.join("secrets/empty")).expect("mkdir");
    std::fs::write(ws.join("secrets/a.key"), "a\n").expect("write");
    chmod(&ws.join("secrets/a.key"), 0o600);
    std::fs::create_dir_all(ws.join("empty-top")).expect("mkdir");
    std::fs::create_dir_all(ws.join("private")).expect("mkdir");
    std::fs::write(ws.join("private/note.txt"), "n\n").expect("write");
    chmod(&ws.join("private"), 0o700);
    std::os::unix::fs::symlink("README.md", ws.join("readme-link")).expect("symlink");

    std::fs::write(ws.join("staged.txt"), "staged\n").expect("write");
    git(&ws, &["add", "staged.txt"]);
    std::fs::write(ws.join("staged.txt"), "then edited\n").expect("write");
    std::fs::write(ws.join("README.md"), "stashed\n").expect("write");
    git(
        &ws,
        &["stash", "push", "-q", "-m", "kept", "--", "README.md"],
    );

    git(&ws, &["config", "user.name", "Someone Else"]);
    git(&ws, &["config", "user.email", "else@example.com"]);
    let repo_str = repo.to_string();
    git(&ws, &["remote", "add", "up", &repo_str]);
    git(&ws, &["fetch", "-q", "up"]);
    git(&ws, &["config", "branch.x.remote", "up"]);
    git(&ws, &["config", "branch.x.merge", "refs/heads/main"]);
    git(&ws, &["config", "branch.x.description", "what this is for"]);
    std::fs::write(ws.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n").expect("write");
    chmod(&ws.join(".git/hooks/pre-commit"), 0o755);
    git(&ws, &["notes", "add", "-m", "a note", "HEAD"]);

    let before = snapshot(&ws);
    let evicted = newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(
        !ws.join("key.pem").exists(),
        "evicted:\n{}{}",
        evicted.stdout,
        evicted.stderr
    );
    newgit_ok(&repo, &["start", "x"]);
    assert_same(&before, &snapshot(&ws));
    assert_eq!(mode(&ws.join(".env.local")), 0o600);
}

/// `newgit undo` puts a tracker file back with the mode it had, not
/// world-readable: Git keeps only the executable bit.
#[test]
fn undo_keeps_a_tracker_files_mode() {
    let (_guard, temp) = tempdir("evict-round");
    let (_store, repo, ws) = spawn(&temp);
    std::fs::write(ws.join(".env.local"), "A=1\n").expect("write");
    chmod(&ws.join(".env.local"), 0o600);
    newgit_ok(&repo, &["checkpoint", "x"]);
    std::fs::write(ws.join(".env.local"), "A=2\n").expect("write");
    newgit_ok(&repo, &["undo", "x"]);
    assert_eq!(
        std::fs::read_to_string(ws.join(".env.local")).expect("read"),
        "A=1\n"
    );
    assert_eq!(mode(&ws.join(".env.local")), 0o600);
}

/// What no save keeps is named before anything is deleted: intent-to-add
/// and hand-set index flags, beside the reflogs.
#[test]
fn what_is_not_kept_is_said() {
    let (_guard, temp) = tempdir("evict-round");
    let (_store, repo, ws) = spawn(&temp);
    std::fs::write(ws.join("later.txt"), "l\n").expect("write");
    git(&ws, &["add", "-N", "later.txt"]);
    git(&ws, &["update-index", "--assume-unchanged", "README.md"]);
    let evicted = newgit_ok(&repo, &["stop", "x", "--evict", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&evicted.stdout).expect("json");
    let not_kept = json["evicted"]["not_kept"].to_string();
    assert!(not_kept.contains("intent-to-add"), "{not_kept}");
    assert!(not_kept.contains("assume-unchanged"), "{not_kept}");
    assert!(not_kept.contains("reflogs"), "{not_kept}");
    newgit_ok(&repo, &["start", "x"]);
    assert_eq!(
        std::fs::read_to_string(ws.join("later.txt")).expect("read"),
        "l\n"
    );
}

/// A second evict says the instance is already evicted, in words and in
/// `--json`, and deletes nothing.
#[test]
fn evicting_again_says_already_evicted() {
    let (_guard, temp) = tempdir("evict-round");
    let (_store, repo, _ws) = spawn(&temp);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let again = newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(
        again.stdout.contains("already evicted to ckpt_"),
        "{}",
        again.stdout
    );
    let json: serde_json::Value =
        serde_json::from_str(&newgit_ok(&repo, &["stop", "x", "--evict", "--json"]).stdout)
            .expect("json");
    assert_eq!(json["evicted"]["already"], true, "{json}");
}

/// `start` fills the directory that is there, so a shell sitting in the
/// marker's directory is in the workspace afterwards, not in a deleted one.
#[test]
fn start_keeps_the_directory_a_shell_is_in() {
    use std::os::unix::fs::MetadataExt as _;
    let (_guard, temp) = tempdir("evict-round");
    let (_store, repo, ws) = spawn(&temp);
    std::fs::write(ws.join("u.txt"), "u\n").expect("write");
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let inode = std::fs::metadata(&ws).expect("stat").ino();
    // Run from inside the marker's directory, as a shell left there would.
    newgit_ok(&ws, &["start", "x"]);
    assert_eq!(std::fs::metadata(&ws).expect("stat").ino(), inode);
    assert_eq!(
        std::fs::read_to_string(ws.join("u.txt")).expect("read"),
        "u\n"
    );
}

/// `cleanup` keeps the install store entry an evicted instance's restore
/// refills from, and says nothing about not being able to key it.
#[test]
fn cleanup_keeps_an_evicted_instances_install_entries() {
    let (_guard, temp) = tempdir("evict-round");
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("deps.toml"),
        r#"ownership = "workspace"

[identity]
paths = ["README.md"]
produces = ["node_modules"]

[actions.prepare]
command = "mkdir -p node_modules && echo built > node_modules/tree.txt"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
"#,
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    let installs = store.paths().installs.join("deps");
    let entries = || std::fs::read_dir(&installs).map(|e| e.count()).unwrap_or(0);
    assert_eq!(entries(), 1);
    let cleanup = newgit_ok(&repo, &["cleanup"]);
    assert!(
        !cleanup.stderr.contains("left alone") && !cleanup.stdout.contains("left alone"),
        "{}{}",
        cleanup.stdout,
        cleanup.stderr
    );
    assert_eq!(entries(), 1, "the evicted instance's entry is kept");
    newgit_ok(&repo, &["start", "x"]);
    assert!(temp.join("workspaces/x/node_modules/tree.txt").is_file());
}
