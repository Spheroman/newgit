// A `git` the server merge runs is stopped when it runs too long, and
// answered 503 saying so (#411 review, item 10).

use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use newgit_serve::repos::git_timed;

mod support;

fn bare(name: &str) -> Utf8PathBuf {
    let dir = support::tempdir(name).join("r.git");
    let made = std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(dir.as_str())
        .status()
        .expect("git init");
    assert!(made.success());
    dir
}

#[test]
fn a_git_that_runs_too_long_is_stopped() {
    let dir = bare("git-timed-slow");
    let started = Instant::now();
    let error = git_timed(
        &dir,
        &["-c", "alias.slow=!sleep 30", "slow"],
        None,
        Duration::from_millis(300),
    )
    .expect_err("stopped");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it did not wait it out"
    );
    assert_eq!(error.status, 503);
    assert!(
        error.message.contains("ran longer than"),
        "{}",
        error.message
    );
}

#[test]
fn a_git_in_time_answers_with_its_output_and_input() {
    let dir = bare("git-timed-quick");
    let output = git_timed(
        &dir,
        &["hash-object", "--stdin"],
        Some(b"hello\n"),
        Duration::from_secs(30),
    )
    .expect("ran");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
}
