// A data directory from before stores (one bare repository per tracker, per
// user's content, and per private fork) moves to one store per audience when
// `newgit serve` opens it (#409), and is served from there.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_serve::{App, Config, db};
use serde_json::Value;

const SECRET: &str = "migrate-test";

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git")
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

/// A bare repository as the old layout made one (loose refs, `main` its
/// default), holding one commit of `files`. Returns the commit.
fn old_repo(scratch: &Utf8Path, dir: &Utf8Path, files: &[(&str, &str)]) -> String {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(
        dir,
        &["init", "-q", "--bare", "-b", "main", "--ref-format=files"],
    );
    let work = scratch.join(format!("work-{}", dir.as_str().replace('/', "_")));
    git(scratch, &["init", "-q", "-b", "main", work.as_str()]);
    for (path, text) in files {
        let path = work.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "old"]);
    git(&work, &["push", "-q", dir.as_str(), "main"]);
    git(&work, &["rev-parse", "HEAD"])
}

fn refs(store: &Utf8Path) -> Vec<(String, String)> {
    assert!(store.is_dir(), "no store {store}");
    git(
        store,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .lines()
    .filter_map(|line| line.split_once(' '))
    .map(|(name, id)| (name.to_owned(), id.to_owned()))
    .collect()
}

fn config(data: &Utf8Path, listen: std::net::SocketAddr) -> Config {
    Config {
        data: data.to_owned(),
        listen,
        url: None,
        allowed_logins: "*".to_owned(),
        blocked_logins: String::new(),
        admin_logins: String::new(),
        projects_per_login: 5,
        github_client_id: None,
        dev_login_secret: Some(SECRET.to_owned()),
        git_slots: 4,
    }
}

fn tempdir() -> Utf8PathBuf {
    let dir = std::env::temp_dir().join(format!("newgit-serve-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8")
}

#[test]
fn a_data_directory_of_per_tracker_repos_moves_to_stores_per_audience() {
    let temp = tempdir();
    let data = temp.join("data");
    let unused: std::net::SocketAddr = "127.0.0.1:0".parse().expect("address");

    // The project as the old layout kept it.
    let home = data.join("homes/jack/p");
    let root = old_repo(&temp, &home.join("t/root.git"), &[("README.md", "root\n")]);
    let access = old_repo(
        &temp,
        &home.join("t/access.git"),
        &[(
            "site.toml",
            "audience = \"public\"\ngit = true\nmount = \"site\"\npaths = [\".\"]\n",
        )],
    );
    let site = old_repo(&temp, &home.join("t/site.git"), &[("index.html", "hi\n")]);
    let seed = old_repo(&temp, &home.join("t/lib.git"), &[("lib.rs", "\n")]);
    let env = old_repo(&temp, &home.join("u/env/ana.git"), &[(".env", "A=1\n")]);
    let secret = old_repo(&temp, &home.join("i/secret/root.git"), &[("x", "\n")]);
    // An instance's fork inside root's repo, and a merge's landing ref.
    let root_dir = home.join("t/root.git");
    git(
        &root_dir,
        &[
            "update-ref",
            "refs/namespaces/i/refs/namespaces/feat/refs/heads/main",
            &root,
        ],
    );
    git(
        &root_dir,
        &["update-ref", &format!("refs/newgit/landing/{root}"), &root],
    );
    {
        // Not a project yet: its directory is left as it is.
        let app = App::open(config(&data, unused)).expect("opens");
        assert!(root_dir.is_dir());
        let db = app.db.lock();
        db::create_project(&db, "jack/p", "jack", "p").expect("project");
        let list = [
            ("root".to_owned(), root.clone()),
            ("access".to_owned(), access.clone()),
            ("site".to_owned(), site.clone()),
        ]
        .into();
        db::create_branch(&db, "jack/p", "main", &list, "jack").expect("main");
        db::create_instance(&db, "jack/p", "feat", "jack", &root, None, false).expect("feat");
        db::add_fork(&db, "jack/p", "feat", "root").expect("fork");
    }

    // Opened again: moved.
    let app = App::open(config(&data, unused)).expect("migrates");
    for old in ["t", "u", "i"] {
        assert!(!home.join(old).exists(), "{old}/ is left");
    }
    let stores = home.join("stores");
    let at = |store: &str| -> Vec<(String, String)> { refs(&stores.join(format!("{store}.git"))) };
    let has = |store: &str, name: &str, id: &str| {
        let found = at(store);
        assert!(
            found.contains(&(name.to_owned(), id.to_owned())),
            "{store} lacks {name} {id}: {found:?}"
        );
    };
    let root_ns = "refs/namespaces/t/refs/namespaces/root/";
    has("project-devs", &format!("{root_ns}refs/heads/main"), &root);
    has(
        "project-devs",
        &format!("{root_ns}refs/namespaces/i/refs/namespaces/feat/refs/heads/main"),
        &root,
    );
    has(
        "project-devs",
        &format!("{root_ns}refs/newgit/landing/{root}"),
        &root,
    );
    has(
        "project-devs",
        "refs/namespaces/t/refs/namespaces/access/refs/heads/main",
        &access,
    );
    has(
        "public",
        "refs/namespaces/t/refs/namespaces/site/refs/heads/main",
        &site,
    );
    assert!(
        at("public")
            .iter()
            .all(|(name, _)| name.starts_with("refs/namespaces/t/refs/namespaces/site/")),
        "only site is public"
    );
    // Not on a branch, so its audience is not decided: still alone.
    has(
        "tracker/lib",
        "refs/namespaces/t/refs/namespaces/lib/refs/heads/main",
        &seed,
    );
    has(
        "user/ana",
        "refs/namespaces/u/refs/namespaces/env/refs/heads/main",
        &env,
    );
    has(
        "instance/secret",
        &format!("{root_ns}refs/namespaces/i/refs/namespaces/secret/refs/heads/main"),
        &secret,
    );
    for store in [
        "project-devs",
        "public",
        "tracker/lib",
        "user/ana",
        "instance/secret",
    ] {
        let dir = stores.join(format!("{store}.git"));
        assert_eq!(git(&dir, &["rev-parse", "--show-ref-format"]), "reftable");
        assert_eq!(git(&dir, &["config", "newgit.audience"]), store);
    }
    {
        let db = app.db.lock();
        let placed = db::tracker_stores(&db, "jack/p").expect("rows");
        assert_eq!(placed["root"], "project-devs");
        assert_eq!(placed["access"], "project-devs");
        assert_eq!(placed["site"], "public");
        assert_eq!(placed["lib"], "tracker/lib");
    }
    drop(app);
    // Again: nothing old is left, so nothing happens.
    drop(App::open(config(&data, unused)).expect("opens again"));
    has(
        "public",
        "refs/namespaces/t/refs/namespaces/site/refs/heads/main",
        &site,
    );

    // Served from the stores, as each tracker's own repo.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port");
    std::thread::spawn({
        let data = data.clone();
        move || newgit_serve::serve(config(&data, port))
    });
    let url = format!("http://{port}");
    let started = std::time::Instant::now();
    while std::net::TcpStream::connect(port).is_err() {
        assert!(started.elapsed().as_secs() < 30, "serve never listened");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let answer: Value = ureq::post(&format!("{url}/v1/dev/login"))
        .header("x-dev-login", SECRET)
        .send_json(serde_json::json!({ "login": "jack" }))
        .expect("dev login")
        .body_mut()
        .read_json()
        .expect("json");
    let token = answer["token"].as_str().expect("a token");
    let header = format!("http.extraHeader=Authorization: Bearer {token}");
    let ls = |repo: &str| {
        git(
            &temp,
            &[
                "-c",
                "credential.helper=",
                "-c",
                &header,
                "ls-remote",
                &format!("{url}/_git/{repo}.git"),
            ],
        )
    };
    let listed = ls("jack.p.t.root");
    assert!(
        listed.contains(&format!("{root}\trefs/heads/main")),
        "{listed}"
    );
    assert!(!listed.contains("namespaces"), "forks are hidden: {listed}");
    assert!(ls("jack.p.i.feat.root").contains(&format!("{root}\trefs/heads/main")));
    assert!(ls("jack.p.t.site").contains(&format!("{site}\trefs/heads/main")));

    let _ = std::fs::remove_dir_all(&temp);
}
