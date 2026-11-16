// Views rooted at a tracker are served from its stored rooted history (#412),
// extended as things land, and are the very commits composing gives.
//
// - A member's view of root and anyone's view of the public `blog` come from
//   the stores, with nothing composed for them; they match, ref for ref and
//   note for note, what the same server composes with stored histories off,
//   after a CLI merge, a merge the server wrote, and `blog` narrowing to the
//   members (its history, and its stored view, move to their store).
// - The members' stored history holds nothing of a tracker only jack reads;
//   jack's own view of root, which has it, is composed for him. Nobody
//   fetches by id, through a view they reach, a commit of a view they may
//   not read.
// - With nothing extending them in the background, a view fetched right
//   after a landing already has it: never behind what landed.
// - A data directory from before stored histories gets them at start,
//   without a request, and a start stopped part way through resumes.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

fn start_env(temp: &Utf8Path, envs: &[(&str, &str)]) -> Server {
    let data = temp.join("serve");
    let mut command = Command::new(env!("CARGO_BIN_EXE_newgit"));
    command
        .args([
            "serve",
            "--data",
            data.as_str(),
            "--listen",
            "127.0.0.1:0",
            "--allow",
            "*",
        ])
        .env("NEWGIT_SERVE_DEV_LOGIN_SECRET", SECRET)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("newgit serve starts");
    let mut line = String::new();
    let _ = BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut line);
    let url = line
        .trim()
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .to_owned();
    Server { child, url, data }
}

/// Every branch, tag, and `HEAD` a view advertises, and its canonical notes.
fn view_of(token: &str, temp: &Utf8Path, url: &str, name: &str) -> (Vec<String>, String) {
    let listed = git_as(token, temp, &["ls-remote", "--symref", url]);
    assert!(
        listed.status.success(),
        "{url}: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    // Git 2.55 advertises a namespace's `HEAD` twice from a reftable store
    // (every tracker repo here too); once is what it says.
    let mut refs: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter(|line| !line.contains("refs/notes/"))
        .map(ToOwned::to_owned)
        .collect();
    refs.sort();
    refs.dedup();
    let dir = temp.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    let cloned = git_as(token, temp, &["clone", "-q", url, dir.as_str()]);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let fetched = git_as(
        token,
        &dir,
        &["fetch", "-q", "origin", "+refs/notes/*:refs/notes/*"],
    );
    let notes = if fetched.status.success() {
        String::from_utf8_lossy(&run("git", &dir, &dir, &["notes", "list"]).stdout).into_owned()
    } else {
        String::new()
    };
    (refs, notes)
}

fn views_composed(server: &Server) -> usize {
    std::fs::read_dir(server.data.join("endpoint/jack/store/views"))
        .map(|read| read.count())
        .unwrap_or(0)
}

fn has_object(dir: &Utf8Path, object: &str) -> bool {
    run(
        "git",
        dir,
        dir,
        &["--git-dir", dir.as_str(), "cat-file", "-e", object],
    )
    .status
    .success()
}

fn state_of(dir: &Utf8Path, tracker: &str) -> Option<String> {
    let output = run(
        "git",
        dir,
        dir,
        &[
            "--git-dir",
            dir.as_str(),
            "cat-file",
            "blob",
            &format!("refs/newgit/rooted/{tracker}:served"),
        ],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A CLI merge in the project's own checkout: `blog`'s post and root's README.
fn land_edit(home: &Utf8Path, store: &Utf8Path, text: &str) {
    std::fs::write(store.join("blog/post.md"), format!("{text}\n")).expect("write");
    git(
        home,
        store,
        &["commit", "-q", "-am", &format!("post: {text}")],
    );
    std::fs::write(
        store.join("README.md"),
        format!("one\ntwo\nthree\n{text}\n"),
    )
    .expect("write");
    git(home, store, &["commit", "-q", "-am", text]);
    newgit(home, store, &["merge", "main"]);
}

/// Two merges begun from one base, root's own files apart: the second is
/// the server's merge commit.
fn land_two_from_one_base(
    server: &Server,
    temp: &Utf8Path,
    home: &Utf8Path,
    jack: &str,
    ana: &str,
) {
    let base = list_of(server, jack, "main");
    let ana_list = root_change(
        server,
        ana,
        temp,
        home,
        &base,
        &[("ana.md", Some("ana\n"))],
        "ana",
    );
    let jack_list = root_change(
        server,
        jack,
        temp,
        home,
        &base,
        &[("jack.md", Some("jack\n"))],
        "jack",
    );
    for (token, list) in [(ana, &ana_list), (jack, &jack_list)] {
        let (status, answer) =
            finish_by_hand(server, token, "main", &base, list, serde_json::json!([]));
        assert_eq!(status, 200, "{answer}");
    }
    let head = list_of(server, jack, "main");
    let parents = run(
        "git",
        home,
        home,
        &[
            "--git-dir",
            server
                .data
                .join("homes/jack/store/stores/project-devs.git")
                .as_str(),
            "rev-list",
            "--parents",
            "-n",
            "1",
            &head["root"],
        ],
    );
    assert_eq!(
        String::from_utf8_lossy(&parents.stdout)
            .split_whitespace()
            .count(),
        3,
        "the second merge is a merge commit the server wrote"
    );
}

#[test]
fn rooted_views_come_from_stored_histories_and_match_composing_them() {
    let (_guard, temp) = tempdir();
    let server = start_env(&temp, &[]);
    let home = temp.join("jack");
    let jack = login(&server, &home, "jack");
    let ana = login(&server, &temp.join("ana"), "ana");
    let eve = login(&server, &temp.join("eve"), "eve");
    let store = merge_project(&temp, &home, &server);
    add_member(&server, &jack, "ana");
    std::fs::create_dir_all(store.join("vault")).expect("mkdir");
    std::fs::write(store.join("vault/keep.md"), "k\n").expect("write");
    git(&home, &store, &["add", "vault"]);
    git(&home, &store, &["commit", "-q", "-m", "vault"]);
    newgit(
        &home,
        &store,
        &[
            "tracker",
            "create",
            "vault",
            "--git",
            "--audience",
            "user:jack",
            "--path",
            "vault",
        ],
    );
    newgit(&home, &store, &["merge", "main"]);
    // Written once vault is its own tracker: only jack's store has it.
    std::fs::write(store.join("vault/plan.md"), "jack's plan, nobody else's\n").expect("write");
    git(&home, &store, &["add", "vault"]);
    git(&home, &store, &["commit", "-q", "-m", "plan"]);
    newgit(&home, &store, &["merge", "main"]);
    land_edit(&home, &store, "second");
    land_two_from_one_base(&server, &temp, &home, &jack, &ana);

    let root_url = format!("{}/jack/store.git", server.url);
    let blog_url = format!("{}/jack/store/blog.git", server.url);
    let composed = views_composed(&server);
    let ana_root = view_of(&ana, &temp, &root_url, "ana-root");
    let eve_blog = view_of(&eve, &temp, &blog_url, "eve-blog");
    let ana_blog = view_of(&ana, &temp, &blog_url, "ana-blog");
    assert_eq!(
        views_composed(&server),
        composed,
        "a view that is a stored history was composed per fetch"
    );
    assert_eq!(eve_blog, ana_blog, "one public view for everyone");
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    let public = server.data.join("homes/jack/store/stores/public.git");
    assert!(
        state_of(&devs, "root").is_some(),
        "root's history is stored with root"
    );
    assert!(
        state_of(&public, "blog").is_some(),
        "blog's is stored with blog"
    );

    // Nothing of vault's is in the members' store or the public one, and
    // ana's view has none of it; jack's has it, composed for him.
    let plan = git(&home, &store, &["rev-parse", "HEAD:vault/plan.md"]);
    assert!(!has_object(&devs, &plan) && !has_object(&public, &plan));
    assert!(!temp.join("ana-root/vault").exists());
    let jack_root = view_of(&jack, &temp, &root_url, "jack-root");
    assert!(temp.join("jack-root/vault/plan.md").exists());
    assert!(
        views_composed(&server) > composed,
        "jack's view is composed for him"
    );
    assert_ne!(jack_root.0, ana_root.0);

    // Nobody fetches by id what they may not read, through any view they
    // reach: eve, root's stored view through blog's; ana, jack's view of
    // root through her own.
    let fetch_by_id = |token: &str, url: &str, commit: &str, name: &str| {
        let dir = temp.join(name);
        git(&home, &temp, &["init", "-q", dir.as_str()]);
        git_as(token, &dir, &["fetch", "-q", url, commit])
            .status
            .success()
    };
    let tip_of = |view: &(Vec<String>, String)| {
        view.0
            .iter()
            .find(|line| line.ends_with("\trefs/heads/main"))
            .and_then(|line| line.split('\t').next())
            .expect("a main")
            .to_owned()
    };
    assert!(!fetch_by_id(
        &eve,
        &blog_url,
        &tip_of(&ana_root),
        "eve-by-id"
    ));
    assert!(!fetch_by_id(
        &ana,
        &root_url,
        &tip_of(&jack_root),
        "ana-by-id"
    ));
    assert!(fetch_by_id(&ana, &root_url, &tip_of(&ana_root), "ana-own"));

    // blog narrows to the members: its history moves to their store, and
    // its stored rooted history is composed there.
    let definition = store.join(".newgit/trackers/blog.toml");
    let text = std::fs::read_to_string(&definition).expect("blog.toml");
    std::fs::write(
        &definition,
        text.replace("audience = \"public\"", "audience = \"project-devs\""),
    )
    .expect("write");
    git(
        &home,
        &store,
        &["commit", "-q", "-am", "blog for the members"],
    );
    newgit(&home, &store, &["merge", "main"]);
    land_edit(&home, &store, "narrowed");
    let ana_root = view_of(&ana, &temp, &root_url, "ana-root-narrowed");
    let ana_blog = view_of(&ana, &temp, &blog_url, "ana-blog-narrowed");
    let jack_root = view_of(&jack, &temp, &root_url, "jack-root-narrowed");
    assert!(
        state_of(&devs, "blog").is_some(),
        "blog's history is stored where it is now"
    );
    assert!(
        !git_as(&eve, &temp, &["ls-remote", &blog_url])
            .status
            .success()
    );

    // The same server composing every view, as before stored histories.
    drop(server);
    let composing = start_env(&temp, &[("NEWGIT_SERVE_ROOTED", "off")]);
    let root_url = format!("{}/jack/store.git", composing.url);
    let blog_url = format!("{}/jack/store/blog.git", composing.url);
    assert_eq!(
        view_of(&ana, &temp, &root_url, "ana-root-composed"),
        ana_root
    );
    assert_eq!(
        view_of(&ana, &temp, &blog_url, "ana-blog-composed"),
        ana_blog
    );
    assert_eq!(
        view_of(&jack, &temp, &root_url, "jack-root-composed"),
        jack_root
    );
}

#[test]
fn a_rooted_view_fetched_right_after_a_landing_has_it() {
    let (_guard, temp) = tempdir();
    // Nothing extends stored histories but the reads themselves.
    let server = start_env(&temp, &[("NEWGIT_SERVE_ROOTED_BACKGROUND", "off")]);
    let home = temp.join("jack");
    let jack = login(&server, &home, "jack");
    let store = merge_project(&temp, &home, &server);
    let root_url = format!("{}/jack/store.git", server.url);
    let blog_url = format!("{}/jack/store/blog.git", server.url);
    view_of(&jack, &temp, &root_url, "root-1");
    view_of(&jack, &temp, &blog_url, "blog-1");
    for text in ["two", "three"] {
        land_edit(&home, &store, text);
        view_of(&jack, &temp, &root_url, "root-2");
        view_of(&jack, &temp, &blog_url, "blog-2");
        assert_eq!(
            std::fs::read_to_string(temp.join("root-2/README.md")).expect("README"),
            format!("one\ntwo\nthree\n{text}\n")
        );
        assert_eq!(
            std::fs::read_to_string(temp.join("blog-2/post.md")).expect("post"),
            format!("{text}\n")
        );
    }
}

#[test]
fn a_data_directory_from_before_stored_histories_gets_them_at_start_resumably() {
    let (_guard, temp) = tempdir();
    let home = temp.join("jack");
    let (jack, expected) = {
        let server = start_env(
            &temp,
            &[
                ("NEWGIT_SERVE_ROOTED", "off"),
                ("NEWGIT_SERVE_ROOTED_BACKGROUND", "off"),
            ],
        );
        let jack = login(&server, &home, "jack");
        let store = merge_project(&temp, &home, &server);
        for text in ["two", "three", "four"] {
            land_edit(&home, &store, text);
        }
        let root = view_of(
            &jack,
            &temp,
            &format!("{}/jack/store.git", server.url),
            "before-root",
        );
        let blog = view_of(
            &jack,
            &temp,
            &format!("{}/jack/store/blog.git", server.url),
            "before-blog",
        );
        (jack, (root, blog))
    };
    let devs = temp.join("serve/homes/jack/store/stores/project-devs.git");
    let public = temp.join("serve/homes/jack/store/stores/public.git");
    assert!(
        state_of(&devs, "root").is_none(),
        "nothing was stored with them off"
    );

    // Stopped after the first commit it records.
    let faults = temp.join("faults");
    std::fs::write(&faults, "rooted-checkpoint\n").expect("faults");
    let mut stopped = start_env(
        &temp,
        &[
            ("NEWGIT_SERVE_FAULT_FILE", faults.as_str()),
            ("NEWGIT_SERVE_ROOTED_CHUNK", "1"),
        ],
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while stopped.child.try_wait().expect("wait").is_none() {
        assert!(
            std::time::Instant::now() < until,
            "the backfill never reached its checkpoint"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let partial: Vec<String> = ["access", "blog", "root"]
        .iter()
        .filter_map(|tracker| state_of(&devs, tracker).or_else(|| state_of(&public, tracker)))
        .collect();
    assert_eq!(
        partial,
        vec![String::new()],
        "one chunk recorded, nothing served yet"
    );
    drop(stopped);

    // Started again: it finishes, with nobody asking.
    std::fs::write(&faults, "").expect("faults");
    let server = start_env(&temp, &[("NEWGIT_SERVE_ROOTED_CHUNK", "1")]);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !(state_of(&devs, "root").is_some_and(|served| !served.is_empty())
        && state_of(&public, "blog").is_some_and(|served| !served.is_empty()))
    {
        assert!(
            std::time::Instant::now() < until,
            "the backfill never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let composed = views_composed(&server);
    let root = view_of(
        &jack,
        &temp,
        &format!("{}/jack/store.git", server.url),
        "after-root",
    );
    let blog = view_of(
        &jack,
        &temp,
        &format!("{}/jack/store/blog.git", server.url),
        "after-blog",
    );
    assert_eq!(views_composed(&server), composed);
    assert_eq!((root, blog), expected);
}
