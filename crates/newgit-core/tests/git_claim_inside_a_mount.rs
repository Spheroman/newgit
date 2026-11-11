//! A claim of a tracker in Git does not reach into a deeper mount (#426
//! review, second round). `sec`, outside Git with audience `user`, is
//! mounted at `apps/t3code` claiming `secrets`; `pub`, in Git at `.`,
//! claims `apps/t3code/secrets/pub.txt`. That file is `sec`'s: a commit
//! adding it is refused, whatever `pub`'s audience, rather than landing in
//! a tracker in Git (which, with the claim reaching in, it did: in root's
//! `main`, since split decides trackers in Git by mount).
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel"
//! conventions.

mod imports;

use camino::Utf8Path;
use imports::*;
use newgit_core::tracker::{owner_of, unreached_claims};

fn a_git_claim_inside_a_mount_of_an_outside_git_file_is_refused(audience: &str) {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    import(&store, &upstream);
    let repo = store.paths().project_root.clone();
    std::fs::write(
        repo.join(".newgit/trackers/sec.toml"),
        "audience = \"user\"\nstorage = \"local\"\nmount = \"apps/t3code\"\npaths = [\"secrets\"]\n",
    )
    .expect("write sec");
    std::fs::write(
        repo.join(".newgit/trackers/pub.toml"),
        format!(
            "audience = \"{audience}\"\nstorage = \"local\"\ngit = true\npaths = [\"apps/t3code/secrets/pub.txt\"]\n"
        ),
    )
    .expect("write pub");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "sec and pub"]);
    manager(&store).merge("main", "main").expect("publish");

    let all = manager(&store).all_definitions();
    let owner = owner_of(&all, Utf8Path::new("apps/t3code/secrets/pub.txt")).expect("owned");
    assert_eq!((owner.name.as_str(), owner.git), ("sec", false));
    let public = all.iter().find(|d| d.name == "pub").expect("pub");
    assert!(!public.owns(Utf8Path::new("apps/t3code/secrets/pub.txt")));

    let main = git(&repo, &["rev-parse", "main"]);
    let file = repo.join("apps/t3code/secrets/pub.txt");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, "not so public\n").expect("write");
    git(&repo, &["add", "-f", "apps/t3code/secrets/pub.txt"]);
    git(&repo, &["commit", "-q", "-m", "pub.txt"]);
    let error = manager(&store)
        .merge("main", "main")
        .expect_err("a file of `sec` never goes into Git");
    let message = error.to_string();
    assert!(
        message.contains("`apps/t3code/secrets/pub.txt`")
            && message.contains("belongs to `sec`, a tracker outside Git"),
        "{message}"
    );
    let tracker_store = store.paths().tracker_store.clone();
    let history = git(
        &repo,
        &[
            &format!("--git-dir={tracker_store}"),
            "log",
            "--glob=refs/trackers/*",
            "--format=%H",
            "--",
            "secrets/pub.txt",
            "apps/t3code/secrets/pub.txt",
        ],
    );
    assert_eq!(history, "", "no tracker's history holds it");
    git(&repo, &["reset", "-q", "--hard", &main]);
}

#[test]
fn a_git_claim_inside_a_mount_does_not_take_a_file_from_outside_git() {
    a_git_claim_inside_a_mount_of_an_outside_git_file_is_refused("project-devs");
}

#[test]
fn a_public_claim_inside_a_mount_does_not_take_a_user_file() {
    a_git_claim_inside_a_mount_of_an_outside_git_file_is_refused("public");
}

#[test]
fn tracker_list_says_a_git_claim_inside_a_mount_is_not_its() {
    let (_guard, temp) = tempdir();
    let upstream = upstream(&temp);
    let store = project(&temp);
    import(&store, &upstream);
    let repo = store.paths().project_root.clone();
    std::fs::write(
        repo.join(".newgit/trackers/pub.toml"),
        "audience = \"public\"\nstorage = \"local\"\ngit = true\npaths = [\"apps/t3code/pub.txt\", \"docs\"]\n",
    )
    .expect("write pub");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "pub"]);
    let manager = manager(&store);
    let all = manager.all_definitions();
    let public = all.iter().find(|d| d.name == "pub").expect("pub");
    let unreached: Vec<(&str, &str)> = unreached_claims(&all, public)
        .into_iter()
        .map(|(claim, mount)| (claim.as_str(), mount.name.as_str()))
        .collect();
    assert_eq!(unreached, [("apps/t3code/pub.txt", "t3code")]);
    let owner = owner_of(&all, Utf8Path::new("apps/t3code/pub.txt")).expect("owned");
    assert_eq!(owner.name, "t3code");
}
