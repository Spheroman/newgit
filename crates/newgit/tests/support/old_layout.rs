// A project laid out before mounts, linked to the fake remote as the old
// engine left it. Included by the migration tests after fake_remote.rs.

use newgit_core::checkpoint::{CheckpointLog, CheckpointRecord};

const OLD_ROOT: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".\"]\ngit = true\n";
const OLD_PROJECT: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".newgit\"]\ngit = true\n";
const OLD_ACCESS: &str = "audience = \"project-devs\"\nstorage = \"local\"\nmerge_with_source = true\npaths = [\".newgit/trackers\"]\ngit = true\nmaintainers = \"owner\"\n";

struct Old {
    fixture: Fixture,
    /// The project's history where the remote's `main` is.
    main: String,
    /// The instance's work, checkpointed and synced.
    feature: String,
}

/// A project laid out before mounts, linked to the fake remote holding it
/// as the old engine pushed it.
fn old_project(temp: &Utf8Path) -> Old {
    let repos = temp.join("artifacts");
    std::fs::create_dir_all(&repos).expect("mkdir");
    let remote = FakeRemote::start(repos.clone());
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    git(&store, &["config", "commit.gpgSign", "false"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);
    let home = temp.join("home/.newgit");
    std::fs::create_dir_all(&home).expect("mkdir");
    std::fs::write(
        home.join("auth.toml"),
        format!(
            "[remotes.\"{}\"]\nlogin = \"jack\"\ntoken = \"{}\"\ntoken_id = \"{LOGIN_TOKEN_ID}\"\n",
            remote.url,
            fake_token(LOGIN_TOKEN_ID)
        ),
    )
    .expect("write auth");
    let fixture = Fixture {
        temp: temp.to_path_buf(),
        store: store.clone(),
        remote,
    };
    fixture.ok(&store, &["init"]);
    let trackers = store.join(".newgit/trackers");
    std::fs::write(trackers.join("root.toml"), OLD_ROOT).expect("write");
    std::fs::write(trackers.join("project.toml"), OLD_PROJECT).expect("write");
    std::fs::write(trackers.join("access.toml"), OLD_ACCESS).expect("write");
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n\n[remote]\nurl = \"{}\"\nproject = \"jack/store\"\n",
        temp.join("workspaces"),
        fixture.remote.url
    ));
    std::fs::write(&config, text).expect("write config");
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "newgit"]);
    let main = git(&store, &["rev-parse", "HEAD"]);

    git(&store, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(store.join("feature.txt"), "work\n").expect("write");
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "feature work"]);
    let feature = git(&store, &["rev-parse", "HEAD"]);
    git(&store, &["checkout", "-q", "main"]);

    // The old tracker store and notes: every old tracker's commit is `main`
    // itself (what the old commits were does not matter to the migration,
    // only which view each state was).
    let tracker_store = store.join(".newgit/local/trackers.git");
    let mut refspecs = Vec::new();
    for tracker in ["root", "project", "access"] {
        refspecs.push(format!("{main}:refs/trackers/{tracker}/main"));
    }
    refspecs.push(format!("{feature}:refs/trackers/root/instances/feature"));
    let mut args = vec!["push", "-q", "-f", tracker_store.as_str()];
    args.extend(refspecs.iter().map(String::as_str));
    git(&store, &args);
    for (view, root) in [(&main, &main), (&feature, &feature)] {
        let note = format!("access {main}\nproject {main}\nroot {root}\n");
        git(
            &store,
            &[
                "notes",
                "--ref",
                "refs/notes/newgit-composed",
                "add",
                "-f",
                "-m",
                &note,
                view,
            ],
        );
    }

    // The remote: a repo per old tracker at `main`, `main`'s list naming
    // all three, and a synced checkpoint whose code is the old lists.
    for tracker in ["root", "project", "access"] {
        let repo = fixture.remote.repo(&format!("jack.store.t.{tracker}"));
        git(temp, &["init", "-q", "--bare", "-b", "main", repo.as_str()]);
        git(
            &store,
            &[
                "push",
                "-q",
                repo.as_str(),
                &format!("{main}:refs/heads/main"),
            ],
        );
    }
    let old_list = json!({ "root": main, "project": main, "access": main });
    {
        let mut state = fixture.remote.state.lock().expect("state");
        state
            .branches
            .entry("jack/store".to_owned())
            .or_default()
            .insert("main".to_owned(), old_list.clone());
    }
    git(
        &store,
        &[
            "update-ref",
            "refs/newgit/checkpoints/feature/ckpt_001",
            &feature,
        ],
    );
    let record: CheckpointRecord = serde_json::from_value(json!({
        "id": "ckpt_001",
        "branch": "feature",
        "created_at": "2026-11-01T00:00:00Z",
        "reason": "explicit",
        "source": {
            "head_rev": feature,
            "store_ref": "refs/newgit/checkpoints/feature/ckpt_001",
        },
        "code": { "head": old_list, "tip": old_list },
    }))
    .expect("a checkpoint record");
    let log = CheckpointLog::new(store.join(".newgit/checkpoints/feature"), "feature");
    log.save(&record).expect("save");
    {
        let mut state = fixture.remote.state.lock().expect("state");
        let key = ("feature".to_owned(), "ckpt_001".to_owned());
        state
            .checkpoints
            .insert(key.clone(), serde_json::to_value(&record).expect("json"));
        state.checkpoint_authors.insert(key, "jack".to_owned());
    }
    Old {
        fixture,
        main,
        feature,
    }
}

fn writes(fixture: &Fixture) -> Vec<(String, String)> {
    fixture
        .remote
        .requests()
        .into_iter()
        .filter(|(method, _, _)| method != "GET")
        .map(|(method, path, _)| (method, path))
        .collect()
}


/// What an uninterrupted migration leaves: root's `main` the carve's root
/// commit and `access`'s its pin, every branch list the new index, every
/// checkpoint naming root commits with root's fork holding them, nothing of
/// the migration's own state left behind.
fn assert_migrated(old: &Old) {
    let fixture = &old.fixture;
    let tracker_store = fixture.store.join(".newgit/local/trackers.git");
    let local_root = git(&tracker_store, &["rev-parse", "refs/trackers/root/main"]);
    let root_repo = fixture.remote.repo("jack.store.t.root");
    assert_eq!(git(&root_repo, &["rev-parse", "main"]), local_root);
    let pin = git(&root_repo, &["rev-parse", "main:.newgit/pins/access"]);
    assert_eq!(
        git(&fixture.remote.repo("jack.store.t.access"), &["rev-parse", "main"]),
        pin
    );
    assert_eq!(
        git(&tracker_store, &["rev-parse", "refs/trackers/access/main"]),
        pin
    );
    let state = fixture.remote.state.lock().expect("state");
    assert_eq!(
        state.branches["jack/store"]["main"],
        json!({ "root": local_root, "access": pin })
    );
    let remote = state.checkpoints[&("feature".to_owned(), "ckpt_001".to_owned())].clone();
    drop(state);
    assert_eq!(remote["code"]["head"], json!({ "root": old.feature }));
    assert_eq!(remote["code"]["tip"], json!({ "root": old.feature }));
    let local = CheckpointLog::new(fixture.store.join(".newgit/checkpoints/feature"), "feature")
        .load("ckpt_001")
        .expect("local record");
    assert_eq!(local.code.head.keys().collect::<Vec<_>>(), vec!["root"]);
    let fork = fixture.remote.repo("jack.store.i.feature.root");
    assert_eq!(git(&fork, &["rev-parse", "refs/heads/feature"]), old.feature);
    assert_eq!(
        split_source(&root_repo, "main"),
        git(&fixture.store, &["rev-parse", "HEAD"]),
        "the project root holds the carve"
    );
    assert!(
        git(&fixture.store, &["for-each-ref", "refs/newgit-migrating/"]).is_empty(),
        "the saved carve is gone"
    );
    assert!(
        fixture
            .ok(&fixture.store, &["tracker", "migrate"])
            .contains("Nothing to migrate")
    );
}
