//! The paths this machine holds back from being stored unencrypted (#338),
//! at the level of the file that records them: comparison ignores case, so
//! a claim spelled `.ENV.SHARED` on a case-insensitive filesystem is the
//! `.env.shared` it opens (runs on any filesystem); writers running at once
//! all land; a file that does not parse is an error, never an empty set.

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::tracker_repo::{held_sealed, note_held_sealed, release_sealed};

/// A tracker store's git dir under `.newgit/local/`; the held paths sit
/// beside it.
fn store(temp: &tempfile::TempDir) -> Utf8PathBuf {
    let local = Utf8Path::from_path(temp.path())
        .expect("utf8")
        .join(".newgit/local");
    std::fs::create_dir_all(&local).expect("mkdir");
    local.join("trackers.git")
}

#[test]
fn held_paths_are_compared_ignoring_case() {
    let temp = tempfile::tempdir().expect("temp");
    let git_dir = store(&temp);
    note_held_sealed(&git_dir, [".env.shared", "secrets/Key.pem"]).expect("note");
    let held = held_sealed(&git_dir).expect("read");
    assert!(held.holds(".env.shared"));
    assert!(held.holds(".ENV.SHARED"));
    assert!(held.holds(".Env.Shared"));
    assert!(held.holds("SECRETS/key.PEM"));
    assert!(!held.holds(".env.shared2"));
    assert!(!held.holds("secrets"));
    // Recorded as first spelled.
    assert!(held.paths().contains("secrets/Key.pem"));
}

#[test]
fn release_ignores_case_and_expands_a_directory() {
    let temp = tempfile::tempdir().expect("temp");
    let git_dir = store(&temp);
    note_held_sealed(
        &git_dir,
        [".env.shared", "secrets/a", "secrets/deep/b", "secretsX/c"],
    )
    .expect("note");

    let released = release_sealed(&git_dir, &[".ENV.SHARED".to_owned()]).expect("release");
    assert_eq!(released, vec![".env.shared".to_owned()]);

    let released = release_sealed(&git_dir, &["./SECRETS/".to_owned()]).expect("release");
    assert_eq!(
        released,
        vec!["secrets/a".to_owned(), "secrets/deep/b".to_owned()]
    );
    let held = held_sealed(&git_dir).expect("read");
    assert!(held.holds("secretsX/c"), "a sibling prefix is not under it");
    assert!(!held.holds("secrets/a"));

    assert!(
        release_sealed(&git_dir, &["nothing".to_owned()])
            .expect("release")
            .is_empty()
    );
}

#[test]
fn a_file_that_does_not_parse_is_an_error_naming_it() {
    let temp = tempfile::tempdir().expect("temp");
    let git_dir = store(&temp);
    note_held_sealed(&git_dir, [".env.shared"]).expect("note");
    let file = git_dir.parent().expect("parent").join("sealed-paths.json");
    let text = std::fs::read_to_string(&file).expect("read");
    std::fs::write(&file, &text[..text.len() - 3]).expect("truncate");

    let error = held_sealed(&git_dir).expect_err("refused").to_string();
    assert!(error.contains("sealed-paths.json"), "{error}");
    assert!(error.contains("does not parse"), "{error}");
    let error = note_held_sealed(&git_dir, ["other"])
        .expect_err("refused")
        .to_string();
    assert!(error.contains("sealed-paths.json"), "{error}");
    // Not overwritten with a fresh set that forgets `.env.shared`.
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        text[..text.len() - 3]
    );
    // No file at all is nothing held.
    std::fs::remove_file(&file).expect("rm");
    assert!(held_sealed(&git_dir).expect("read").is_empty());
}

/// Sixteen writers, each noting its own paths many times over, at once:
/// every path is recorded, and no writer fails.
#[test]
fn writers_at_once_all_land() {
    let temp = tempfile::tempdir().expect("temp");
    let git_dir = store(&temp);
    let threads: Vec<_> = (0..16)
        .map(|writer| {
            let git_dir = git_dir.clone();
            std::thread::spawn(move || {
                for round in 0..40 {
                    let path = format!("secrets/w{writer}-{round}");
                    note_held_sealed(&git_dir, [path.as_str()]).expect("note");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("writer");
    }
    let held = held_sealed(&git_dir).expect("read");
    assert_eq!(held.paths().len(), 16 * 40);
    let leftovers: Vec<_> = std::fs::read_dir(git_dir.parent().expect("parent"))
        .expect("ls")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
