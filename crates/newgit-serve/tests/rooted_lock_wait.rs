// The background extension of stored rooted histories (#412's review, item
// 1) must not hold the server's db lock, which every request and every
// landing takes, while it runs Git. A project of `TRACKERS` trackers mounted
// under `pkgs/` in root, with definitions read from `access` as the server
// reads them, behind the real server's App with its real background thread:
// right after a landing returns, API requests are made back to back for as
// long as the extension runs, and the longest any of them waits is held to
// a bound. Before the fix, at 150 trackers, one waited 2.4 s (debug).

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use newgit_core::objects::{EMPTY_TREE, Entry, GITLINK_MODE, Objects};
use newgit_serve::api::{self, ApiRequest};
use newgit_serve::db::List;
use newgit_serve::landing::{Change, Entry as LandingEntry, Expect, Landing, Move};
use newgit_serve::repos::{Repo, Store};
use newgit_serve::{App, rooted};
use support::{HOME, config, incarnation, make_project, tempdir};

const TRACKERS: usize = 300;
const SECRET: &str = "lock-wait-secret";

/// The longest a request may wait right after a landing. What is left under
/// the lock is reading the project's rows and definitions, which every
/// request that names the project does too.
const BOUND: Duration = Duration::from_millis(400);

const NOBODY: &[u8] = b"Ana <ana@example.com> 1700000000 +0000";

fn name(index: usize) -> String {
    format!("pkg{index:03}")
}

fn request(method: &str, path: &str, bearer: Option<&str>, body: &str) -> ApiRequest {
    ApiRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        query: BTreeMap::new(),
        bearer: bearer.map(str::to_owned),
        dev_login: Some(SECRET.to_owned()),
        headers: Default::default(),
        origin: "http://localhost".to_owned(),
        body: body.as_bytes().to_vec(),
    }
}

fn root_commit(
    objects: &mut Objects,
    pins: &BTreeMap<String, String>,
    definitions: &str,
    parent: Option<&str>,
) -> String {
    let pins = objects.make_tree(
        pins.iter()
            .map(|(name, commit)| Entry {
                mode: GITLINK_MODE.to_vec(),
                name: name.as_bytes().to_vec(),
                id: commit.clone(),
            })
            .collect(),
    );
    let readme = objects.make_blob(format!("root {}\n", parent.unwrap_or("-")).as_bytes());
    let mut tree = objects
        .set_path(EMPTY_TREE, "README.md", Some((b"100644".to_vec(), readme)))
        .expect("set");
    tree = objects
        .set_path(&tree, ".newgit/pins", Some((b"40000".to_vec(), pins)))
        .expect("set");
    tree = objects
        .set_path(
            &tree,
            ".newgit/trackers",
            Some((b"40000".to_vec(), definitions.to_owned())),
        )
        .expect("set");
    let parents: Vec<String> = parent.map(str::to_owned).into_iter().collect();
    objects.make_commit(&tree, &parents, NOBODY, NOBODY, b"", b"root\n")
}

fn leaf_commit(objects: &mut Objects, leaf: &str, serial: usize, parent: Option<&str>) -> String {
    let blob = objects.make_blob(format!("{leaf} {serial}\n").as_bytes());
    let tree = objects
        .set_path(EMPTY_TREE, "package.nix", Some((b"100644".to_vec(), blob)))
        .expect("set");
    let parents: Vec<String> = parent.map(str::to_owned).into_iter().collect();
    objects.make_commit(&tree, &parents, NOBODY, NOBODY, b"", leaf.as_bytes())
}

fn state(devs: &Utf8Path) -> Option<String> {
    let output = newgit_core::git::command()
        .args([
            "--git-dir",
            devs.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/newgit/rooted/root",
        ])
        .output()
        .expect("git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn landing(app: &App, list: &List, before: Option<&List>, stores: bool) -> Landing {
    Landing {
        entry: LandingEntry {
            by: "jack".into(),
            homes: [(
                HOME.to_owned(),
                Change {
                    incarnation: incarnation(app, HOME),
                    branches: [("main".to_owned(), list.clone())].into(),
                    stores: if stores {
                        list.keys()
                            .map(|name| {
                                (
                                    name.clone(),
                                    Move {
                                        from: None,
                                        to: "project-devs".into(),
                                    },
                                )
                            })
                            .collect()
                    } else {
                        BTreeMap::new()
                    },
                    ..Change::default()
                },
            )]
            .into(),
            ..LandingEntry::default()
        },
        expect: vec![Expect {
            home: HOME.to_owned(),
            branch: "main".to_owned(),
            list: before.cloned(),
        }],
        ..Landing::default()
    }
}

#[test]
fn a_request_right_after_a_landing_never_waits_for_the_extension() {
    let data = tempdir("rooted-lock-wait");
    let app = Arc::new(
        App::open(newgit_serve::Config {
            dev_login_secret: Some(SECRET.to_owned()),
            ..config(&data)
        })
        .expect("opens"),
    );
    make_project(&app, HOME);
    let home = data.join("homes").join(HOME);
    let devs = Store::ProjectDevs.dir(&home);
    let repo = |tracker: &str| Repo::tracker(&home, Some(Store::ProjectDevs), tracker);
    for tracker in ["root", "access"]
        .into_iter()
        .map(str::to_owned)
        .chain((0..TRACKERS).map(name))
    {
        repo(&tracker).create().expect("made");
    }

    // Every package, its definition in `access` (as the server reads them)
    // and in root (as composing reads them), and root pinning them all.
    let mut objects = Objects::open(&devs).expect("objects");
    let mut pins = BTreeMap::new();
    let mut definitions = Vec::new();
    for index in 0..TRACKERS {
        let leaf = name(index);
        pins.insert(leaf.clone(), leaf_commit(&mut objects, &leaf, 0, None));
        let text = format!("audience = \"project-devs\"\ngit = true\nmount = \"pkgs/{leaf}\"\n");
        definitions.push(Entry {
            mode: b"100644".to_vec(),
            name: format!("{leaf}.toml").into_bytes(),
            id: objects.make_blob(text.as_bytes()),
        });
    }
    let definitions = objects.make_tree(definitions);
    let access = objects.make_commit(&definitions, &[], NOBODY, NOBODY, b"", b"access\n");
    let root = root_commit(&mut objects, &pins, &definitions, None);
    objects.flush().expect("flush");
    drop(objects);
    support::git(
        &devs,
        &[
            "update-ref",
            &repo("access").full("refs/heads/main"),
            &access,
        ],
    );
    let mut list: List = pins.clone();
    list.insert("root".into(), root.clone());
    list.insert("access".into(), access.clone());
    app.log
        .land(&app, landing(&app, &list, None, true))
        .expect("the project lands");

    // The real background thread: the backfill first.
    rooted::start(&app);
    let started = Instant::now();
    while !app
        .rooted
        .backfilled
        .load(std::sync::atomic::Ordering::SeqCst)
        || state(&devs).is_none()
    {
        assert!(
            started.elapsed() < Duration::from_secs(900),
            "the backfill ends"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Let the landing's own entry drain too.
    std::thread::sleep(Duration::from_millis(500));
    eprintln!(
        "backfill of {TRACKERS} trackers: {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let login = api::handle(
        &app,
        &request("POST", "/v1/dev/login", None, r#"{"login":"jack"}"#),
    )
    .expect("dev login");
    let token = login.body["token"].as_str().expect("a token").to_owned();
    let me = request("GET", "/v1/me", Some(&token), "");

    // One package changes, root's pin follows: a landing like any other.
    let before_state = state(&devs);
    let mut objects = Objects::open(&devs).expect("objects");
    let leaf = name(7);
    let moved = leaf_commit(&mut objects, &leaf, 1, Some(&pins[&leaf]));
    let mut after = pins.clone();
    after.insert(leaf.clone(), moved.clone());
    let next_root = root_commit(&mut objects, &after, &definitions, Some(&root));
    objects.flush().expect("flush");
    drop(objects);
    let mut next = list.clone();
    next.insert(leaf.clone(), moved);
    next.insert("root".into(), next_root);
    let landed_at = Instant::now();
    app.log
        .land(&app, landing(&app, &next, Some(&list), false))
        .expect("lands");

    // Requests back to back while the extension runs.
    let mut worst = Duration::ZERO;
    let mut requests = 0;
    while state(&devs) == before_state {
        assert!(
            landed_at.elapsed() < Duration::from_secs(300),
            "the extension ends"
        );
        let asked = Instant::now();
        let answer = api::handle(&app, &me).expect("answered");
        assert_eq!(answer.status, 200);
        worst = worst.max(asked.elapsed());
        requests += 1;
    }
    eprintln!(
        "extension after the landing: {:.0} ms; {requests} requests meanwhile, the longest waited {:.1} ms",
        landed_at.elapsed().as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0
    );
    assert!(requests > 0, "a request was made while it ran");
    assert!(
        worst < BOUND,
        "a request waited {worst:?} right after a landing (bound {BOUND:?})"
    );
    let _ = std::fs::remove_dir_all(&data);
}
