//! Split commits composed somewhere that does not have the originals — the
//! remote — come back as the originals, byte for byte: the message ends
//! exactly as it did (GitHub's merge commits have no final newline), and a
//! signed commit's signature is put back, so ids match everywhere a reader
//! can see every tracker. A reader who cannot gets rebuilt commits, unsigned.

use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, CANONICAL_NOTES_REF, TrackerTip};
use newgit_core::tracker::{Storage, TrackerDefinition};

const VIEW: &str = "refs/newgit/views/test";

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)], input: Option<&str>) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git runs");
    if let Some(input) = input {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[], None)
}

/// A commit object's exact bytes — no trimming, so a final newline counts.
fn raw_commit(dir: &Utf8Path, commit: &str) -> String {
    let output = Command::new("git")
        .args(["-C", dir.as_str(), "cat-file", "commit", commit])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{commit} is a commit in {dir}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn has(dir: &Utf8Path, commit: &str) -> bool {
    Command::new("git")
        .args(["-C", dir.as_str(), "cat-file", "-e", commit])
        .status()
        .expect("git runs")
        .success()
}

fn who(name: &str, time: i64) -> Vec<(&'static str, String)> {
    vec![
        ("GIT_AUTHOR_NAME", name.to_owned()),
        ("GIT_AUTHOR_EMAIL", format!("{name}@example.com")),
        ("GIT_AUTHOR_DATE", format!("@{time} +0000")),
        ("GIT_COMMITTER_NAME", name.to_owned()),
        ("GIT_COMMITTER_EMAIL", format!("{name}@example.com")),
        ("GIT_COMMITTER_DATE", format!("@{time} +0000")),
    ]
}

fn tracker(name: &str, paths: &[&str], excluded: &[&str]) -> TrackerDefinition {
    TrackerDefinition {
        name: name.to_owned(),
        audience: "project-devs".to_owned(),
        storage: Storage::Local,
        merge_with_source: true,
        paths: paths.iter().map(Utf8PathBuf::from).collect(),
        secret: false,
        maintainers: None,
        role: None,
        priority: 0,
        requires: Vec::new(),
        resources: Vec::new(),
        excluded: excluded.iter().map(Utf8PathBuf::from).collect(),
        definition_rev: "sha256:000000000000".to_owned(),
    }
}

/// A project composed from `root` and a carved `electron`, with a bare
/// `remote` beside it.
struct Project {
    _guard: tempfile::TempDir,
    temp: Utf8PathBuf,
    repo: Utf8PathBuf,
    remote: Utf8PathBuf,
    root: TrackerDefinition,
    electron: TrackerDefinition,
    /// The composed view the agent works on top of.
    base: String,
}

impl Project {
    fn new() -> Self {
        let guard = tempfile::tempdir().expect("tempdir");
        let temp = Utf8PathBuf::from_path_buf(guard.path().canonicalize().expect("canonicalize"))
            .expect("utf8 tempdir");
        let repo = temp.join("project");
        std::fs::create_dir_all(repo.join("apps/electron")).expect("mkdir");
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        std::fs::write(repo.join("apps/electron/main.js"), "v1\n").expect("write");
        git(&repo, &["add", "-A"]);
        git_env(
            &repo,
            &["commit", "-q", "--no-gpg-sign", "-m", "initial"],
            &who("ana", 1_000),
            None,
        );
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let root = tracker("root", &["."], &["apps/electron"]);
        let electron = tracker("electron", &["apps/electron"], &[]);
        let electron_tip = compose::carve(
            &repo,
            &head,
            &electron,
            "refs/newgit/trackers/electron/main",
        )
        .expect("carve")
        .expect("electron has history");
        let base = compose::compose(
            &repo,
            VIEW,
            &[
                TrackerTip {
                    definition: &root,
                    commit: head,
                },
                TrackerTip {
                    definition: &electron,
                    commit: electron_tip,
                },
            ],
            &[],
        )
        .expect("compose")
        .tip
        .expect("tip");
        git(&repo, &["reset", "-q", "--hard", &base]);
        let remote = temp.join("remote");
        std::fs::create_dir_all(&remote).expect("mkdir");
        git(&remote, &["init", "-q", "--bare"]);
        Project {
            _guard: guard,
            temp,
            repo,
            remote,
            root,
            electron,
            base,
        }
    }

    /// Sign commits made from here with a fresh SSH key; `None` when
    /// `ssh-keygen` is unavailable. Returns the allowed-signers file that
    /// verifies them.
    fn sign_commits(&self) -> Option<Utf8PathBuf> {
        let key = self.temp.join("signing-key");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f", key.as_str()])
            .status();
        if !made.is_ok_and(|status| status.success()) {
            return None;
        }
        for (name, value) in [("gpg.format", "ssh"), ("user.signingkey", key.as_str())] {
            git(&self.repo, &["config", name, value]);
        }
        let public = std::fs::read_to_string(format!("{key}.pub")).expect("public key");
        let allowed = self.temp.join("allowed-signers");
        std::fs::write(&allowed, format!("agent@example.com {public}")).expect("write");
        Some(allowed)
    }

    /// Write `files`, then commit them on top of HEAD with `message` exactly
    /// as given — `git commit` would add a final newline.
    fn commit(&self, files: &[(&str, &str)], message: &str, time: i64, sign: bool) -> String {
        for (path, contents) in files {
            std::fs::write(self.repo.join(path), contents).expect("write");
        }
        git(&self.repo, &["add", "-A"]);
        let tree = git(&self.repo, &["write-tree"]);
        let parent = git(&self.repo, &["rev-parse", "HEAD"]);
        let signing = if sign { "-S" } else { "--no-gpg-sign" };
        let commit = git_env(
            &self.repo,
            &["commit-tree", signing, &tree, "-p", &parent, "-F", "-"],
            &who("agent", time),
            Some(message),
        );
        git(&self.repo, &["reset", "-q", "--soft", &commit]);
        commit
    }

    /// Split the agent's work into the trackers and copy only the tracker
    /// commits to the remote, as a sync does: never the view commits.
    /// Returns each tracker's tip.
    fn split_to_remote(&self, tip: &str) -> (String, String) {
        let all = [self.root.clone(), self.electron.clone()];
        let outcome = compose::split(
            &self.repo,
            &self.base,
            tip,
            &[&self.root, &self.electron],
            &all,
        )
        .expect("split");
        let root = outcome.state["root"].clone();
        let electron = outcome.state["electron"].clone();
        git(&self.repo, &["update-ref", "refs/split/root", &root]);
        git(
            &self.repo,
            &["update-ref", "refs/split/electron", &electron],
        );
        git(
            &self.remote,
            &[
                "fetch",
                "-q",
                self.repo.as_str(),
                "refs/split/*:refs/split/*",
            ],
        );
        (root, electron)
    }

    /// Compose on the remote, with `electron` bound or not.
    fn compose_on_remote(&self, root: &str, electron: Option<&str>) -> String {
        let mut tips = vec![TrackerTip {
            definition: &self.root,
            commit: root.to_owned(),
        }];
        if let Some(electron) = electron {
            tips.push(TrackerTip {
                definition: &self.electron,
                commit: electron.to_owned(),
            });
        }
        compose::compose(
            &self.remote,
            VIEW,
            &tips,
            std::slice::from_ref(&self.electron),
        )
        .expect("compose on the remote")
        .tip
        .expect("tip")
    }
}

#[test]
fn a_split_commit_recomposes_elsewhere_with_its_message_and_id_intact() {
    let project = Project::new();
    // A mixed commit whose message has no final newline, as GitHub writes a
    // merge's, then one ending in a blank line.
    let first = project.commit(
        &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
        "Both at once (#1)\n\nCo-authored-by: Bo <bo@example.com>",
        2_000,
        false,
    );
    let second = project.commit(
        &[("README.md", "more docs\n")],
        "Root only\n\n",
        3_000,
        false,
    );

    let (root, electron) = project.split_to_remote(&second);
    for original in [&first, &second] {
        assert!(
            !has(&project.remote, original),
            "the remote never has the view commits"
        );
    }
    let rebuilt = project.compose_on_remote(&root, Some(&electron));
    assert_eq!(
        raw_commit(&project.remote, &format!("{rebuilt}^")),
        raw_commit(&project.repo, &first),
        "the rebuilt commit is the original, byte for byte"
    );
    assert_eq!(rebuilt, second, "so the ids are the originals'");
}

#[test]
fn signed_commits_come_back_signed_with_their_original_ids() {
    let project = Project::new();
    let Some(allowed) = project.sign_commits() else {
        eprintln!("ssh-keygen unavailable; skipping");
        return;
    };
    // Signed, unsigned after it, then signed again: each one after a
    // given-back commit has to move onto it to come back too.
    let first = project.commit(
        &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
        "Both at once (#1)",
        2_000,
        true,
    );
    let second = project.commit(
        &[("apps/electron/main.js", "v3\n")],
        "Electron\n",
        3_000,
        false,
    );
    let third = project.commit(
        &[("README.md", "more docs\n")],
        "Root only (#2)",
        4_000,
        true,
    );

    let (root, electron) = project.split_to_remote(&third);
    let rebuilt = project.compose_on_remote(&root, Some(&electron));
    assert_eq!(rebuilt, third, "the tip is the original commit");
    for original in [&first, &second, &third] {
        assert_eq!(
            raw_commit(&project.remote, original),
            raw_commit(&project.repo, original),
            "{original} is the original, byte for byte"
        );
    }
    assert!(raw_commit(&project.remote, &first).contains("\ngpgsig "));
    git_env(
        &project.remote,
        &[
            "-c",
            &format!("gpg.ssh.allowedSignersFile={allowed}"),
            "verify-commit",
            &first,
        ],
        &[],
        None,
    );
    let canonical = git(
        &project.remote,
        &["notes", "--ref", CANONICAL_NOTES_REF, "list"],
    );
    assert!(
        canonical.is_empty(),
        "an original needs no canonical note: {canonical}"
    );
}

#[test]
fn a_view_without_every_tracker_gets_rebuilt_commits_unsigned() {
    let project = Project::new();
    if project.sign_commits().is_none() {
        eprintln!("ssh-keygen unavailable; skipping");
        return;
    }
    let first = project.commit(
        &[("README.md", "docs\n"), ("apps/electron/main.js", "v2\n")],
        "Both at once (#1)",
        2_000,
        true,
    );

    let (root, _) = project.split_to_remote(&first);
    let rebuilt = project.compose_on_remote(&root, None);
    assert_ne!(rebuilt, first, "without electron the tree differs");
    assert!(
        !raw_commit(&project.remote, &rebuilt).contains("gpgsig"),
        "a signature is never put on a commit it does not cover"
    );
    assert_eq!(
        git(
            &project.remote,
            &["notes", "--ref", CANONICAL_NOTES_REF, "show", &rebuilt],
        ),
        format!("canonical {first}"),
        "the rebuilt commit names the original it stands for"
    );
}

#[test]
fn a_github_merge_comes_back_with_its_pgp_signature() {
    let project = Project::new();
    std::fs::write(project.repo.join("README.md"), "merged\n").expect("write");
    git(&project.repo, &["add", "-A"]);
    let tree = git(&project.repo, &["write-tree"]);
    // As GitHub writes a merge: PGP armor, whose blank line Git stores as a
    // lone space, and no final newline. The signature need not verify for
    // the id to come back; the bytes have to.
    let object = format!(
        "tree {tree}\nparent {}\nauthor DJ <dj@example.com> 2000 -0700\n\
         committer GitHub <noreply@github.com> 2000 -0700\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n wsFcBAABCAAQBQJq39A0CRC1aQ7uu5UhlAAACnEQAH4X\n \
         =6GVQ\n -----END PGP SIGNATURE-----\n \n\n\
         Fix the endpoint (#136)\n\nCo-authored-by: Bo <bo@example.com>",
        project.base
    );
    let merge = git_env(
        &project.repo,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        &[],
        Some(&object),
    );
    git(&project.repo, &["reset", "-q", "--soft", &merge]);

    let (root, electron) = project.split_to_remote(&merge);
    assert_eq!(
        git(
            &project.repo,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Newgit-Split,valueonly)",
                &root
            ],
        ),
        merge,
        "Git's own trailer parser still finds the split source"
    );
    let rebuilt = project.compose_on_remote(&root, Some(&electron));
    assert_eq!(rebuilt, merge, "the merge comes back as itself");
    assert_eq!(raw_commit(&project.remote, &merge), object);
}
