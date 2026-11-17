//! `newgit pull --remote <target> --force` (#442): a target force-pushed to
//! purge a commit is followed, not merged with. The tracker's line is reset
//! to the target's, the commits only the old line had are listed with every
//! instance still holding them, and this machine refuses to bring them back:
//! not by a merge, and not by an export. An instance with nothing of its own
//! on the old line follows the reset on `newgit pull`.

mod imports;

use camino::Utf8Path;
use imports::*;
use newgit_core::manager::RemoteExportOptions;
use newgit_core::store::MetadataStore;

/// `t3code` imported at `apps/t3code`, published, exported to `github` (a
/// bare copy of its upstream, branch `newgit`) rooted at it.
fn exported(temp: &Utf8Path) -> (MetadataStore, camino::Utf8PathBuf) {
    let upstream = upstream(temp);
    let store = project(temp);
    let github = github_copy(temp, &upstream, "t3code");
    import(&store, &upstream);
    set_export(
        &store,
        "t3code",
        Some(&export_to(&github, "t3code", "branch = \"newgit\"")),
    );
    let repo = store.paths().project_root.clone();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "export target"]);
    manager(&store).merge("main", "main").expect("publish");
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    (store, github)
}

fn main_of(store: &MetadataStore, tracker: &str) -> String {
    store_ref(store, &format!("refs/trackers/{tracker}/main"))
        .unwrap_or_else(|| panic!("`{tracker}` has a main"))
}

/// `github`'s `newgit` rewritten elsewhere: reset to `onto`, one commit of
/// `files` on it, force-pushed. Returns the new tip.
fn force_push(temp: &Utf8Path, github: &Utf8Path, onto: &str, files: &[(&str, &str)]) -> String {
    let clone = temp.join("purge");
    git(
        temp,
        &[
            "clone",
            "-q",
            "-b",
            "newgit",
            github.as_str(),
            clone.as_str(),
        ],
    );
    git(&clone, &["config", "commit.gpgSign", "false"]);
    git(&clone, &["reset", "-q", "--hard", onto]);
    commit_as(&clone, "pat", 2_100_000_000, files, "rotate the key");
    git(&clone, &["push", "-q", "--force", "origin", "newgit"]);
    git(&clone, &["rev-parse", "HEAD"])
}

#[test]
fn a_force_pushed_target_is_followed_and_what_it_dropped_stays_dropped() {
    let (_guard, temp) = tempdir();
    let (store, github) = exported(&temp);
    let repo = store.paths().project_root.clone();
    let clean = main_of(&store, "t3code");

    // A key lands in t3code, and is exported.
    let feature = manager(&store)
        .spawn("feature", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &feature,
        "ana",
        1_800_000_000,
        &[(
            "apps/t3code/src/key.ts",
            "export const KEY = \"sk-live\";\n",
        )],
        "t3code: add the key",
    );
    manager(&store).merge("feature", "main").expect("merge");
    let leaked = main_of(&store, "t3code");
    assert_ne!(leaked, clean);
    manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");

    // One instance only has it; another built on it.
    let holder = manager(&store).spawn("holder", None).expect("spawn");
    manager(&store)
        .checkpoint("holder", None)
        .expect("checkpoint");
    let worker = manager(&store)
        .spawn("worker", None)
        .expect("spawn")
        .branch
        .workspace_path;
    commit_as(
        &worker,
        "ana",
        1_900_000_000,
        &[("apps/t3code/src/more.ts", "more\n")],
        "t3code: more",
    );
    manager(&store)
        .checkpoint("worker", None)
        .expect("checkpoint");

    // The target purges it.
    let purged = force_push(&temp, &github, &clean, &[("src/key.ts", "rotated\n")]);

    // A dry run says what it would drop, and from whom, and moves nothing.
    let dry = manager(&store)
        .reset_to_remote("t3code", true)
        .expect("dry run");
    assert!(dry.dry_run);
    let [line] = dry.lines.as_slice() else {
        panic!("one line: {dry:?}");
    };
    assert_eq!(line.tracker, "t3code");
    assert_eq!(line.from.as_deref(), Some(leaked.as_str()));
    assert_eq!(line.to, purged);
    let dropped: Vec<&str> = line.dropped.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(dropped, [leaked.as_str()]);
    assert_eq!(line.dropped[0].1, "t3code: add the key");
    let mut held: Vec<(&str, usize, usize)> = dry
        .instances
        .iter()
        .map(|held| (held.instance.as_str(), held.dropped, held.own))
        .collect();
    held.sort();
    // `feature`, which merged it, still has it too.
    assert_eq!(
        held,
        [("feature", 1, 0), ("holder", 1, 0), ("worker", 1, 1)],
        "{dry:?}"
    );
    assert_eq!(main_of(&store, "t3code"), leaked, "a dry run moves nothing");
    assert!(dry.view.is_none());

    // Reset: `main` and the project root's pin follow the target.
    let reset = manager(&store)
        .reset_to_remote("t3code", false)
        .expect("reset");
    let view = reset.view.expect("a pin commit");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), view);
    assert!(
        git(&repo, &["log", "-1", "--format=%s"]).starts_with("Reset t3code to "),
        "{}",
        git(&repo, &["log", "-1", "--format=%s"])
    );
    assert_eq!(main_of(&store, "t3code"), purged);
    assert_eq!(
        std::fs::read_to_string(repo.join("apps/t3code/src/key.ts")).expect("key"),
        "rotated\n"
    );

    // Published, it stays purged: the merge replaces the old line.
    manager(&store).merge("main", "main").expect("merge main");
    assert_eq!(main_of(&store, "t3code"), purged);
    let exported = manager(&store)
        .export_to_remote("t3code", RemoteExportOptions::default())
        .expect("export");
    assert_eq!(git(&github, &["rev-parse", "newgit"]), purged);
    assert!(!git_ok(
        &github,
        &["merge-base", "--is-ancestor", &leaked, "newgit"]
    ));
    assert_eq!(exported.tip, purged);

    // The instance built on the old line cannot bring it back.
    let refused = manager(&store)
        .merge("worker", "main")
        .expect_err("worker holds the dropped commit");
    let said = refused.to_string();
    assert!(said.contains("dropped"), "{said}");
    assert!(said.contains(&leaked[..8]), "{said}");
    assert_eq!(main_of(&store, "t3code"), purged);

    // The one with nothing of its own follows the reset.
    let pulled = manager(&store).pull(Some("holder"), &[]).expect("pull");
    assert_eq!(pulled.pulled.len(), 1, "{pulled:?}");
    assert!(pulled.pulled[0].from.contains("reset"), "{pulled:?}");
    assert!(
        !holder
            .branch
            .workspace_path
            .join("apps/t3code/src/key.ts")
            .read_to_string_lossy()
            .contains("sk-live")
    );
    manager(&store)
        .merge("holder", "main")
        .expect("merge holder");
    assert!(!git_ok(
        &repo,
        &[
            "merge-base",
            "--is-ancestor",
            &leaked,
            "refs/newgit/trackers/t3code/main"
        ]
    ));
}

#[test]
fn the_projects_own_view_is_not_reset() {
    let (_guard, temp) = tempdir();
    let (store, _github) = exported(&temp);
    let mirror = temp.join("mirror.git");
    git(
        &temp,
        &["init", "-q", "--bare", "-b", "main", mirror.as_str()],
    );
    set_export(&store, "root", Some(&export_to(&mirror, "mirror", "")));
    let refused = manager(&store)
        .reset_to_remote("mirror", true)
        .expect_err("root's view");
    assert!(refused.to_string().contains("rooted at"), "{refused}");
}

trait ReadLossy {
    fn read_to_string_lossy(&self) -> String;
}

impl ReadLossy for camino::Utf8PathBuf {
    fn read_to_string_lossy(&self) -> String {
        std::fs::read(self)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}
