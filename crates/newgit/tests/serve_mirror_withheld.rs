// What a webhook pull on `newgit serve` does about work the mirror never
// saw (#404, #451): the check is of what the commits being landed contain,
// never of a branch's name.
//
// - #451 (b): an instance with work in the withheld tracker, and a pull
//   request on the mirror whose commits are not that instance's: it lands.
//   As a merge commit, and as a squash whose pull request GitHub cannot be
//   asked about until its `pull_request` event says (#398: the pull waits,
//   and says why, rather than refusing).
// - An instance's own exported commit, whose withheld work is not on
//   `main`, merged on the mirror: refused, naming the instance, the
//   tracker, and both ways on. #451 (c): a later pull carrying it is
//   refused once, on the first, not again with its own misleading reason;
//   after `newgit merge <instance>`, redelivering the later event lands it.

include!("support/serve.rs");
include!("support/serve_mirror.rs");

/// Instance `name` with `files` written and committed in its workspace, and
/// pushed: its checkpoint (its forks) on the server, its branch exported to
/// the mirror when it touched what the mirror ships.
fn instance_pushed(
    home: &Utf8Path,
    store: &Utf8Path,
    name: &str,
    files: &[(&str, &str)],
) -> Utf8PathBuf {
    newgit(home, store, &["spawn", name]);
    let workspace = Utf8PathBuf::from(newgit(home, store, &["status", name, "--path"]));
    for (path, text) in files {
        std::fs::write(workspace.join(path), text).expect("write");
        git(home, &workspace, &["add", path]);
    }
    git(
        home,
        &workspace,
        &["commit", "-q", "-m", &format!("{name}'s work")],
    );
    git(home, &workspace, &["push", "-q"]);
    workspace
}

/// A pull request on the mirror changing `path` to `text`, from its own
/// branch `branch` on `main`, merged there with a merge commit: the merge.
fn merged_pr(
    home: &Utf8Path,
    work: &Utf8Path,
    branch: &str,
    number: u32,
    path: &str,
    text: &str,
) -> String {
    git(home, work, &["fetch", "-q", "origin"]);
    git(home, work, &["checkout", "-q", "-B", "main", "origin/main"]);
    git(home, work, &["checkout", "-q", "-B", branch]);
    std::fs::write(work.join(path), text).expect("write");
    git(
        home,
        work,
        &["commit", "-q", "-am", &format!("change {path}")],
    );
    git(home, work, &["checkout", "-q", "main"]);
    git(
        home,
        work,
        &[
            "merge",
            "-q",
            "--no-ff",
            branch,
            "-m",
            &format!("Merge pull request #{number} from jack/{branch}"),
        ],
    );
    git(home, work, &["push", "-q", "origin", "main"]);
    git(home, work, &["rev-parse", "HEAD"])
}

#[test]
fn a_pull_request_from_a_branch_named_for_an_instance_with_other_withheld_work_lands() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    // `fixer` has work in `t3code`, which the mirror does not ship, and
    // none of it is on `main`.
    instance_pushed(
        &home,
        &store,
        "fixer",
        &[("apps/t3code/app.ts", "export const fixer = 1\n")],
    );

    // A pull request on a branch named for it, whose commits are not its.
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    let merged = merged_pr(
        &home,
        &work,
        "fixer",
        1,
        "crates/serve/b.rs",
        "fn b() { 2 }\n",
    );
    let (status, answer) = deliver(&server, "push", "d-b1", &push_event(&merged, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");
    assert_eq!(
        main_file(&server, &token, &temp, "crates/serve/b.rs"),
        "fn b() { 2 }\n"
    );
    // `fixer`'s work stays on its fork.
    assert_eq!(
        main_file(&server, &token, &temp, "apps/t3code/app.ts"),
        "export {}\n"
    );

    // A squash: a new commit on `main`'s line, whose pull request only
    // GitHub can say. GitHub cannot be reached, so the pull waits, saying
    // why, and refuses nothing.
    // `main` had not moved: the pull fast-forwarded it to the mirror's own
    // commits, so the export finds the mirror where it is.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while state(&server, &token)["exports"][0]["tip"] != merged.as_str() {
        assert!(
            std::time::Instant::now() < until,
            "the export never came back to {merged}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let exported = mirror_main_until(&mirror, |tip| ancestor(&mirror, &merged, tip));
    git(&home, &work, &["fetch", "-q", "origin"]);
    git(
        &home,
        &work,
        &["checkout", "-q", "-B", "main", "origin/main"],
    );
    assert_eq!(git(&home, &work, &["rev-parse", "HEAD"]), exported);
    git(&home, &work, &["checkout", "-q", "-b", "squashed"]);
    std::fs::write(work.join("README.md"), "store, squashed\n").expect("write");
    git(
        &home,
        &work,
        &["commit", "-q", "-am", "head of the squashed pull request"],
    );
    let head = git(&home, &work, &["rev-parse", "HEAD"]);
    git(&home, &work, &["checkout", "-q", "main"]);
    git(&home, &work, &["merge", "-q", "--squash", "squashed"]);
    git(
        &home,
        &work,
        &["commit", "-q", "-m", "Squashed pull request (#2)"],
    );
    let squash = git(&home, &work, &["rev-parse", "HEAD"]);
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let (status, answer) = deliver(&server, "push", "d-b2", &push_event(&squash, "jack"));
    assert_eq!(status, 202, "{answer}");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let now = state(&server, &token);
        let pull = &now["pulls"][0];
        if pull["lookupFailed"].is_string() {
            assert_eq!(pull["step"], "asking GitHub", "{now}");
            break;
        }
        assert!(
            now["pulls"]
                .as_array()
                .is_some_and(|pulls| !pulls.is_empty()),
            "it ended without asking: {now}"
        );
        assert!(
            std::time::Instant::now() < until,
            "it never said GitHub could not answer: {now}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let status = newgit(&home, &store, &["status"]);
    assert!(status.contains("GitHub could not say"), "{status}");

    // GitHub's `pull_request` event says which head the squash merged.
    let event = serde_json::json!({
        "action": "closed",
        "repository": { "full_name": "jack/mirror" },
        "pull_request": {
            "merged": true, "merge_commit_sha": squash,
            "head": { "sha": head, "ref": "squashed", "repo": { "full_name": "jack/mirror" } },
        },
    })
    .to_string();
    let (status, answer) = deliver(&server, "pull_request", "d-b2-pr", &event);
    assert_eq!(status, 200, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");
    assert_eq!(
        main_file(&server, &token, &temp, "README.md"),
        "store, squashed\n"
    );
}

#[test]
fn an_instances_own_commit_is_refused_once_and_lands_after_the_instance_merges() {
    let (_guard, temp) = tempdir();
    let github = Github::new(&temp);
    let mirror = github.repo("jack/mirror");
    let server = start_mirrored(&temp, &github, &no_api());
    let home = temp.join("jack");
    github.for_client(&home);
    let token = login(&server, &home, "jack");
    let store = mirrored_project(&temp, &home, &server);
    mirror_main_until(&mirror, |tip| !tip.is_empty());

    // `worker` changes root and `t3code`: its branch on the mirror has the
    // root change only.
    instance_pushed(
        &home,
        &store,
        "worker",
        &[
            ("README.md", "store, by worker\n"),
            ("apps/t3code/app.ts", "export const worker = 1\n"),
        ],
    );
    let work = mirror_clone(&home, &temp, &mirror, "pr");
    git(&home, &work, &["fetch", "-q", "origin"]);
    let branches = git(&home, &work, &["branch", "-r"]);
    let exported = branches
        .lines()
        .map(str::trim)
        .find(|branch| branch.contains("worker"))
        .unwrap_or_else(|| panic!("worker's branch was not exported: {branches}"))
        .trim_start_matches("origin/")
        .to_owned();
    git(&home, &work, &["checkout", "-q", "main"]);
    git(
        &home,
        &work,
        &[
            "merge",
            "-q",
            "--no-ff",
            &format!("origin/{exported}"),
            "-m",
            &format!("Merge pull request #3 from jack/{exported}"),
        ],
    );
    git(&home, &work, &["push", "-q", "origin", "main"]);
    let first = git(&home, &work, &["rev-parse", "HEAD"]);
    let (status, answer) = deliver(&server, "push", "d-c1", &push_event(&first, "jack"));
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let ended = last_ended(&done);
    assert_eq!(ended["outcome"], "refused", "{done}");
    let why = ended["why"].as_str().unwrap_or_default();
    assert!(why.contains("`worker`"), "{why}");
    assert!(why.contains("`t3code`"), "{why}");
    assert!(why.contains("newgit merge worker"), "{why}");
    assert!(why.contains("newgit pull --remote mirror"), "{why}");

    // A later, unrelated pull request carries the refused merge: refused
    // with it, said once, on the first.
    let second = merged_pr(
        &home,
        &work,
        "later",
        4,
        "crates/serve/b.rs",
        "fn b() { 4 }\n",
    );
    let later = push_event(&second, "jack");
    let (status, answer) = deliver(&server, "push", "d-c2", &later);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    let ended = done["ended"].as_array().cloned().unwrap_or_default();
    let refused: Vec<&Value> = ended
        .iter()
        .filter(|one| one["outcome"] == "refused")
        .collect();
    assert_eq!(refused.len(), 1, "one refusal, said once: {done}");
    assert_eq!(refused[0]["commit"], first.as_str(), "{done}");
    let why = refused[0]["why"].as_str().unwrap_or_default();
    assert!(why.contains("1 later pull"), "{why}");
    let status = newgit(&home, &store, &["status"]);
    assert_eq!(status.matches("was refused").count(), 1, "{status}");

    // The way on it names: land the instance, then redeliver.
    newgit(&home, &store, &["merge", "worker"]);
    let (status, answer) = deliver(&server, "push", "d-c2", &later);
    assert_eq!(status, 202, "{answer}");
    let done = pulls_done(&server, &token);
    assert_eq!(last_ended(&done)["outcome"], "landed", "{done}");
    assert_eq!(
        main_file(&server, &token, &temp, "crates/serve/b.rs"),
        "fn b() { 4 }\n"
    );
    assert_eq!(
        main_file(&server, &token, &temp, "apps/t3code/app.ts"),
        "export const worker = 1\n"
    );
    assert_eq!(
        main_file(&server, &token, &temp, "README.md"),
        "store, by worker\n"
    );
    let status = newgit(&home, &store, &["status"]);
    assert!(
        !status.contains("was refused"),
        "the trouble is over once it lands: {status}"
    );
}
