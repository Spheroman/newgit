//! `newgit tracker create <name> --git --mount <dir> --from <repo>` from the
//! command line (#140), on a project linked to a newgit remote: a relative
//! `--from` is a path from here, `--path` is refused with `--from`, and the
//! imported tracker reaches the remote as any new tracker does, through
//! `newgit merge main`, its commits unchanged.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn upstream(fixture: &Fixture) -> String {
    let repo = fixture.temp.join("upstream");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "newgit"]);
    std::fs::write(repo.join("app.ts"), "app\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "t3: first"]);
    git(&repo, &["rev-parse", "HEAD"])
}

#[test]
fn from_with_path_is_refused_naming_mount() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    upstream(&fixture);
    let output = fixture.newgit(
        &fixture.store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--path",
            "apps/t3code",
            "--from",
            "../upstream",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--mount, not --path"), "{stderr}");
    assert!(!fixture.store.join(".newgit/trackers/t3code.toml").exists());
}

#[test]
fn a_relative_from_is_imported_and_published_unchanged() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let tip = upstream(&fixture);
    let stdout = fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--mount",
            "apps/t3code",
            "--from",
            "../upstream",
            "--from-branch",
            "newgit",
        ],
    );
    assert!(stdout.contains("Imported `t3code`"), "{stdout}");
    assert!(stdout.contains("newgit merge main"), "{stdout}");
    assert!(fixture.store.join("apps/t3code/app.ts").is_file());

    fixture.ok(&fixture.store, &["merge", "main"]);
    let repo = fixture.remote.repo("jack.store.t.t3code");
    assert_eq!(git(&repo, &["rev-parse", "main"]), tip, "the very commit");
    assert_eq!(files_at(&repo, "main"), "app.ts");
}
