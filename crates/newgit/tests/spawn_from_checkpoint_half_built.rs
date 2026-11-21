//! A `spawn --from-checkpoint` that fails after the instance is made keeps
//! it for inspection, as a plain spawn that fails midway does, and says
//! the exact commands that finish it or remove it, so the retry is not
//! refused by a record nobody explained.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;

/// A project whose resource `db` captures fine in instance `src` but
/// fails in any other (`$NEWGIT_BRANCH`), restoring with `restore`; and a
/// checkpoint `src/ckpt_001`.
fn checkpointed(tag: &str, restore: &str) -> (TempDir, Utf8PathBuf) {
    let (guard, temp) = tempdir(tag);
    let store = setup(&temp);
    std::fs::write(
        store.paths().resources.join("db.toml"),
        format!(
            "ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\ncommand = 'test \
             \"$NEWGIT_BRANCH\" = src || exit 3; echo dump'\n\n[restore]\nmode = \
             \"command\"\ncommand = \"{restore}\"\n"
        ),
    )
    .expect("write resource");
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "src"]);
    newgit_ok(&repo, &["checkpoint", "src"]);
    (guard, repo)
}

#[test]
fn a_failed_spawn_names_the_commands_that_finish_or_remove_it() {
    let (_guard, repo) = checkpointed("half-built-error", "true");
    let run = refused(
        &repo,
        &["spawn", "copy", "--from-checkpoint", "src/ckpt_001"],
        "`copy` was spawned but not restored to src/ckpt_001",
    );
    assert!(
        run.stderr.contains("`newgit undo copy --to ckpt_001`")
            && run.stderr.contains("`newgit remove copy --purge`"),
        "{}",
        run.stderr
    );
    // Kept, as said.
    let status = newgit_ok(&repo, &["status"]);
    assert!(status.stdout.contains("copy"), "{}", status.stdout);
    // And the removal it names makes the retry possible.
    newgit_ok(&repo, &["remove", "copy", "--purge"]);
    refused(
        &repo,
        &["spawn", "copy", "--from-checkpoint", "src/ckpt_001"],
        "`copy` was spawned but not restored",
    );
}

#[test]
fn a_name_already_taken_is_not_called_half_built() {
    let (_guard, repo) = checkpointed("half-built-taken", "true");
    let run = refused(
        &repo,
        &["spawn", "src", "--from-checkpoint", "src/ckpt_001"],
        "collides with existing record",
    );
    assert!(!run.stderr.contains("was spawned but"), "{}", run.stderr);
}

#[test]
fn a_restore_that_fails_names_the_undo_that_retries_it() {
    let (_guard, repo) = checkpointed("half-built-restore", "exit 4");
    // The new instance's own capture must not be what fails here.
    std::fs::write(
        repo.join(".newgit/resources/db.toml"),
        "ownership = \"branch\"\n\n[checkpoint]\nmode = \"command\"\ncommand = \"echo \
         dump\"\n\n[restore]\nmode = \"command\"\ncommand = \"exit 4\"\n",
    )
    .expect("write resource");
    let run = newgit(
        &repo,
        &["spawn", "copy", "--from-checkpoint", "src/ckpt_001"],
    );
    assert!(!run.status.success(), "{}", run.stdout);
    assert!(
        run.stdout
            .contains("retry:     fix what failed, then `newgit undo copy --to ckpt_001`")
            && run.stdout.contains("`newgit remove copy --purge`"),
        "{}",
        run.stdout
    );
}
