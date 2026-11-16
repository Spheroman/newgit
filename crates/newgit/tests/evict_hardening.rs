//! The edges of `stop --evict` a second review found: a read-only tree in
//! the workspace, a moved-aside workspace that cannot be deleted, an
//! `lsof` that does not answer, processes named with their directory,
//! special mode bits and symlinks in the mode replay, and config values
//! added to a key the new clone already has.

mod evict_support;

use camino::{Utf8Path, Utf8PathBuf};
use evict_support::*;

fn spawn(temp: &Utf8Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let store = setup(temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    let ws = temp.join("workspaces/x");
    std::fs::write(ws.join("untracked.txt"), "u\n").expect("write");
    (repo, ws)
}

fn evicted(repo: &Utf8Path) -> bool {
    newgit_ok(repo, &["status"]).stdout.contains("evicted")
}

/// A read-only directory (0555, with a 0444 file) does not leave the
/// moved-aside workspace behind: it is made writable and deleted, the
/// restore gives the modes back, and evicting again works.
#[test]
fn a_read_only_directory_is_evicted_whole() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    std::fs::create_dir_all(ws.join("ro")).expect("mkdir");
    std::fs::write(ws.join("ro/f"), "f\n").expect("write");
    chmod(&ws.join("ro/f"), 0o444);
    chmod(&ws.join("ro"), 0o555);
    let before = snapshot(&ws);
    let trash = temp.join("workspaces/.x.evicting");

    let evicting = newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(!trash.exists(), "{}", evicting.stderr);
    assert!(!evicting.stderr.contains("could not be deleted"));
    assert!(evicted(&repo));

    newgit_ok(&repo, &["start", "x"]);
    assert_same(&before, &snapshot(&ws));
    assert_eq!(mode(&ws.join("ro")), 0o555);

    newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(!trash.exists());
    newgit_ok(&repo, &["start", "x"]);
    assert_same(&before, &snapshot(&ws));
    chmod(&ws.join("ro"), 0o755);
}

/// A moved-aside workspace an earlier eviction could not delete (here, an
/// immutable file in it) stops the next eviction before anything is
/// stopped or recorded, saying what to do; once it is gone, eviction works.
#[cfg(target_os = "macos")]
#[test]
fn an_undeletable_leftover_refuses_before_anything_changes() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    let trash = temp.join("workspaces/.x.evicting");
    std::fs::create_dir_all(&trash).expect("mkdir");
    std::fs::write(trash.join("stuck"), "stuck\n").expect("write");
    let flag = |how: &str| {
        let status = std::process::Command::new("chflags")
            .args([how, trash.join("stuck").as_str()])
            .status()
            .expect("chflags");
        assert!(status.success());
    };
    flag("uchg");

    let refusal = refused(&repo, &["stop", "x", "--evict"], "could not be deleted");
    assert!(refusal.stderr.contains("rm -rf"), "{}", refusal.stderr);
    assert!(refusal.stderr.contains("Nothing was changed"));
    assert!(!evicted(&repo));
    assert_same(&before, &snapshot(&ws));
    assert!(!temp.join("store/.newgit/state/x/evicted").exists());

    flag("nouchg");
    newgit_ok(&repo, &["stop", "x", "--evict"]);
    assert!(!trash.exists());
    newgit_ok(&repo, &["start", "x"]);
    assert_same(&before, &snapshot(&ws));
}

/// An `lsof` that does not answer (a dead network mount) refuses the
/// eviction under a timeout, deleting nothing.
#[cfg(target_os = "macos")]
#[test]
fn an_lsof_that_hangs_refuses_the_eviction() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    let before = snapshot(&ws);
    let bin = temp.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    std::fs::write(bin.join("lsof"), "#!/bin/sh\nexec sleep 30\n").expect("write");
    chmod(&bin.join("lsof"), 0o755);
    let path = format!("{bin}:{}", std::env::var("PATH").unwrap_or_default());

    let started = std::time::Instant::now();
    let run = newgit_env(
        &repo,
        &["stop", "x", "--evict"],
        &[
            ("PATH", path.as_str()),
            ("NEWGIT_TEST_LSOF_TIMEOUT_MS", "500"),
        ],
    );
    assert!(started.elapsed().as_secs() < 20, "did not wait for `lsof`");
    assert!(!run.status.success());
    assert!(run.stderr.contains("did not answer"), "{}", run.stderr);
    assert!(!evicted(&repo));
    assert_same(&before, &snapshot(&ws));
}

/// A process with its working directory in the workspace is named with
/// its pid, its command, and that directory.
#[test]
fn a_process_in_the_workspace_is_named_with_its_directory() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    std::fs::create_dir_all(ws.join("sub")).expect("mkdir");
    let mut sitting = std::process::Command::new("sleep")
        .arg("60")
        .current_dir(ws.join("sub"))
        .spawn()
        .expect("sleep");
    let refusal = refused(&repo, &["stop", "x", "--evict"], "not evicting `x`");
    let _ = sitting.kill();
    let _ = sitting.wait();
    assert!(
        refusal.stderr.contains(&format!(
            "pid {} `sleep` in {}",
            sitting.id(),
            ws.join("sub")
        )),
        "{}",
        refusal.stderr
    );
    assert!(ws.join("untracked.txt").exists());
}

/// Setuid, setgid, and sticky bits are not put back, and are said first;
/// the other bits come back. A mode is never put on what a symlink points
/// at, even when the saved record names a path through one.
#[test]
fn special_bits_are_masked_and_symlinks_never_followed() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    std::fs::write(ws.join("suid"), "#!/bin/sh\n").expect("write");
    chmod(&ws.join("suid"), 0o4755);
    std::fs::create_dir_all(ws.join("sticky")).expect("mkdir");
    std::fs::write(ws.join("sticky/f"), "f\n").expect("write");
    chmod(&ws.join("sticky"), 0o1777);
    let outside = temp.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir");
    std::fs::write(outside.join("secret"), "secret\n").expect("write");
    chmod(&outside.join("secret"), 0o600);
    std::os::unix::fs::symlink(outside.join("secret"), ws.join("link-out")).expect("symlink");
    std::os::unix::fs::symlink(&outside, ws.join("link-dir")).expect("symlink");

    let evicting = newgit_ok(&repo, &["stop", "x", "--evict", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&evicting.stdout).expect("json");
    let not_kept = json["evicted"]["not_kept"].to_string();
    assert!(
        not_kept.contains("setuid, setgid, and sticky"),
        "{not_kept}"
    );
    assert!(not_kept.contains("`suid`") && not_kept.contains("`sticky`"));

    // A record that names paths through symlinks (as one written by hand,
    // or by an older newgit, could).
    let saved = temp.join("store/.newgit/state/x/evicted/saved.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&saved).expect("read")).expect("json");
    let modes = record["files"]["modes"].as_object_mut().expect("modes");
    modes.insert("link-out".into(), 0o777.into());
    modes.insert("link-dir/secret".into(), 0o777.into());
    modes.insert("suid".into(), 0o4755.into());
    std::fs::write(&saved, record.to_string()).expect("write");

    let started = newgit_ok(&repo, &["start", "x"]);
    assert_eq!(mode(&ws.join("suid")), 0o755, "{}", started.stderr);
    assert_eq!(mode(&ws.join("sticky")), 0o777);
    assert_eq!(mode(&outside.join("secret")), 0o600);
    assert!(
        std::fs::symlink_metadata(ws.join("link-out"))
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

/// A value added to a key the new clone already has (a second
/// `remote.origin.fetch`) comes back beside newgit's; a value set by hand
/// on a key newgit sets itself (`remote.origin.pushurl`) is named before
/// anything is deleted, and newgit's comes back.
#[test]
fn config_additions_come_back_and_drops_are_said_first() {
    let (_guard, temp) = tempdir("evict-hard");
    let (repo, ws) = spawn(&temp);
    let extra = "+refs/pull/*:refs/remotes/origin/pr/*";
    git(&ws, &["config", "--add", "remote.origin.fetch", extra]);
    let pushurl = git_out(&ws, &["config", "--get", "remote.origin.pushurl"]);
    git(
        &ws,
        &["config", "remote.origin.pushurl", "/somewhere/by/hand"],
    );
    git(&ws, &["config", "core.checkStat", "default"]);

    let evicting = newgit_ok(&repo, &["stop", "x", "--evict", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&evicting.stdout).expect("json");
    let not_kept = json["evicted"]["not_kept"].to_string();
    assert!(not_kept.contains("remote.origin.pushurl"), "{not_kept}");
    assert!(!not_kept.contains("remote.origin.fetch"), "{not_kept}");
    assert!(
        !not_kept.contains("/somewhere/by/hand"),
        "values are not printed"
    );

    newgit_ok(&repo, &["start", "x"]);
    let fetch = git_out(&ws, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.contains("+refs/heads/*:refs/remotes/origin/*"),
        "{fetch}"
    );
    assert!(fetch.contains(extra), "{fetch}");
    assert_eq!(
        git_out(&ws, &["config", "--get", "remote.origin.pushurl"]),
        pushurl
    );
    assert_eq!(
        git_out(&ws, &["config", "--get", "core.checkStat"]).trim(),
        "default"
    );
}
