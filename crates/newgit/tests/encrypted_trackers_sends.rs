//! What a send of an encrypted tracker (#338) refuses or rebuilds: an
//! unsent history with a merge commit in it is refused (a rebuild of its
//! first-parent line would still send the other parents), and what this
//! machine counts as already on the remote is forgotten when the project is
//! linked again, so commits encrypted to a stale set are rebuilt for the
//! new one rather than sent as "already there".

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

/// `env` (project-devs, synced) owning `.env.shared`, and `feature-a` with
/// the secret in it, checkpointed.
fn checkpointed(fixture: &Fixture) -> Utf8PathBuf {
    fixture.ok(
        &fixture.store,
        &["tracker", "create", "env", "--storage", "remote"],
    );
    fixture.ok(&fixture.store, &["tracker", "track", "env", ".env.shared"]);
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "env"]);
    fixture.ok(&fixture.store, &["merge", "main"]);
    fixture.ok(&fixture.store, &["spawn", "feature-a"]);
    let workspace = workspace_of(fixture, "feature-a");
    std::fs::write(workspace.join(".env.shared"), SECRET).expect("write");
    fixture.ok(&workspace, &["checkpoint"]);
    workspace
}

fn trackers_git(fixture: &Fixture) -> Utf8PathBuf {
    fixture.store.join(".newgit/local/trackers.git")
}

/// Git in the tracker store, with an identity for commits made by hand.
fn store_git(fixture: &Fixture, args: &[&str]) -> String {
    let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
    full.extend_from_slice(args);
    git(&trackers_git(fixture), &full)
}

/// `bytes` decrypted with the SSH key at `private`, if it opens them.
fn opens(private: &Utf8Path, bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let text = std::fs::read_to_string(private).expect("key");
    let identity = age::ssh::Identity::from_buffer(text.as_bytes(), None).expect("ssh key");
    let decryptor = age::Decryptor::new(bytes).ok()?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .ok()?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Every plaintext the key at `private` opens among the age files `repo`
/// holds, reachable or not.
fn readable_by(repo: &Utf8Path, private: &Utf8Path) -> Vec<String> {
    if !repo.exists() {
        return Vec::new();
    }
    let objects = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    let mut out = Vec::new();
    for line in objects.lines() {
        let Some((oid, "blob")) = line.split_once(' ') else {
            continue;
        };
        let bytes = git_output(repo, &["cat-file", "blob", oid]).stdout;
        if let Some(plain) = opens(private, &bytes) {
            out.push(String::from_utf8_lossy(&plain).into_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A merge commit (made by hand in the store; newgit never makes one in a
/// tracker outside Git) bound in the instance: checkpoint keeps it here
/// and fails the command, `tracker merge` refuses, and the remote never
/// gets the merge or its second parent.
#[test]
fn an_unsent_merge_commit_is_refused() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);

    std::fs::write(workspace.join(".env.shared"), "API_KEY=two\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", "env"]);
    let two = store_git(
        &fixture,
        &["rev-parse", "refs/trackers/env/instances/feature-a"],
    );
    std::fs::write(workspace.join(".env.shared"), "API_KEY=side\n").expect("write");
    fixture.ok(&workspace, &["tracker", "capture", "env"]);
    let side = store_git(
        &fixture,
        &["rev-parse", "refs/trackers/env/instances/feature-a"],
    );
    let tree = store_git(&fixture, &["rev-parse", &format!("{two}^{{tree}}")]);
    let merge = store_git(
        &fixture,
        &[
            "commit-tree",
            &tree,
            "-p",
            &two,
            "-p",
            &side,
            "-m",
            "by hand",
        ],
    );
    let short = &merge[..12];
    store_git(
        &fixture,
        &[
            "update-ref",
            &format!("refs/trackers/env/pins/{short}"),
            &merge,
        ],
    );
    fixture.ok(&workspace, &["tracker", "checkout", "env", "--rev", short]);

    let checkpoint = fixture.newgit(&workspace, &["checkpoint"]);
    let said = all(&checkpoint);
    assert!(!checkpoint.status.success(), "{said}");
    assert!(
        said.contains(&format!("commit {short} is a merge")),
        "{said}"
    );
    assert!(said.contains("Checkpoint ckpt_"), "kept here: {said}");
    let merge_out = fixture.newgit(&workspace, &["tracker", "merge", "env"]);
    assert!(!merge_out.status.success(), "{}", all(&merge_out));
    assert!(all(&merge_out).contains("is a merge"));

    let repo = fixture.remote.repo("jack.store.t.env");
    for commit in [&merge, &side] {
        let there = git_output(&repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]);
        assert!(!there.status.success(), "{commit} reached the remote");
    }
}

/// `env`'s `main`, merged and sent while ana was a member, encrypted to
/// her. The project is deleted and made again under the same name, with
/// jack as its only member. What this machine sent the old project does
/// not count as on the new one, so `members reencrypt` rebuilds that
/// history for jack alone: ana opens nothing the new project holds.
#[test]
fn a_project_linked_again_counts_nothing_as_already_there() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let workspace = checkpointed(&fixture);
    fixture.ok(&workspace, &["tracker", "merge", "env"]);
    let ana = temp.join("ana/.ssh/id_ed25519");
    let repo = fixture.remote.repo("jack.store.t.env");
    assert_eq!(readable_by(&repo, &ana), vec![SECRET], "ana was a member");

    fixture.ok(&fixture.store, &["remote", "delete", "jack/store"]);
    assert!(!repo.exists(), "the old project's repos are gone");
    fixture.ok(
        &fixture.store,
        &["remote", "add", "--url", &fixture.remote.url],
    );
    fixture.remote.state.lock().expect("state").members = Some(vec!["jack".to_owned()]);

    let reencrypt = fixture.newgit(&fixture.store, &["members", "reencrypt"]);
    assert!(reencrypt.status.success(), "{}", all(&reencrypt));
    assert!(repo.exists(), "env's main was published to the new project");
    assert_eq!(
        readable_by(&repo, &ana),
        Vec::<String>::new(),
        "ana opens nothing on the new project: {}",
        all(&reencrypt)
    );
    let jack = temp.join("home/.ssh/id_ed25519");
    assert_eq!(readable_by(&repo, &jack), vec![SECRET]);
}
