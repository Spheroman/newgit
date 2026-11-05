//! Which merge messages are nobody's words (docs/design/trackers.md, *One
//! commit, one audience*): Git's own one line, or newgit's, naming refs
//! that `git check-ref-format --branch` would take. A sentence dressed up as
//! a branch name is someone's words, and keeps to one audience.

use newgit_core::compose::stock_merge_message;

#[test]
fn words_dressed_up_as_branch_names_are_not_stock() {
    for written in [
        "Merge branch 'the refund bypass lives in src/pay.rs, see secrets.env'",
        "Merge branch 'x y z' into main",
        "Merge `fix the API key leak in .env` into main",
        "Merge remote-tracking branch 'origin/anything you like here'",
        "Merge branch 'a..b'",
        "Merge branch 'x' into main because",
        "Merge branch 'feature/.hidden'",
        "Merge branch 'x' into ma~in",
    ] {
        assert!(!stock_merge_message(written.as_bytes()), "{written}");
    }
}

#[test]
fn gits_and_newgits_own_merge_lines_are_stock() {
    for stock in [
        "Merge branch 'main'",
        "Merge branch 'feature/t3-ab12' into main\n",
        "Merge remote-tracking branch 'origin/main'",
        "Merge `feature-a` into main",
    ] {
        assert!(stock_merge_message(stock.as_bytes()), "{stock}");
    }
}

#[test]
fn what_counts_as_a_name_agrees_with_git() {
    for name in [
        "main",
        "feature/x",
        "a.b",
        "release-1.2",
        "x y",
        "a..b",
        "a~1",
        ".x",
        "x.lock",
        "-x",
        "x/",
    ] {
        let git = std::process::Command::new("git")
            .args(["check-ref-format", "--branch", name])
            .output()
            .expect("git runs")
            .status
            .success();
        let ours = stock_merge_message(format!("Merge branch '{name}'").as_bytes());
        assert_eq!(ours, git, "`{name}`");
    }
}
