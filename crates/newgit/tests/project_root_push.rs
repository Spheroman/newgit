//! `git push` in a linked project root publishes: `newgit merge main`, then
//! an export to each target whose trackers changed, each getting only its
//! own. Fetch stays on the endpoint; the push goes through the outbox, as a
//! workspace's does.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

#![allow(dead_code)]

include!("support/fake_remote.rs");

/// A project with `t3code` mounted, a mirror shipping `root` and `access`,
/// and a target rooted at t3code, both exported.
struct Project {
    fixture: Fixture,
    mirror: Utf8PathBuf,
    t3code: Utf8PathBuf,
}

fn project(temp: &Utf8Path) -> Project {
    let fixture = setup(temp);
    let store = fixture.store.clone();
    let upstream = temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "newgit"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "t3: first"]);
    let t3code = temp.join("t3code.git");
    git(
        temp,
        &["clone", "-q", "--bare", upstream.as_str(), t3code.as_str()],
    );
    let mirror = temp.join("mirror.git");
    git(
        temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    fixture.ok(
        &store,
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
    let dir = store.join(".newgit/exports");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("github.toml"),
        format!("url = \"{mirror}\"\ntrackers = [\"root\", \"access\"]\n"),
    )
    .expect("write");
    std::fs::write(
        dir.join("t3code.toml"),
        format!("url = \"{t3code}\"\nbranch = \"newgit\"\nroot = \"t3code\"\n"),
    )
    .expect("write");
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "targets"]);
    fixture.ok(&store, &["merge", "main"]);
    fixture.ok(&store, &["export", "--remote", "github"]);
    fixture.ok(&store, &["export", "--remote", "t3code"]);
    Project {
        fixture,
        mirror,
        t3code,
    }
}

impl Project {
    fn commit(&self, files: &[(&str, &str)], message: &str) {
        let store = &self.fixture.store;
        for (path, text) in files {
            let file = store.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(file, text).expect("write");
        }
        git(store, &["add", "-A"]);
        git(store, &["commit", "-q", "-m", message]);
    }

    /// `git push` as someone with newgit installed runs it, from `dir`.
    fn push(&self, dir: &Utf8Path, args: &[&str]) -> Output {
        let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
        let path = format!(
            "{}:{}",
            binary.parent().expect("a directory"),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("git")
            .arg("-C")
            .arg(dir.as_str())
            .arg("push")
            .args(args)
            .env("HOME", self.fixture.temp.join("home").as_str())
            .env("PATH", path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs")
    }

    fn pushed(&self, dir: &Utf8Path, args: &[&str]) -> String {
        let output = self.push(dir, args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        stderr
    }

    fn refused(&self, dir: &Utf8Path, args: &[&str]) -> String {
        let output = self.push(dir, args);
        assert!(!output.status.success(), "the push should have failed");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// `main`'s root commit on the fake remote.
    fn main_root(&self) -> String {
        let state = self.fixture.remote.state.lock().expect("state");
        let branches = state.branches.values().next().expect("a project");
        branches["main"]["root"].as_str().expect("root").to_owned()
    }
}

#[test]
fn one_push_updates_both_targets_with_their_own_part_from_any_directory() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(
        &[
            ("ours.txt", "ours\n"),
            ("apps/t3code/theirs.ts", "theirs\n"),
        ],
        "work on both",
    );
    let root = project.main_root();

    let said = project.pushed(&store.join("apps/t3code"), &["origin", "main"]);
    assert!(said.contains("exported to `github`"), "{said}");
    assert!(said.contains("exported to `t3code`"), "{said}");
    assert_ne!(project.main_root(), root, "`main` moved");

    let mirror = files_at(&project.mirror, "main");
    assert!(mirror.lines().any(|path| path == "ours.txt"), "{mirror}");
    assert!(!mirror.contains("theirs.ts"), "{mirror}");
    assert!(!mirror.contains("apps/t3code"), "{mirror}");

    let t3code = files_at(&project.t3code, "newgit");
    assert!(t3code.lines().any(|path| path == "theirs.ts"), "{t3code}");
    assert!(!t3code.contains("ours.txt"), "{t3code}");

    // Nothing new: it says so, and neither target moves.
    let mirror_tip = git(&project.mirror, &["rev-parse", "main"]);
    let t3code_tip = git(&project.t3code, &["rev-parse", "newgit"]);
    let again = project.pushed(&store, &["origin", "main"]);
    assert!(again.contains("`main` already has this"), "{again}");
    assert!(!again.contains("exported to"), "{again}");
    assert_eq!(git(&project.mirror, &["rev-parse", "main"]), mirror_tip);
    assert_eq!(git(&project.t3code, &["rev-parse", "newgit"]), t3code_tip);
}

#[test]
fn a_target_whose_trackers_did_not_change_is_not_touched() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    let mirror_tip = git(&project.mirror, &["rev-parse", "main"]);
    // Ahead, and it does not matter: this push has nothing for it.
    let clone = temp.join("pr");
    git(
        &temp,
        &["clone", "-q", project.mirror.as_str(), clone.as_str()],
    );
    std::fs::write(clone.join("pr.txt"), "pr\n").expect("write");
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "merged there"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let ahead = git(&clone, &["rev-parse", "HEAD"]);
    assert_ne!(ahead, mirror_tip);

    project.commit(&[("apps/t3code/theirs.ts", "theirs\n")], "t3code only");
    let said = project.pushed(&store, &["origin", "main"]);
    assert!(said.contains("exported to `t3code`"), "{said}");
    assert!(!said.contains("`github`"), "{said}");
    assert_eq!(git(&project.mirror, &["rev-parse", "main"]), ahead);
}

#[test]
fn a_failed_export_is_reported_names_the_rerun_and_the_merge_stays() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    let hook = project.mirror.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho no >&2\nexit 1\n").expect("write");
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&hook, mode).expect("chmod");
    let mirror_tip = git(&project.mirror, &["rev-parse", "main"]);
    project.commit(
        &[
            ("ours.txt", "ours\n"),
            ("apps/t3code/theirs.ts", "theirs\n"),
        ],
        "work on both",
    );
    let root = project.main_root();

    let said = project.refused(&store, &["origin", "main"]);
    assert!(said.contains("the export to `github` failed"), "{said}");
    assert!(said.contains("`newgit export --remote github`"), "{said}");
    assert!(said.contains("`main` has the merge"), "{said}");
    assert_ne!(project.main_root(), root, "the merge stays");
    assert_eq!(git(&project.mirror, &["rev-parse", "main"]), mirror_tip);
    assert!(
        files_at(&project.t3code, "newgit").contains("theirs.ts"),
        "the other target still got its part"
    );

    // Fixed, a second push finishes the job, and the first target is left be.
    std::fs::remove_file(&hook).expect("remove");
    let again = project.pushed(&store, &["origin", "main"]);
    assert!(again.contains("exported to `github`"), "{again}");
    assert!(!again.contains("exported to `t3code`"), "{again}");
    assert!(files_at(&project.mirror, "main").contains("ours.txt"));
}

#[test]
fn a_target_that_is_ahead_is_refused_naming_the_command_and_nothing_pulls() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    let clone = temp.join("pr");
    git(
        &temp,
        &["clone", "-q", project.mirror.as_str(), clone.as_str()],
    );
    std::fs::write(clone.join("pr.txt"), "pr\n").expect("write");
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "merged there"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let ahead = git(&clone, &["rev-parse", "HEAD"]);

    project.commit(&[("ours.txt", "ours\n")], "ours");
    let said = project.refused(&store, &["origin", "main"]);
    assert!(said.contains("`newgit pull --remote github`"), "{said}");
    assert_eq!(git(&project.mirror, &["rev-parse", "main"]), ahead);
    assert!(!store.join("pr.txt").exists(), "a push does not pull");
}

#[test]
fn a_refused_merge_fails_the_push_and_nothing_is_exported() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(
        &[(
            ".newgit/trackers/env.toml",
            "audience = \"user\"\nstorage = \"local\"\npaths = [\".env\"]\n",
        )],
        "env",
    );
    project.pushed(&store, &["origin", "main"]);
    let mirror_tip = git(&project.mirror, &["rev-parse", "main"]);
    let root = project.main_root();

    project.commit(&[(".env", "SECRET=1\n")], "oops");
    let said = project.refused(&store, &["origin", "main"]);
    assert!(said.contains(".env"), "{said}");
    assert!(!said.contains("export to"), "{said}");
    assert_eq!(project.main_root(), root);
    assert_eq!(git(&project.mirror, &["rev-parse", "main"]), mirror_tip);
}

#[test]
fn any_other_branch_is_refused_saying_to_spawn_an_instance() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    let root = project.main_root();
    git(&store, &["switch", "-q", "-c", "topic"]);
    project.commit(&[("topic.txt", "topic\n")], "topic");

    let said = project.refused(&store, &["origin", "topic"]);
    assert!(said.contains("newgit spawn"), "{said}");
    assert_eq!(project.main_root(), root);
    assert!(!files_at(&project.mirror, "main").contains("topic.txt"));

    // Nor a commit that is not `main`'s, pushed onto `main`.
    let said = project.refused(&store, &["origin", "topic:main"]);
    assert!(said.contains("newgit spawn"), "{said}");
    assert_eq!(project.main_root(), root);
}
