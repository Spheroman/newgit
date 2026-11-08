//! Tags from a linked project root: `git push origin <tag>` names a point of
//! `main` on the remote, then on each export target, composed as that target
//! sees it. A tag never moves, and it names only what `main` has.
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

impl Project {
    /// Tag `name` on the fake remote, as it answers it.
    fn remote_tag(&self, name: &str) -> Option<Value> {
        let state = self.fixture.remote.state.lock().expect("state");
        state
            .tags
            .values()
            .next()
            .and_then(|tags| tags.get(name).cloned())
    }
}

#[test]
fn a_tag_pushed_with_main_reaches_the_remote_and_every_target() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(
        &[
            ("ours.txt", "ours\n"),
            ("apps/t3code/theirs.ts", "theirs\n"),
        ],
        "release work",
    );
    git(&store, &["tag", "v1.0.0"]);

    let said = project.pushed(&store, &["origin", "main", "v1.0.0"]);
    assert!(said.contains("tagged `v1.0.0` on the remote"), "{said}");
    assert!(said.contains("tagged `v1.0.0` on `github`"), "{said}");
    assert!(said.contains("tagged `v1.0.0` on `t3code`"), "{said}");

    let tag = project
        .remote_tag("v1.0.0")
        .expect("the remote has the tag");
    assert_eq!(
        tag["list"]["root"].as_str(),
        Some(project.main_root().as_str())
    );
    assert!(tag["list"]["t3code"].is_string(), "{tag}");
    assert!(tag["annotation"].is_null(), "{tag}");

    // Each target has it at its own view of that commit: its branch's tip.
    assert_eq!(
        git(&project.mirror, &["rev-parse", "refs/tags/v1.0.0"]),
        git(&project.mirror, &["rev-parse", "main"])
    );
    assert_eq!(
        git(&project.t3code, &["rev-parse", "refs/tags/v1.0.0"]),
        git(&project.t3code, &["rev-parse", "newgit"])
    );

    // Pushed again, it is there already: nothing moves, and nothing fails.
    let again = project.pushed(&store, &["origin", "v1.0.0"]);
    assert!(again.contains("tagged `v1.0.0` on the remote"), "{again}");
    assert!(!again.contains("on `github`"), "{again}");
}

#[test]
fn an_annotated_tag_keeps_its_message_on_the_target() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(&[("ours.txt", "ours\n")], "release work");
    project.pushed(&store, &["origin", "main"]);
    git(&store, &["tag", "-a", "v2.0.0", "-m", "Release 2.0.0"]);

    project.pushed(&store, &["origin", "v2.0.0"]);
    let tag = project
        .remote_tag("v2.0.0")
        .expect("the remote has the tag");
    assert_eq!(
        tag["annotation"]["message"].as_str(),
        Some("Release 2.0.0\n")
    );
    assert!(
        tag["annotation"]["tagger"]
            .as_str()
            .is_some_and(|tagger| tagger.contains('<')),
        "{tag}"
    );

    assert_eq!(git(&project.mirror, &["cat-file", "-t", "v2.0.0"]), "tag");
    assert_eq!(
        git(&project.mirror, &["rev-parse", "v2.0.0^{commit}"]),
        git(&project.mirror, &["rev-parse", "main"])
    );
    let object = git(&project.mirror, &["cat-file", "tag", "v2.0.0"]);
    assert!(object.ends_with("Release 2.0.0"), "{object}");
}

#[test]
fn a_tag_on_work_main_does_not_have_is_refused_saying_to_push_main() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(&[("ours.txt", "ours\n")], "not pushed");
    git(&store, &["tag", "v3.0.0"]);

    let said = project.refused(&store, &["origin", "v3.0.0"]);
    assert!(said.contains("git push origin main v3.0.0"), "{said}");
    assert!(project.remote_tag("v3.0.0").is_none());
    let tags = git(&project.mirror, &["for-each-ref", "refs/tags"]);
    assert!(tags.is_empty(), "{tags}");
}

#[test]
fn a_tag_never_moves() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    git(&store, &["tag", "v4"]);
    project.pushed(&store, &["origin", "v4"]);
    let first = project.remote_tag("v4").expect("tagged");

    project.commit(&[("ours.txt", "ours\n")], "later");
    project.pushed(&store, &["origin", "main"]);
    git(&store, &["tag", "-f", "v4"]);
    let said = project.refused(&store, &["origin", "--force", "v4"]);
    assert!(said.contains("never moves"), "{said}");
    assert_eq!(project.remote_tag("v4"), Some(first));
}

#[test]
fn a_tag_comes_back_through_the_endpoint_at_the_projects_own_commit() {
    let (_guard, temp) = tempdir();
    let project = project(&temp);
    let store = project.fixture.store.clone();
    project.commit(&[("ours.txt", "ours\n")], "release work");
    let head = git(&store, &["rev-parse", "HEAD"]);
    git(&store, &["tag", "-a", "v5", "-m", "five"]);
    project.pushed(&store, &["origin", "main", "v5"]);

    // Somewhere that never had it: the tag arrives with a fetch, at the
    // commit the project root made.
    git(&store, &["tag", "-d", "v5"]);
    let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
    let fetched = Command::new("git")
        .args(["-C", store.as_str(), "fetch", "-q", "origin", "--tags"])
        .env("HOME", project.fixture.temp.join("home").as_str())
        .env(
            "PATH",
            format!(
                "{}:{}",
                binary.parent().expect("a directory"),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(git(&store, &["rev-parse", "v5^{commit}"]), head);
    assert_eq!(git(&store, &["cat-file", "-t", "v5"]), "tag");
}
