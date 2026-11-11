// How fast concurrent merges begun from one base land when each must be
// merged on the server (#411). Not run by default:
// `cargo test --release -p newgit --test serve_server_merge_bench -- --ignored --nocapture`.
//
// `MERGES` merges are begun from the same base and their commits sent; then
// all are finished at once from as many threads. `BENCH_MODE` says what each
// changes:
//
// - `mixed` (the default): a new file in `blog` or in root, alternately;
// - `same`: a new file each, all in `blog`;
// - `spread`: a new file each in a tracker of its own (`t0` … `t23`);
// - `overlap`: a line each of one file in root, so every merge onto a head
//   that moved runs Git's merge (cleanly: the lines are apart).
//
// Every one but the first lands on what landed before it: re-made on the new
// head when the two changed different paths, merged again when they did not.

include!("support/serve.rs");
include!("support/serve_merge.rs");

const MERGES: usize = 24;
/// A file of four lines per merge, with merge `n`'s own line changed: each
/// merge edits the same file, three lines from any other's change.
fn long_file(changed: Option<usize>) -> String {
    (0..MERGES * 4)
        .map(|line| match changed {
            Some(n) if line == n * 4 + 1 => format!("line {line}, changed by merge {n}\n"),
            _ => format!("line {line}\n"),
        })
        .collect()
}

fn mode() -> String {
    std::env::var("BENCH_MODE").unwrap_or_else(|_| "mixed".into())
}

#[test]
#[ignore = "a measurement, not a check"]
fn concurrent_merges_from_one_base_per_second() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let store = merge_project(&temp, &home, &server);
    if mode() == "spread" {
        for n in 0..MERGES {
            std::fs::create_dir_all(store.join(format!("t{n}"))).expect("mkdir");
            std::fs::write(store.join(format!("t{n}/keep.md")), "k\n").expect("write");
        }
        git(&home, &store, &["add", "."]);
        git(&home, &store, &["commit", "-q", "-m", "dirs"]);
        for n in 0..MERGES {
            newgit(
                &home,
                &store,
                &[
                    "tracker",
                    "create",
                    &format!("t{n}"),
                    "--git",
                    "--audience",
                    "public",
                    "--path",
                    &format!("t{n}"),
                ],
            );
        }
        newgit(&home, &store, &["merge", "main"]);
    }
    if mode() == "overlap" {
        std::fs::write(store.join("long.md"), long_file(None)).expect("write");
        git(&home, &store, &["add", "long.md"]);
        git(&home, &store, &["commit", "-q", "-m", "a long file"]);
        newgit(&home, &store, &["merge", "main"]);
    }
    let base = list_of(&server, &token, "main");
    let spread: Vec<Utf8PathBuf> = if mode() == "spread" {
        (0..MERGES)
            .map(|n| clone_tracker(&server, &token, &temp, &format!("t{n}"), &format!("c{n}")))
            .collect()
    } else {
        Vec::new()
    };
    let blog = clone_tracker(&server, &token, &temp, "blog", "blog");
    let root = clone_tracker(&server, &token, &temp, "root", "root");
    let mut lists = Vec::new();
    for n in 0..MERGES {
        let mut list = base.clone();
        let mut sent = Vec::new();
        if mode() == "overlap" {
            let root_commit = commit_on(
                &home,
                &root,
                &base["root"],
                &[("long.md", Some(&long_file(Some(n))))],
                &[],
                &format!("line {n}"),
            );
            list.insert("root".into(), root_commit.clone());
            sent.push(("root", root.clone(), root_commit));
        } else if mode() == "spread" {
            let t = format!("t{n}");
            let c = commit_on(
                &home,
                spread.get(n).expect("a clone"),
                &base[&t],
                &[(&format!("post-{n}.md"), Some("words\n"))],
                &[],
                "p",
            );
            list.insert(t.clone(), c.clone());
            sent.push((
                Box::leak(t.into_boxed_str()) as &str,
                spread[n].clone(),
                c.clone(),
            ));
            let rc = commit_on(
                &home,
                &root,
                &base["root"],
                &[],
                &[(&format!("t{n}"), &c)],
                "Update",
            );
            list.insert("root".into(), rc.clone());
            sent.push(("root", root.clone(), rc));
        } else if n % 2 == 0 || mode() == "same" {
            let blog_commit = commit_on(
                &home,
                &blog,
                &base["blog"],
                &[(&format!("post-{n}.md"), Some("words\n"))],
                &[],
                &format!("post {n}"),
            );
            list.insert("blog".into(), blog_commit.clone());
            sent.push(("blog", blog.clone(), blog_commit.clone()));
            let root_commit = commit_on(
                &home,
                &root,
                &base["root"],
                &[],
                &[("blog", &blog_commit)],
                "Update blog",
            );
            list.insert("root".into(), root_commit.clone());
            sent.push(("root", root.clone(), root_commit));
        } else {
            let root_commit = commit_on(
                &home,
                &root,
                &base["root"],
                &[(&format!("notes-{n}.md"), Some("words\n"))],
                &[],
                &format!("notes {n}"),
            );
            list.insert("root".into(), root_commit.clone());
            sent.push(("root", root.clone(), root_commit));
        }
        let sent: Vec<Sent<'_>> = sent
            .iter()
            .map(|(tracker, dir, commit)| Sent {
                tracker,
                dir,
                commit: commit.clone(),
            })
            .collect();
        let (status, begun) = begin_by_hand(&server, &token, "main", &base, &list, &sent);
        assert_eq!(status, 200, "{begun}");
        lists.push(list);
    }
    eprintln!("SETUP done: {} trackers in the index", base.len());
    let start = std::time::Instant::now();
    let threads: Vec<_> = lists
        .into_iter()
        .map(|list| {
            let (url, token, base) = (server.url.clone(), token.clone(), base.clone());
            std::thread::spawn(move || {
                let bearer = format!("Bearer {token}");
                http(
                    &url,
                    "PUT",
                    "/v1/projects/jack/store/branches/main",
                    &[("authorization", bearer.as_str())],
                    &serde_json::json!({ "base": base, "list": list, "moves": [] }).to_string(),
                )
            })
        })
        .collect();
    let (mut merged, mut refused, mut runs) = (0, 0, 0);
    for thread in threads {
        let (status, answer) = thread.join().expect("joined");
        if status != 200 {
            eprintln!("REFUSED {status} {answer}");
            refused += 1;
            continue;
        }
        merged += usize::from(answer["merged"] == true);
        runs += answer["server_merges"].as_u64().unwrap_or(0);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{}: {MERGES} merges from one base, finished at once: {:.2} s, {:.2} landed a second, {merged} merged by the server, {runs} runs of Git's merge, {refused} refused",
        mode(),
        elapsed.as_secs_f64(),
        (MERGES - refused) as f64 / elapsed.as_secs_f64()
    );
    assert_eq!(refused, 0, "every merge lands");
    let landed = list_of(&server, &token, "main");
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    let files = in_store(&home, &devs, &["ls-tree", "--name-only", &landed["root"]]);
    if mode() != "mixed" {
        return;
    }
    for n in (1..MERGES).step_by(2) {
        assert!(
            files.lines().any(|file| file == format!("notes-{n}.md")),
            "notes-{n}.md landed"
        );
    }
}
