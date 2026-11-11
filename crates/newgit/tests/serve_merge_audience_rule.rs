// A server merge tells the merger only what it may read, and carries no
// content into a tracker more people may read than chose to widen it
// (#411 review, blocker 3). A carry that widens nothing, `blog` (public)
// into root (the members'), is followed: `serve_server_merge.rs`.

include!("support/serve.rs");
include!("support/serve_merge.rs");
include!("support/serve_merge_people.rs");

const NOTES: &str = "first line\nsecond line\nthird line\nfourth line\nfifth line\n";

/// `merge_project`'s project with a tracker `vault` only jack reads.
fn with_vault(server: &Server, temp: &Utf8Path) -> (Utf8PathBuf, String, String) {
    let home = temp.join("jack");
    let jack = login(server, &home, "jack");
    let ana = login(server, &temp.join("ana"), "ana");
    let store = merge_project(temp, &home, server);
    add_member(server, &jack, "ana");
    std::fs::create_dir_all(store.join("vault")).expect("mkdir");
    std::fs::write(store.join("vault/keep.md"), "k\n").expect("write");
    std::fs::write(store.join("vault/plan.md"), NOTES).expect("write");
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
    (home, jack, ana)
}

/// jack moves root's notes.md into vault and edits its last line; ana, from
/// the base before, edits the same line in root. The conflict is real, and
/// ana is told so without the name of a file only jack may read.
#[test]
fn a_conflict_in_a_tracker_the_merger_cannot_read_is_not_named() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, ana) = with_vault(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let ana_list = root_change(
        &server,
        &ana,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&NOTES.replace("fifth line", "fifth line by ana")),
        )],
        "ana-notes",
    );
    let vault = clone_tracker(&server, &jack, &temp, "vault", "vault-jack");
    let root = clone_tracker(&server, &jack, &temp, "root", "root-jack");
    let jack_vault = commit_on(
        &home,
        &vault,
        &base["vault"],
        &[(
            "payroll-q3.md",
            Some(&NOTES.replace("fifth line", "fifth line by jack")),
        )],
        &[],
        "notes to the vault",
    );
    let jack_root = commit_on(
        &home,
        &root,
        &base["root"],
        &[("notes.md", None)],
        &[("vault", &jack_vault)],
        "notes to the vault",
    );
    let mut jack_list = base.clone();
    jack_list.insert("vault".into(), jack_vault.clone());
    jack_list.insert("root".into(), jack_root.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &jack_list,
        &[
            Sent {
                tracker: "vault",
                dir: &vault,
                commit: jack_vault,
            },
            Sent {
                tracker: "root",
                dir: &root,
                commit: jack_root,
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");

    let (status, answer) = finish_by_hand(
        &server,
        &ana,
        "main",
        &base,
        &ana_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 409, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(!error.contains("payroll"), "{error}");
    assert!(
        error.contains("one or more files in trackers you cannot read"),
        "{error}"
    );
}

/// The same conflict, merged by jack (who reads the vault), names the file.
#[test]
fn a_conflict_the_merger_may_read_is_named() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, _ana) = with_vault(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let late = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&NOTES.replace("fifth line", "fifth line, late")),
        )],
        "late",
    );
    let first = root_change(
        &server,
        &jack,
        &temp,
        &home,
        &base,
        &[(
            "notes.md",
            Some(&NOTES.replace("fifth line", "fifth line, first")),
        )],
        "first",
    );
    let (status, answer) =
        finish_by_hand(&server, &jack, "main", &base, &first, serde_json::json!([]));
    assert_eq!(status, 200, "{answer}");
    let (status, answer) =
        finish_by_hand(&server, &jack, "main", &base, &late, serde_json::json!([]));
    assert_eq!(status, 409, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`notes.md`"),
        "{answer}"
    );
}

/// One of jack's sessions moves `vault/plan.md` (his alone) into root (the
/// members'); another, from the base before, edits it where it was. Git
/// would follow the move and put the second edit in root: content nobody
/// chose to widen. Refused, saying where it would have gone.
#[test]
fn an_edit_is_not_carried_into_a_wider_tracker_by_another_landings_move() {
    let (_guard, temp) = tempdir();
    let server = start(&temp);
    let (home, jack, _ana) = with_vault(&server, &temp);
    let base = list_of(&server, &jack, "main");
    let (devs, _) = devs_and_public(&server);

    // The edit, where the plan was.
    let vault_b = clone_tracker(&server, &jack, &temp, "vault", "vault-b");
    let root_b = clone_tracker(&server, &jack, &temp, "root", "root-b");
    let b_vault = commit_on(
        &home,
        &vault_b,
        &base["vault"],
        &[(
            "plan.md",
            Some(&NOTES.replace("fifth line", "fifth line: the real numbers")),
        )],
        &[],
        "the plan's numbers",
    );
    let b_root = commit_on(
        &home,
        &root_b,
        &base["root"],
        &[],
        &[("vault", &b_vault)],
        "Update vault",
    );
    let mut b_list = base.clone();
    b_list.insert("vault".into(), b_vault.clone());
    b_list.insert("root".into(), b_root.clone());
    let (status, begun) = begin_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &b_list,
        &[
            Sent {
                tracker: "vault",
                dir: &vault_b,
                commit: b_vault,
            },
            Sent {
                tracker: "root",
                dir: &root_b,
                commit: b_root,
            },
        ],
    );
    assert_eq!(status, 200, "{begun}");

    // The move, landed first: jack's own choice, from content he read.
    let vault_a = clone_tracker(&server, &jack, &temp, "vault", "vault-a");
    let root_a = clone_tracker(&server, &jack, &temp, "root", "root-a");
    let a_vault = commit_on(
        &home,
        &vault_a,
        &base["vault"],
        &[("plan.md", None)],
        &[],
        "the plan goes public",
    );
    let a_root = commit_on(
        &home,
        &root_a,
        &base["root"],
        &[("plan.md", Some(NOTES))],
        &[("vault", &a_vault)],
        "the plan goes public",
    );
    let mut a_list = base.clone();
    a_list.insert("vault".into(), a_vault.clone());
    a_list.insert("root".into(), a_root.clone());
    let (status, answer) = merge_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &a_list,
        &[
            Sent {
                tracker: "vault",
                dir: &vault_a,
                commit: a_vault,
            },
            Sent {
                tracker: "root",
                dir: &root_a,
                commit: a_root,
            },
        ],
        serde_json::json!([]),
    );
    assert_eq!(status, 200, "{answer}");
    let landed = list_of(&server, &jack, "main");

    let (status, answer) = finish_by_hand(
        &server,
        &jack,
        "main",
        &base,
        &b_list,
        serde_json::json!([]),
    );
    assert_eq!(status, 409, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("would carry `vault/plan.md` from `vault` into `root`"),
        "{error}"
    );
    assert_eq!(list_of(&server, &jack, "main"), landed, "nothing landed");
    assert_eq!(
        file_at(&home, &devs, &landed["root"], "plan.md").as_deref(),
        Some(NOTES),
        "root has only what was moved there"
    );
    // Nor is the edit's content anywhere the members may fetch it by id.
    let edited = NOTES.replace("fifth line", "fifth line: the real numbers");
    std::fs::write(temp.join("edited.md"), &edited).expect("write");
    let blob = git(&home, &temp, &["hash-object", "edited.md"]);
    assert!(
        !store_has(&devs, &blob),
        "the edit is not in the members' store"
    );
}
