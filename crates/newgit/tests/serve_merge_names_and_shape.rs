// The re-make keeps a name as Git stores it, and leaves a merge that
// reshapes the composed tree to Git (#436, second review, blockers 1 and
// 2), each against a real server.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

/// jack moves every file of `blog` (mounted at blog/) into root's `posts/`
/// and lands; ana, from the base, adds `blog/ana.md`. Git's merge of the
/// composed views follows the directory rename blog/ -> posts/.
#[test]
fn a_tracker_emptied_into_root_is_followed_by_the_merge() {
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let (home, jack, ana) = two_people(&server, &temp);
    set_blog(
        &server,
        &jack,
        &temp,
        &home,
        &[("maintainers", "project-devs")],
        "members",
    );
    let base = list_of(&server, &jack, "main");
    // jack: blog emptied, root gains posts/post.md
    let jblog = clone_tracker(&server, &jack, &temp, "blog", "blog-jack");
    let jroot = clone_tracker(&server, &jack, &temp, "root", "root-jack");
    let jb = commit_on(
        &home,
        &jblog,
        &base["blog"],
        &[("post.md", None)],
        &[],
        "move out",
    );
    let jr = commit_on(
        &home,
        &jroot,
        &base["root"],
        &[("posts/post.md", Some("hello\n"))],
        &[("blog", &jb)],
        "move in",
    );
    let mut jl = base.clone();
    jl.insert("blog".into(), jb.clone());
    jl.insert("root".into(), jr.clone());
    let (s, a) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jl,
        &[
            Sent {
                tracker: "blog",
                dir: &jblog,
                commit: jb.clone(),
            },
            Sent {
                tracker: "root",
                dir: &jroot,
                commit: jr.clone(),
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(s, 200, "{a}");
    // ana: adds blog/ana.md from base
    let ablog = clone_tracker(&server, &ana, &temp, "blog", "blog-ana");
    let aroot = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    let ab = commit_on(
        &home,
        &ablog,
        &base["blog"],
        &[("ana.md", Some("ana's\n"))],
        &[],
        "ana page",
    );
    let ar = commit_on(
        &home,
        &aroot,
        &base["root"],
        &[],
        &[("blog", &ab)],
        "Update blog",
    );
    let mut al = base.clone();
    al.insert("blog".into(), ab.clone());
    al.insert("root".into(), ar.clone());
    let (s, a) = merge_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &al,
        &[
            Sent {
                tracker: "blog",
                dir: &ablog,
                commit: ab.clone(),
            },
            Sent {
                tracker: "root",
                dir: &aroot,
                commit: ar.clone(),
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(s, 200, "{a}");
    assert_eq!(a["server_merges"], 1, "Git's merge ran: {a}");
    let landed = list_of(&server, &jack, "main");
    let (devs, public) = devs_and_public(&server);
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "posts/ana.md").as_deref(),
        Some("ana's\n"),
        "ana's file follows blog/ to posts/, as Git's merge of the composed trees has it"
    );
    assert_eq!(file_at(&home, &public, &landed["blog"], "ana.md"), None);
}

/// ana adds a file whose name is not UTF-8 (bytes `caf\xe9.md`) to root;
/// jack, meanwhile, lands another root file. Disjoint: re-made by reparent.
#[test]
fn a_name_that_is_not_utf8_lands_from_the_remake() {
    use std::os::unix::ffi::OsStrExt;
    let (_guard, temp) = tempdir();
    let faults = temp.join("faults");
    std::fs::write(&faults, "").expect("faults");
    let server = start_with_faults(&temp, &faults);
    let (home, jack, ana) = two_people(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let aroot = clone_tracker(&server, &ana, &temp, "root", "root-ana");
    git(
        &home,
        &aroot,
        &["checkout", "-q", "--detach", &base["root"]],
    );
    std::fs::write(aroot.join("blobsrc"), "ana's bytes\n").expect("write");
    let blob = git(&home, &aroot, &["hash-object", "-w", "blobsrc"]);
    let mut spec = b"100644,".to_vec();
    spec.extend_from_slice(blob.trim().as_bytes());
    spec.extend_from_slice(b",caf\xe9.md");
    let out = std::process::Command::new("git")
        .current_dir(&aroot)
        .env("HOME", &home)
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(std::ffi::OsStr::from_bytes(&spec))
        .output()
        .expect("git update-index");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(&home, &aroot, &["commit", "-q", "-m", "ana non-utf8"]);
    let ar = git(&home, &aroot, &["rev-parse", "HEAD"]);
    let ar = ar.trim().to_owned();
    let mut al = base.clone();
    al.insert("root".into(), ar.clone());
    let (s, a) = begin_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &al,
        &[Sent {
            tracker: "root",
            dir: &aroot,
            commit: ar.clone(),
        }],
    );
    assert_eq!(s, 200, "{a}");
    let jl = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[("jack.md", Some("jack\n"))],
        "jack",
    );
    let (s, a) = finish_by_hand(&server, &jack, "main", &base, &jl, serde_json::json!([]));
    assert_eq!(s, 200, "{a}");
    let (s, a) = finish_by_hand(&server, &ana, "main", &base, &al, serde_json::json!([]));
    assert_eq!(s, 200, "{a}");
    assert_eq!(a["server_merges"], 0, "re-made, no Git merge: {a}");
    let landed = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);
    // `ls-tree` quotes a name past ASCII: `"caf\351.md"`.
    let tree = in_store(&home, &devs, &["ls-tree", "--name-only", &landed["root"]]);
    assert!(tree.contains("caf\\351.md"), "ana's file landed:\n{tree}");
    assert!(tree.contains("jack.md"), "{tree}");
    assert_eq!(parents_of(&home, &devs, &landed["root"]).len(), 2);
}
