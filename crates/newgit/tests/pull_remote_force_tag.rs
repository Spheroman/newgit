//! `newgit pull --remote <target> --force` (#442): a tag pushed from the
//! project root goes through the same check as every push to a target, so a
//! tag after a rewind-only purge of the project's own view does not put the
//! purged commit back; and when a push of `main` and a tag has `main`'s
//! export refused, the tag is not pushed to that target either.

#![allow(dead_code)]

include!("support/fake_remote.rs");

fn bare(fixture: &Fixture, name: &str) -> Utf8PathBuf {
    let repo = fixture.temp.join(format!("{name}.git"));
    git(
        &fixture.temp,
        &["init", "-q", "--bare", "-b", "main", repo.as_str()],
    );
    repo
}

/// `git push` as someone with newgit installed runs it, from the project
/// root: whether it succeeded, and what it said.
fn push(fixture: &Fixture, args: &[&str]) -> (bool, String) {
    let binary = Utf8PathBuf::from(env!("CARGO_BIN_EXE_newgit"));
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.store.as_str())
        .arg("push")
        .args(args)
        .env("HOME", fixture.temp.join("home").as_str())
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
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The project's own view exported to `mirror`, a key committed, pushed
/// (and so exported), then purged from `mirror` by the simplest purge
/// there is: a rewind, force-pushed. Returns (mirror, the purged commit).
fn leaked_then_rewound(fixture: &Fixture) -> (Utf8PathBuf, String) {
    let store = fixture.store.clone();
    let mirror = bare(fixture, "mirror");
    set_export(&store, "root", Some(&export_to(&mirror, "github", "")));
    set_export(&store, "access", Some("\"inherit\""));
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "targets"]);
    fixture.ok(&store, &["merge", "main"]);
    fixture.ok(&store, &["export", "--remote", "github"]);
    let before = git(&mirror, &["rev-parse", "refs/heads/main"]);

    std::fs::write(store.join("key.txt"), "sk-live\n").expect("write");
    git(&store, &["add", "-A"]);
    git(&store, &["commit", "-q", "-m", "add the key"]);
    let (ok, said) = push(fixture, &["origin", "main"]);
    assert!(ok, "{said}");
    let leaked = git(&mirror, &["rev-parse", "refs/heads/main"]);
    assert_ne!(leaked, before, "the key was exported");

    git(&mirror, &["update-ref", "refs/heads/main", &before]);
    assert_eq!(git(&mirror, &["for-each-ref", "--contains", &leaked]), "");
    (mirror, leaked)
}

fn refs_holding(mirror: &Utf8Path, commit: &str) -> String {
    git(
        mirror,
        &["for-each-ref", "--contains", commit, "--format=%(refname)"],
    )
}

#[test]
fn a_tag_after_a_rewind_does_not_republish() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (mirror, leaked) = leaked_then_rewound(&fixture);

    git(&fixture.store, &["tag", "v1.0.0"]);
    let (ok, said) = push(&fixture, &["origin", "v1.0.0"]);
    let refs = refs_holding(&mirror, &leaked);
    assert_eq!(
        refs, "",
        "the purged commit is back on the target under {refs}"
    );
    assert!(
        !ok,
        "a tag that did not reach a target fails the push: {said}"
    );
    assert!(said.contains("did not reach `github`"), "{said}");
    assert!(said.contains("was rewritten"), "{said}");
}

#[test]
fn a_refused_export_of_main_stops_its_tag_too() {
    let (_guard, temp) = tempdir();
    let fixture = setup(&temp);
    let (mirror, leaked) = leaked_then_rewound(&fixture);

    std::fs::write(fixture.store.join("more.txt"), "more\n").expect("write");
    git(&fixture.store, &["add", "-A"]);
    git(&fixture.store, &["commit", "-q", "-m", "more"]);
    git(&fixture.store, &["tag", "v1.0.0"]);
    let (ok, said) = push(&fixture, &["origin", "main", "v1.0.0"]);
    let refs = refs_holding(&mirror, &leaked);
    assert_eq!(
        refs, "",
        "the purged commit is back on the target under {refs}"
    );
    assert!(!ok, "{said}");
    assert!(said.contains("was rewritten"), "{said}");
    assert!(
        said.contains("so the tag naming it was not pushed there either"),
        "{said}"
    );
    assert_eq!(git(&mirror, &["tag", "--list"]), "", "no tag on the target");
}
