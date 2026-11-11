// How fast concurrent merges begun from one base land when each must be
// merged on the server (#411). Not run by default:
// `cargo test --release -p newgit --test serve_server_merge_bench -- --ignored --nocapture`.
//
// `MERGES` merges, each adding its own file to `blog` or to root
// (alternately), are begun from the same base and their commits sent; then
// all are finished at once from as many threads. Every one but the first is
// merged onto what landed before it, and each lost recheck merges again.

include!("support/serve.rs");
include!("support/serve_merge.rs");

const MERGES: usize = 24;

#[test]
#[ignore = "a measurement, not a check"]
fn concurrent_merges_from_one_base_per_second() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let home = temp.join("jack");
    let token = login(&server, &home, "jack");
    let _store = merge_project(&temp, &home, &server);
    let base = list_of(&server, &token, "main");
    let blog = clone_tracker(&server, &token, &temp, "blog", "blog");
    let root = clone_tracker(&server, &token, &temp, "root", "root");
    let mut lists = Vec::new();
    for n in 0..MERGES {
        let mut list = base.clone();
        let mut sent = Vec::new();
        if n % 2 == 0 {
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
    let mut merged = 0;
    for thread in threads {
        let (status, answer) = thread.join().expect("joined");
        assert_eq!(status, 200, "{answer}");
        merged += usize::from(answer["merged"] == true);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{MERGES} merges from one base, finished at once: {:.2} s, {:.1} merges a second, {merged} merged by the server",
        elapsed.as_secs_f64(),
        MERGES as f64 / elapsed.as_secs_f64()
    );
    let landed = list_of(&server, &token, "main");
    let devs = server.data.join("homes/jack/store/stores/project-devs.git");
    let files = in_store(&home, &devs, &["ls-tree", "--name-only", &landed["root"]]);
    for n in (1..MERGES).step_by(2) {
        assert!(
            files.lines().any(|file| file == format!("notes-{n}.md")),
            "notes-{n}.md landed"
        );
    }
}
