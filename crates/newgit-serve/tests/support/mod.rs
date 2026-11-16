// What the landing log's tests share: a server on a scratch data directory,
// a project made by a landing, and landings that make or move a branch.

#![allow(dead_code, reason = "each test file uses some of these")]

use camino::{Utf8Path, Utf8PathBuf};
use newgit_serve::db::{self, List};
use newgit_serve::landing::{Change, Entry, Expect, Landing, Made, Move, RefMove};
use newgit_serve::repos::{Repo, Store};
use newgit_serve::{App, Config};

pub fn tempdir(name: &str) -> Utf8PathBuf {
    let dir = std::env::temp_dir().join(format!("newgit-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8")
}

pub fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub fn config(data: &Utf8Path) -> Config {
    Config {
        data: data.to_owned(),
        listen: "127.0.0.1:0".parse().expect("address"),
        url: None,
        allowed_logins: "*".to_owned(),
        blocked_logins: String::new(),
        admin_logins: String::new(),
        projects_per_login: 5,
        github_client_id: None,
        dev_login_secret: None,
        git_slots: 4,
    }
}

pub const HOME: &str = "jack/p";

/// A commit id no store holds.
pub const BOGUS: &str = "dead00000000000000000000000000000000beef";

/// The incarnation of the project at `home`.
pub fn incarnation(app: &App, home: &str) -> String {
    db::project(&app.db.lock(), home)
        .expect("project")
        .expect("there is one")
        .incarnation
}

/// Make project `home` by a landing; returns its incarnation.
pub fn make_project(app: &App, home: &str) -> String {
    let (owner, name) = home.split_once('/').expect("owner/name");
    let incarnation = format!("{:032x}", rand_u128());
    app.log
        .land(
            app,
            Landing {
                entry: Entry {
                    by: owner.to_owned(),
                    homes: [(
                        home.to_owned(),
                        Change {
                            incarnation: incarnation.clone(),
                            made: Some(Made {
                                owner: owner.to_owned(),
                                name: name.to_owned(),
                            }),
                            ..Change::default()
                        },
                    )]
                    .into(),
                    ..Entry::default()
                },
                ..Landing::default()
            },
        )
        .expect("the project is made");
    incarnation
}

fn rand_u128() -> u128 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut high = std::collections::hash_map::RandomState::new().build_hasher();
    high.write_u64(1);
    let mut low = std::collections::hash_map::RandomState::new().build_hasher();
    low.write_u64(2);
    (u128::from(high.finish()) << 64) | u128::from(low.finish())
}

/// Project `home`'s `root`, made in `project-devs.git` and landed there,
/// holding `commits` commits (pushed from `<data>/work-<home>`); returns
/// the repo and the commits, oldest first.
pub fn project_with_root(app: &App, home: &str, commits: usize) -> (Repo, Vec<String>) {
    let incarnation = make_project(app, home);
    let data = &app.config.data;
    let root = Repo::new(
        &data.join("homes").join(home),
        Store::ProjectDevs,
        "t/root".into(),
    );
    root.create().expect("root");
    let work = data.join(format!("work-{}", home.replace('/', "-")));
    if !work.exists() {
        git(data, &["init", "-q", "-b", "main", work.as_str()]);
    }
    let mut made = Vec::new();
    for n in 0..commits {
        std::fs::write(work.join("file"), format!("{home} {n} {:x}", rand_u128())).expect("write");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &n.to_string()]);
        made.push(git(&work, &["rev-parse", "HEAD"]));
    }
    if commits > 0 {
        git(
            &work,
            &[
                "push",
                "-q",
                "-f",
                root.dir().as_str(),
                &format!("HEAD:{}refs/heads/keep", root.prefix()),
            ],
        );
    }
    app.log
        .land(
            app,
            Landing {
                entry: Entry {
                    by: "jack".into(),
                    homes: [(
                        home.to_owned(),
                        Change {
                            incarnation,
                            stores: [(
                                "root".to_owned(),
                                Move {
                                    from: None,
                                    to: "project-devs".into(),
                                },
                            )]
                            .into(),
                            ..Change::default()
                        },
                    )]
                    .into(),
                    ..Entry::default()
                },
                ..Landing::default()
            },
        )
        .expect("root's row lands");
    (root, made)
}

/// A server with project `jack/p` whose `root` is in `project-devs.git`,
/// holding `commits` commits; returns them, oldest first.
pub fn server(data: &Utf8Path, commits: usize) -> (App, Repo, Vec<String>) {
    let app = App::open(config(data)).expect("opens");
    let (root, made) = project_with_root(&app, HOME, commits);
    (app, root, made)
}

/// Branch `branch` of `home` moved from `base` (`None`: made) to root at
/// `commit`.
pub fn landing_in(
    app: &App,
    home: &str,
    branch: &str,
    base: Option<&str>,
    commit: &str,
) -> Landing {
    let list: List = [("root".to_owned(), commit.to_owned())].into();
    Landing {
        entry: Entry {
            by: "jack".into(),
            homes: [(
                home.to_owned(),
                Change {
                    incarnation: incarnation(app, home),
                    branches: [(branch.to_owned(), list)].into(),
                    refs: [(
                        "root".to_owned(),
                        [(
                            format!("refs/heads/{branch}"),
                            RefMove::to(Some(commit.to_owned())),
                        )]
                        .into(),
                    )]
                    .into(),
                    ..Change::default()
                },
            )]
            .into(),
            ..Entry::default()
        },
        expect: vec![Expect {
            home: home.to_owned(),
            branch: branch.to_owned(),
            list: base.map(|base| [("root".to_owned(), base.to_owned())].into()),
        }],
        ..Landing::default()
    }
}

/// [`landing_in`] for [`HOME`].
pub fn landing(app: &App, branch: &str, base: Option<&str>, commit: &str) -> Landing {
    landing_in(app, HOME, branch, base, commit)
}

pub fn root_of(app: &App, home: &str, branch: &str) -> Option<String> {
    let db = app.db.lock();
    db::list_of(&db, home, branch)
        .expect("list")
        .map(|list| list["root"].clone())
}

/// Lock the store at `dir` against ref updates, as a disk that refuses
/// writes would: every transaction there fails until [`unjam`].
pub fn jam(dir: &Utf8Path) {
    std::fs::write(dir.join("reftable/tables.list.lock"), "").expect("lock");
}

pub fn unjam(dir: &Utf8Path) {
    std::fs::remove_file(dir.join("reftable/tables.list.lock")).expect("unlock");
}
