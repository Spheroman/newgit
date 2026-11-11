//! What a remote's refusal of a commit by id does and does not mean. A
//! remote may hold a commit and still refuse it because no branch names it,
//! so a refused pin is settled by that tracker's whole history; and a failure
//! that is the same for every commit is not worth halving a fetch to see.
//!
//! The remotes are real Git. Protocol v0 is what makes them refuse: a v2
//! `upload-pack` serves any object it has. New tests live in their own file
//! per AGENTS.md's "Working in parallel".

use std::collections::BTreeMap;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::endpoint::{Plan, Source, compose_view};
use newgit_core::source::GitSource;

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

fn script(path: &Utf8Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A repo of `count` commits: the commits, oldest first.
fn history(dir: &Utf8Path, count: usize) -> Vec<String> {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main"]);
    (0..count)
        .map(|n| {
            std::fs::write(dir.join("file.txt"), format!("{n}\n")).expect("write");
            git(dir, &["add", "-A"]);
            // Named for its repo: two repos must not make the same commit.
            let message = format!("{dir} commit {n}");
            git(dir, &["commit", "-q", "-m", &message]);
            git(dir, &["rev-parse", "HEAD"])
        })
        .collect()
}

fn bare(from: &Utf8Path, to: &Utf8Path, tip: &str) {
    git(from, &["init", "-q", "--bare", "-b", "main", to.as_str()]);
    git(
        from,
        &["push", "-q", to.as_str(), &format!("{tip}:refs/heads/main")],
    );
}

/// A root with a commit pinning `child`'s `pin`, its definition in root's
/// own tree: root's repo, and the root commit.
fn root_pinning(temp: &Utf8Path, pin: &str) -> (Utf8PathBuf, String) {
    let dir = temp.join("root");
    std::fs::create_dir_all(dir.join(".newgit/trackers")).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write");
    std::fs::write(
        dir.join(".newgit/trackers/root.toml"),
        "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\n",
    )
    .expect("write");
    std::fs::write(
        dir.join(".newgit/trackers/child.toml"),
        "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \"child\"\npaths = [\".\"]\n",
    )
    .expect("write");
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{pin},.newgit/pins/child"),
        ],
    );
    let tree = git(&dir, &["write-tree"]);
    let root = git(&dir, &["commit-tree", &tree, "-m", "mount the child"]);
    let repo = temp.join("root.git");
    bare(&dir, &repo, &root);
    (repo, root)
}

fn source(repo: &Utf8Path) -> Source {
    Source {
        url: repo.to_string(),
        auth: "Authorization: Basic eDpwdw==".to_owned(),
        namespace: None,
    }
}

/// A container whose shallow fetches speak protocol v0, where a remote can
/// refuse an id.
fn container_refused_ids(temp: &Utf8Path) -> Utf8PathBuf {
    let container = temp.join("container");
    let snapshots = container.join("snapshots.git");
    std::fs::create_dir_all(&container).expect("mkdir");
    git(temp, &["init", "-q", "--bare", snapshots.as_str()]);
    git(&snapshots, &["config", "protocol.version", "0"]);
    container
}

fn plan(container: &Utf8Path, root: (&Utf8Path, &str), child: (&Utf8Path, &str)) -> Plan {
    Plan {
        root: "root".to_owned(),
        dir: container.to_path_buf(),
        branches: BTreeMap::from([(
            "main".to_owned(),
            BTreeMap::from([
                ("root".to_owned(), root.1.to_owned()),
                ("child".to_owned(), child.1.to_owned()),
            ]),
        )]),
        default_branch: "main".to_owned(),
        tags: BTreeMap::new(),
        repos: BTreeMap::from([
            ("root".to_owned(), source(root.0)),
            ("child".to_owned(), source(child.0)),
        ]),
    }
}

/// `pin` is `child`'s, an older commit than its tip: the case a remote
/// serving only what a branch names refuses. `configure` makes it refuse.
fn composes_the_child_it_refused(configure: &[&str]) {
    let (_guard, temp) = tempdir();
    let commits = history(&temp.join("child"), 3);
    let pin = &commits[1];
    let child_repo = temp.join("child.git");
    bare(&temp.join("child"), &child_repo, &commits[2]);
    for setting in configure {
        git(&child_repo, &["config", setting, "true"]);
    }
    let (root_repo, root) = root_pinning(&temp, pin);

    let container = container_refused_ids(&temp);
    let view =
        compose_view(&plan(&container, (&root_repo, &root), (&child_repo, pin))).expect("composed");

    assert_eq!(git(&view, &["show", "main:child/file.txt"]), "1");
    assert_eq!(
        git(
            &container.join("trackers.git"),
            &["rev-parse", "refs/trackers/child/heads/main"]
        ),
        commits[2],
        "settled by the tracker's whole history"
    );
}

#[test]
fn a_remote_that_will_not_serve_an_unadvertised_id_still_composes_the_child() {
    // Git's client gives up before asking: `unadvertised object`.
    composes_the_child_it_refused(&[]);
}

#[test]
fn a_remote_answering_not_our_ref_still_composes_the_child() {
    // Tips by id, and nothing else: the server says `not our ref`.
    composes_the_child_it_refused(&["uploadpack.allowTipSHA1InWant"]);
}

#[test]
fn a_pin_no_branch_reaches_is_left_out_after_the_whole_history_says_so() {
    let (_guard, temp) = tempdir();
    let commits = history(&temp.join("child"), 2);
    let elsewhere = history(&temp.join("elsewhere"), 1);
    let child_repo = temp.join("child.git");
    bare(&temp.join("child"), &child_repo, &commits[1]);
    let (root_repo, root) = root_pinning(&temp, &elsewhere[0]);

    let container = container_refused_ids(&temp);
    let view = compose_view(&plan(
        &container,
        (&root_repo, &root),
        (&child_repo, &elsewhere[0]),
    ))
    .expect("served, without the child");

    let files = git(&view, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("child/"), "{files}");
    assert!(files.contains("README.md"), "{files}");
    assert_eq!(
        git(
            &container.join("trackers.git"),
            &["rev-parse", "refs/trackers/child/heads/main"]
        ),
        commits[1],
        "it looked"
    );
}

/// A bare store fetching remote `up`, whose `upload-pack` is `wrapper`.
fn store_with(temp: &Utf8Path, upstream: &Utf8Path, wrapper: &Utf8Path) -> Utf8PathBuf {
    let store = temp.join("store.git");
    git(temp, &["init", "-q", "--bare", store.as_str()]);
    git(
        &store,
        &["config", "remote.up.url", &format!("file://{upstream}")],
    );
    git(
        &store,
        &["config", "remote.up.uploadpack", wrapper.as_str()],
    );
    store
}

fn lines(path: &Utf8Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn snapshot_refs(store: &Utf8Path) -> usize {
    git(
        store,
        &["for-each-ref", "--format=%(refname)", "refs/pins/"],
    )
    .lines()
    .count()
}

#[test]
fn a_failure_every_commit_shares_is_returned_at_once() {
    let (_guard, temp) = tempdir();
    let commits = history(&temp.join("upstream"), 8);
    let calls = temp.join("calls");
    let wrapper = temp.join("refuse.sh");
    script(
        &wrapper,
        &format!(
            "#!/bin/sh\n\
             echo call >> '{calls}'\n\
             echo 'fatal: Authentication failed' >&2\n\
             exit 128\n"
        ),
    );
    let store = store_with(&temp, &temp.join("upstream"), &wrapper);

    let error = GitSource::fetch_snapshots(
        &[&format!("--git-dir={store}")],
        "up",
        "refs/pins/",
        &commits,
        None,
    )
    .expect_err("a password it will not take is a failure");

    assert!(
        error.to_string().contains("Authentication failed"),
        "{error}"
    );
    assert_eq!(lines(&calls), 1, "one fetch, not one per halving");
}

#[test]
fn a_dropped_fetch_is_halved_and_every_commit_arrives() {
    let (_guard, temp) = tempdir();
    let upstream = temp.join("upstream");
    let commits = history(&upstream, 4);
    git(
        &upstream,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );

    // The first pack fails, as Artifacts' does; every later one is served.
    let count = temp.join("packs");
    let hook = temp.join("pack-hook.sh");
    script(
        &hook,
        &format!(
            "#!/bin/sh\n\
             echo pack >> '{count}'\n\
             if [ \"$(wc -l < '{count}')\" -le 1 ]; then exit 1; fi\n\
             exec \"$@\"\n"
        ),
    );
    let wrapper = temp.join("upload-pack.sh");
    script(
        &wrapper,
        &format!(
            "#!/bin/sh\n\
             exec git -c uploadpack.packObjectsHook='{hook}' upload-pack \"$@\"\n"
        ),
    );
    let store = store_with(&temp, &upstream, &wrapper);

    let absent = GitSource::fetch_snapshots(
        &[&format!("--git-dir={store}")],
        "up",
        "refs/pins/",
        &commits,
        None,
    )
    .expect("halved, and through");

    assert!(absent.is_empty(), "{absent:?}");
    assert_eq!(snapshot_refs(&store), 4);
    assert_eq!(lines(&count), 3, "the batch, then each half");
}

#[test]
fn a_batch_with_a_refused_commit_is_halved_down_to_it() {
    let (_guard, temp) = tempdir();
    let upstream = temp.join("upstream");
    let commits = history(&upstream, 3);
    let elsewhere = history(&temp.join("elsewhere"), 1);
    let wrapper = temp.join("upload-pack.sh");
    script(&wrapper, "#!/bin/sh\nexec git upload-pack \"$@\"\n");
    let store = store_with(&temp, &upstream, &wrapper);

    let mut asked = commits.clone();
    asked.push(elsewhere[0].clone());
    let absent = GitSource::fetch_snapshots(
        &[&format!("--git-dir={store}")],
        "up",
        "refs/pins/",
        &asked,
        None,
    )
    .expect("the refusal is an answer");

    assert_eq!(absent, elsewhere);
    assert_eq!(snapshot_refs(&store), 3);
}
