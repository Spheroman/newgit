// `newgit merge` when `main` moved on the server since the work began
// (#411 review, item 5): the server merges, and the CLI says what landed,
// not what it sent. Two clones of one project, each with an instance.

include!("support/serve.rs");
include!("support/serve_merge.rs");

/// An instance `name` in the clone at `store`, with `file` written and
/// committed in its workspace.
fn instance_with(home: &Utf8Path, store: &Utf8Path, name: &str, file: &str, text: &str) {
    newgit(home, store, &["spawn", name]);
    let workspace = Utf8PathBuf::from(newgit(home, store, &["status", name, "--path"]));
    std::fs::write(workspace.join(file), text).expect("write");
    git(home, &workspace, &["add", file]);
    git(
        home,
        &workspace,
        &["commit", "-q", "-m", &format!("{name}: {file}")],
    );
}

/// The project, with ana a member and a clone of her own: `(server,
/// faults, jack's home, jack's token, jack's clone, ana's home, ana's
/// clone)`.
fn two_clones(
    temp: &Utf8Path,
) -> (
    Server,
    Utf8PathBuf,
    Utf8PathBuf,
    String,
    Utf8PathBuf,
    Utf8PathBuf,
    Utf8PathBuf,
) {
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(temp, &faults);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let ana_home = temp.join("ana");
    login(&server, &ana_home, "ana");
    let store = merge_project(temp, &home, &server);
    let bearer = format!("Bearer {token}");
    let (status, answer) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/members",
        &[("authorization", bearer.as_str())],
        "{\"login\":\"ana\"}",
    );
    assert_eq!(status, 200, "{answer}");
    let ana_store = temp.join("ana-store");
    newgit(
        &ana_home,
        temp,
        &[
            "clone",
            "jack/store",
            ana_store.as_str(),
            "--url",
            &server.url,
        ],
    );
    (server, faults, home, token, store, ana_home, ana_store)
}

/// ana's `newgit merge late`, held on the server once it reaches the finish
/// (her client has read `main` and begun by then) until `then` has run.
fn merge_held(
    faults: &Utf8Path,
    ana_home: &Utf8Path,
    ana_store: &Utf8Path,
    then: impl FnOnce(),
) -> std::process::Output {
    std::fs::write(faults, "hold-finish-by-ana\n").expect("faults");
    let (home, store) = (ana_home.to_owned(), ana_store.to_owned());
    let merging = std::thread::spawn(move || {
        run(
            env!("CARGO_BIN_EXE_newgit"),
            &home,
            &store,
            &["merge", "late"],
        )
    });
    let marker = Utf8PathBuf::from(format!("{faults}.at"));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < until,
            "ana's finish never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    then();
    std::fs::write(faults, "").expect("faults");
    merging.join().expect("joined")
}

#[test]
fn a_merge_the_server_merged_says_what_landed() {
    let (_guard, temp) = tempdir();
    let (server, faults, home, token, store, ana_home, ana_store) = two_clones(&temp);
    instance_with(&ana_home, &ana_store, "late", "late.md", "late\n");
    instance_with(&home, &store, "early", "early.md", "early\n");

    let output = merge_held(&faults, &ana_home, &ana_store, || {
        let early = newgit(&home, &store, &["merge", "early"]);
        assert!(!early.contains("merged by the remote"), "{early}");
    });
    let late = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{late}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let landed = list_of(&server, &token, "main");
    assert!(
        late.contains("the remote merged it onto what `main` is now"),
        "{late}"
    );
    let root_line = late
        .lines()
        .find(|line| line.contains(&landed["root"][..12]))
        .unwrap_or_default();
    assert!(root_line.contains("merged by the remote"), "{late}");
    assert!(
        !late.lines().any(|line| line.contains("access")),
        "access did not move with this merge: {late}"
    );
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    for file in ["early.md", "late.md"] {
        assert!(
            file_at(&home, &devs, &landed["root"], file).is_some(),
            "{file}"
        );
    }
}

#[test]
fn a_conflict_the_server_found_is_refused_naming_the_file() {
    let (_guard, temp) = tempdir();
    let (server, faults, home, token, store, ana_home, ana_store) = two_clones(&temp);
    instance_with(
        &ana_home,
        &ana_store,
        "late",
        "README.md",
        "one\ntwo\nlate\n",
    );
    instance_with(&home, &store, "early", "README.md", "one\ntwo\nearly\n");
    let mut landed = None;
    let output = merge_held(&faults, &ana_home, &ana_store, || {
        newgit(&home, &store, &["merge", "early"]);
        landed = Some(list_of(&server, &token, "main"));
    });
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("`README.md`"), "{said}");
    assert_eq!(
        Some(list_of(&server, &token, "main")),
        landed,
        "nothing landed"
    );
}
