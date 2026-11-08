//! Two newgits in one store writing notes at once (two merges begun
//! together from one project root): both read the notes ref, both build on
//! it, and Git refused the second's update, failing its merge. Every note
//! must land. Its own file per AGENTS.md's "Working in parallel".

use super::write_notes;
use camino::Utf8PathBuf;

fn git(dir: &Utf8PathBuf, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn notes_written_at_once_all_land() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = Utf8PathBuf::from_path_buf(temp.path().join("store.git")).expect("utf8");
    git(
        &Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8"),
        &["init", "-q", "--bare", repo.as_str()],
    );
    let tree = git(&repo, &["mktree"]);
    let commits: Vec<String> = (0..8)
        .map(|index| {
            git(
                &repo,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit-tree",
                    &tree,
                    "-m",
                    &format!("c{index}"),
                ],
            )
        })
        .collect();
    for round in 0..3 {
        let threads: Vec<_> = commits
            .iter()
            .enumerate()
            .map(|(index, commit)| {
                let repo = repo.clone();
                let notes = vec![(commit.clone(), format!("note {round} {index}"))];
                std::thread::spawn(move || write_notes(&repo, "refs/notes/race", "race", &notes))
            })
            .collect();
        for thread in threads {
            let written = thread.join().expect("ran");
            assert!(written.is_ok(), "{written:?}");
        }
        for (index, commit) in commits.iter().enumerate() {
            assert_eq!(
                git(
                    &repo,
                    &["notes", "--ref", "refs/notes/race", "show", commit]
                ),
                format!("note {round} {index}")
            );
        }
    }
}
