// A migration to stores per audience that stopped part way (#409's review) is
// finished by the next start, and nothing is lost: a tracker copied but not
// yet recorded is copied again, one whose move into its audience's store
// finished before its row followed is placed there, and per-user content
// copied but not deleted where it was is copied again harmlessly.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_serve::repos::{Repo, Store};
use newgit_serve::{App, Config, db};

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

/// A bare repository as the old layout made one, holding one commit of
/// `files`. Returns the commit.
fn old_repo(scratch: &Utf8Path, dir: &Utf8Path, files: &[(&str, &str)]) -> String {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(
        dir,
        &["init", "-q", "--bare", "-b", "main", "--ref-format=files"],
    );
    let work = scratch.join(format!("work-{}", dir.as_str().replace('/', "_")));
    git(scratch, &["init", "-q", "-b", "main", work.as_str()]);
    for (path, text) in files {
        std::fs::write(work.join(path), text).expect("write");
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "old"]);
    git(&work, &["push", "-q", dir.as_str(), "main"]);
    git(&work, &["rev-parse", "HEAD"])
}

fn config(data: &Utf8Path) -> Config {
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
        github: None,
    }
}

#[test]
fn a_migration_that_stopped_part_way_is_finished_by_the_next_start() {
    let dir = std::env::temp_dir().join(format!(
        "newgit-serve-migrate-interrupted-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let temp = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8");
    let data = temp.join("data");
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
    let env = old_repo(&temp, &home.join("u/env/ana.git"), &[(".env", "A=1\n")]);
    {
        let app = App::open(config(&data)).expect("opens");
        let db = app.db.lock();
        db::create_project(&db, "jack/p", "jack", "p").expect("project");
        let list = [
            ("root".to_owned(), root.clone()),
            ("access".to_owned(), access.clone()),
            ("site".to_owned(), site.clone()),
        ]
        .into();
        db::set_branch(&db, "jack/p", "main", &list, "jack", 0).expect("main");

        // The run that stopped. root: copied alone, nothing recorded, the old
        // repository still there.
        Repo::tracker(&home, Some(Store::Tracker("root".into())), "root")
            .copy_from(&home.join("t/root.git"), "", "refs/")
            .expect("root copied");
        // site: copied alone, recorded, the old one deleted, then renamed
        // into `public.git`; stopped before its row followed.
        let alone = Repo::tracker(&home, Some(Store::Tracker("site".into())), "site");
        alone
            .copy_from(&home.join("t/site.git"), "", "refs/")
            .expect("site copied");
        db::set_tracker_store(&db, "jack/p", "site", "tracker/site").expect("row");
        std::fs::remove_dir_all(home.join("t/site.git")).expect("old site");
        // As a move does when its store is the audience's first: labelled,
        // then renamed.
        git(
            &temp,
            &[
                "--git-dir",
                alone.dir().as_str(),
                "config",
                "newgit.audience",
                "public",
            ],
        );
        std::fs::rename(alone.dir(), Store::Public.dir(&home)).expect("renamed");
        // env: copied, the old one not yet deleted.
        Repo::new(&home, Store::User("ana".into()), "u/env".into())
            .copy_from(&home.join("u/env/ana.git"), "", "refs/")
            .expect("env copied");
    }

    let app = App::open(config(&data)).expect("finishes the migration");
    for old in ["t", "u", "i"] {
        assert!(!home.join(old).exists(), "{old}/ is left");
    }
    assert!(
        !home.join("stores/tracker").exists(),
        "a tracker is left alone"
    );
    let head = |store: &str, namespace: &str| {
        git(
            &temp,
            &[
                "--git-dir",
                home.join(format!("stores/{store}.git")).as_str(),
                "rev-parse",
                &format!("{namespace}refs/heads/main"),
            ],
        )
    };
    let tracker = |name: &str| format!("refs/namespaces/t/refs/namespaces/{name}/");
    assert_eq!(head("project-devs", &tracker("root")), root);
    assert_eq!(head("project-devs", &tracker("access")), access);
    assert_eq!(head("public", &tracker("site")), site);
    assert_eq!(
        head("user/ana", "refs/namespaces/u/refs/namespaces/env/"),
        env
    );
    let db = app.db.lock();
    let placed = db::tracker_stores(&db, "jack/p").expect("rows");
    assert_eq!(placed["root"], "project-devs");
    assert_eq!(placed["access"], "project-devs");
    assert_eq!(placed["site"], "public");
    drop(db);
    drop(app);
    let _ = std::fs::remove_dir_all(&temp);
}
