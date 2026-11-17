// `newgit pull --remote <target> --force` on a project on `newgit serve`
// (#442): an instance still on the old line keeps its work here. Its
// checkpoint's sync would send the dropped commit to its fork on the remote
// (the remote's `main` no longer has it), so the sync refuses, the checkpoint
// is saved here only, and says why; a push from it, which is that sync,
// fails the same way.

#![allow(dead_code, reason = "the shared helpers are not all used here")]

include!("support/serve.rs");

fn main_in(home: &Utf8Path, store: &Utf8Path, namespace: &str) -> Option<String> {
    let output = run(
        "git",
        home,
        store,
        &[
            "--git-dir",
            store.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/namespaces/t/refs/namespaces/{namespace}/refs/heads/main"),
        ],
    );
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn is_ancestor(home: &Utf8Path, git_dir: &Utf8Path, ancestor: &str, of: &str) -> bool {
    run(
        "git",
        home,
        git_dir,
        &[
            "--git-dir",
            git_dir.as_str(),
            "merge-base",
            "--is-ancestor",
            ancestor,
            of,
        ],
    )
    .status
    .success()
}

#[test]
fn a_checkpoint_on_the_old_line_does_not_send_the_dropped_commit() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let home = temp.join("jack");
    login(&server, &home, "jack");

    // t3code's upstream, and its copy "on GitHub".
    let upstream = temp.join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    git(&home, &upstream, &["init", "-q", "-b", "newgit"]);
    std::fs::write(upstream.join("app.ts"), "app\n").expect("write");
    git(&home, &upstream, &["add", "-A"]);
    git(&home, &upstream, &["commit", "-q", "-m", "t3: first"]);
    let github = temp.join("t3code.git");
    git(
        &home,
        &temp,
        &["clone", "-q", "--bare", upstream.as_str(), github.as_str()],
    );

    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&home, &store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "root\n").expect("write");
    git(&home, &store, &["add", "."]);
    git(&home, &store, &["commit", "-q", "-m", "initial"]);
    newgit(&home, &store, &["init"]);
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "\n[workspace]\nroot = \"{}\"\n",
        temp.join("workspaces")
    ));
    std::fs::write(&config, text).expect("write config");
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "newgit"]);
    newgit(&home, &store, &["remote", "add", "--url", &server.url]);
    git(&home, &store, &["commit", "-q", "-am", "link"]);
    newgit(
        &home,
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
    let definition = store.join(".newgit/trackers/t3code.toml");
    let mut text = std::fs::read_to_string(&definition).expect("definition");
    text.push_str(&format!(
        "export = {{ url = \"{github}\", name = \"t3code\", branch = \"newgit\" }}\n"
    ));
    std::fs::write(&definition, text).expect("write");
    git(&home, &store, &["add", "-A"]);
    git(&home, &store, &["commit", "-q", "-m", "export target"]);
    newgit(&home, &store, &["merge", "main"]);
    newgit(&home, &store, &["export", "--remote", "t3code"]);
    let clean = git(&home, &github, &["rev-parse", "newgit"]);

    // A key lands in t3code, through an instance, and is exported.
    newgit(&home, &store, &["spawn", "feature"]);
    let feature = Utf8PathBuf::from(newgit(&home, &store, &["status", "feature", "--path"]));
    std::fs::write(feature.join("apps/t3code/key.ts"), "KEY=sk-live\n").expect("write");
    git(&home, &feature, &["add", "-A"]);
    git(
        &home,
        &feature,
        &["commit", "-q", "-m", "t3code: add the key"],
    );
    newgit(&home, &store, &["merge", "feature"]);
    newgit(&home, &store, &["export", "--remote", "t3code"]);
    let leaked = git(&home, &github, &["rev-parse", "newgit"]);
    assert_ne!(leaked, clean);
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    assert_eq!(
        main_in(&home, &devs, "t3code").as_deref(),
        Some(leaked.as_str())
    );

    // GitHub purges it: the branch force-pushed back, one commit on top.
    let purge = temp.join("purge");
    git(
        &home,
        &temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            purge.as_str(),
        ],
    );
    git(&home, &purge, &["reset", "-q", "--hard", &clean]);
    std::fs::write(purge.join("key.ts"), "KEY=rotated\n").expect("write");
    git(&home, &purge, &["add", "-A"]);
    git(&home, &purge, &["commit", "-q", "-m", "t3: rotate"]);
    git(
        &home,
        &purge,
        &["push", "-q", "--force", "origin", "newgit"],
    );
    let purged = git(&home, &purge, &["rev-parse", "HEAD"]);

    newgit(&home, &store, &["pull", "--remote", "t3code", "--force"]);
    newgit(&home, &store, &["merge", "main"]);
    let main = main_in(&home, &devs, "t3code").expect("t3code's main on the server");
    assert_eq!(main, purged, "the remote's `main` is the new line");

    // `feature`, still on the old line, works on and checkpoints.
    std::fs::write(feature.join("apps/t3code/more.ts"), "more\n").expect("write");
    git(&home, &feature, &["add", "-A"]);
    git(&home, &feature, &["commit", "-q", "-m", "t3code: more"]);
    let checkpoint = run(
        env!("CARGO_BIN_EXE_newgit"),
        &home,
        &feature,
        &["checkpoint"],
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&checkpoint.stdout),
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    assert!(said.contains("not on the remote"), "{said}");
    assert!(said.contains("dropped"), "{said}");
    assert!(said.contains(&leaked[..8]), "{said}");
    // The instance's t3code commit, split here, is not on the remote.
    let refs = git(
        &home,
        &store,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/newgit/trackers/t3code/instances/",
        ],
    );
    let split = refs
        .lines()
        .find(|line| line.contains("feature"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("feature's t3code branch: {refs}"))
        .to_owned();
    assert!(
        is_ancestor(&home, &store.join(".git"), &leaked, &split),
        "it is on the old line"
    );
    let there = run(
        "git",
        &home,
        &devs,
        &[
            "--git-dir",
            devs.as_str(),
            "cat-file",
            "-e",
            &format!("{split}^{{commit}}"),
        ],
    );
    assert!(
        !there.status.success(),
        "the checkpoint's commit is not on the remote"
    );
}
