//! A merge's ancestry questions, answered in process (#445), agree with
//! `git merge-base --is-ancestor`: near, across a merge, between sides that
//! never meet, for commits the repository lacks, past the walk's limit,
//! where Git answers, and in a repository whose parents Git reads other
//! than as stored (shallow, or grafted).

use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::Utf8Path;
use newgit_serve::readers::{Readers, WALK};

mod support;

fn git(dir: &Utf8Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(args)
        .output()
        .expect("git runs")
}

/// A history in `dir`: `main`, a line of `long` commits; `side`, two
/// commits off its third; and `merged`, `main` merging `side`.
fn history(dir: &Utf8Path, long: usize) {
    let mut script = String::new();
    let mut mark = 0;
    let mut commit = |script: &mut String,
                      branch: &str,
                      from: Option<String>,
                      merge: Option<String>| {
        mark += 1;
        script.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{mark}\ncommitter t <t@example.com> {mark} +0000\ndata 2\nc\n"
        ));
        if let Some(from) = from {
            script.push_str(&format!("from {from}\n"));
        }
        if let Some(merge) = merge {
            script.push_str(&format!("merge {merge}\n"));
        }
        script.push_str(&format!(
            "M 644 inline f\ndata {}\n{mark}\n\n",
            mark.to_string().len() + 1
        ));
        mark
    };
    for _ in 0..long {
        commit(&mut script, "main", None, None);
    }
    let first = commit(&mut script, "side", Some(":3".into()), None);
    let side = commit(&mut script, "side", Some(format!(":{first}")), None);
    commit(
        &mut script,
        "merged",
        Some(":".to_owned() + &long.to_string()),
        Some(format!(":{side}")),
    );
    script.push_str("done\n");
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(dir.as_str())
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("script");
    assert!(child.wait().expect("waits").success());
}

fn rev(dir: &Utf8Path, name: &str) -> String {
    String::from_utf8(git(dir, &["rev-parse", name]).stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// Every pair of `commits` is answered in `dir` as Git answers it.
fn agrees(dir: &Utf8Path, commits: &[String]) {
    let mut readers = Readers::new();
    for ancestor in commits {
        for descendant in commits {
            let git_says = git(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
                .status
                .success();
            assert_eq!(
                readers.is_ancestor(dir, ancestor, descendant),
                git_says,
                "is {ancestor} behind {descendant}?"
            );
        }
    }
}

/// A bare repository at `<temp>/<name>` holding [`history`]'s commits, and
/// the ones the tests ask about.
fn repository(temp: &Utf8Path, name: &str, long: usize) -> (camino::Utf8PathBuf, Vec<String>) {
    let dir = temp.join(name);
    assert!(
        git(temp, &["init", "-q", "--bare", dir.as_str()])
            .status
            .success()
    );
    history(&dir, long);
    let names = [
        "main",
        "main~1",
        "main~3",
        "main~4",
        &format!("main~{}", long - 1),
        "side",
        "side~1",
        "merged",
    ];
    let mut commits: Vec<String> = names.iter().map(|name| rev(&dir, name)).collect();
    // Not in the repository at all.
    commits.push("0123456789abcdef0123456789abcdef01234567".to_owned());
    (dir, commits)
}

/// A shallow repository ends its history at each commit its `shallow` file
/// lists, though their parents are still in it: `main~4` is not behind
/// `main` once `main~3` is a boundary.
#[test]
fn a_shallow_repository_is_answered_as_git_answers_it() {
    let temp = support::tempdir("readers-ancestry-shallow");
    let (dir, commits) = repository(&temp, "a.git", 12);
    std::fs::write(dir.join("shallow"), format!("{}\n", rev(&dir, "main~3"))).expect("shallow");
    assert!(
        !git(
            &dir,
            &["merge-base", "--is-ancestor", &commits[3], &commits[0]]
        )
        .status
        .success(),
        "Git reads the boundary"
    );
    agrees(&dir, &commits);
}

/// Grafts give a commit other parents than it stores: `main~3` none, so
/// what is behind it is not behind `main`, and `main~1` `side` as well, so
/// `side` is behind `main`.
#[test]
fn a_grafted_repository_is_answered_as_git_answers_it() {
    let temp = support::tempdir("readers-ancestry-grafts");
    let (dir, commits) = repository(&temp, "a.git", 12);
    let grafts = format!(
        "{}\n{} {} {}\n",
        rev(&dir, "main~3"),
        rev(&dir, "main~1"),
        rev(&dir, "main~2"),
        rev(&dir, "side"),
    );
    std::fs::create_dir_all(dir.join("info")).expect("info");
    std::fs::write(dir.join("info/grafts"), grafts).expect("grafts");
    for (ancestor, behind) in [(&commits[3], false), (&commits[5], true)] {
        assert_eq!(
            git(
                &dir,
                &["merge-base", "--is-ancestor", ancestor, &commits[0]]
            )
            .status
            .success(),
            behind,
            "Git reads the grafts"
        );
    }
    agrees(&dir, &commits);
}

#[test]
fn ancestry_in_process_is_gits() {
    let temp = support::tempdir("readers-ancestry");
    let long = WALK + 40;
    let (dir, commits) = repository(&temp, "a.git", long);
    agrees(&dir, &commits);
    let mut readers = Readers::new();
    assert_eq!(
        readers.head(&dir, "refs/heads/main").as_deref(),
        Some(commits[0].as_str())
    );
    assert_eq!(readers.head(&dir, "refs/heads/nothing"), None);
}
