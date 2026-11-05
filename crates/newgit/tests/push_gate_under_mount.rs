//! The push gate judges a mounted tracker's commits where the view has them.
//! A tracker stores its files at its own root, so an imported t3code's
//! `AGENTS.md` is `apps/t3code/AGENTS.md` in the view, not the `AGENTS.md`
//! a tracker outside Git (agent notes) owns at the top. Publishing the
//! import with `newgit merge main` was refused as if it carried the notes
//! (found bringing the real Spheroman/t3code in, #144).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A tracker outside Git owning `AGENTS.md` at the top, as `agent-notes`
/// does in newgit itself.
fn with_agent_notes(fixture: &Fixture) {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "notes", "--audience", "user"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "notes", "AGENTS.md"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "agent notes"]);
}

/// An outside repository with its own `AGENTS.md` at its root.
fn upstream_with_agents_md(fixture: &Fixture) -> Utf8PathBuf {
    let upstream = fixture.temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "newgit"]);
    std::fs::write(upstream.join("AGENTS.md"), "t3code's own notes\n").expect("write");
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
    upstream
}

#[test]
fn an_imported_trackers_own_agents_md_is_not_the_notes_at_the_top() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    with_agent_notes(&fixture);
    fixture.ok(&fixture.store, &["merge", "main"]);
    let upstream = upstream_with_agents_md(&fixture);

    fixture.ok(
        &fixture.store,
        &[
            "tracker",
            "create",
            "t3code",
            "--git",
            "--mount",
            "apps/t3code",
            "--from",
            upstream.as_str(),
        ],
    );
    assert!(
        fixture.store.join("apps/t3code/AGENTS.md").exists(),
        "the import's own AGENTS.md sits under the mount"
    );

    // Publishing it sends t3code's history, whose `AGENTS.md` is t3code's.
    fixture.ok(&fixture.store, &["merge", "main"]);
    let shown = fixture.ok(&fixture.store, &["remote", "show"]);
    assert!(
        shown.contains("t3code"),
        "t3code reached the remote:\n{shown}"
    );
}
