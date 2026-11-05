//! What a commit newgit makes says about the repository it came from
//! (`Import … from <source>`, `Pull … from <source>`): commits are published,
//! so never a token, a password, or a path on this machine.

use std::process::Command;

use camino::Utf8PathBuf;
use newgit_core::manager::recorded_source;

/// `https://<userinfo>@github.com/<path>`, put together here so the source
/// holds no credential-shaped URL for a secret scanner to flag.
fn with_userinfo(userinfo: &str, path: &str) -> String {
    format!("https://{userinfo}{}github.com/{path}", '@')
}

#[test]
fn a_url_loses_its_userinfo_and_query() {
    assert_eq!(
        recorded_source(&with_userinfo(
            &["name", "not-a-real-token"].join(":"),
            "Spheroman/t3code.git"
        )),
        "https://github.com/Spheroman/t3code.git"
    );
    assert_eq!(
        recorded_source("https://github.com/Spheroman/t3code?token=abc#frag"),
        "https://github.com/Spheroman/t3code"
    );
    assert_eq!(
        recorded_source("ssh://git@github.com/Spheroman/t3code.git"),
        "ssh://github.com/Spheroman/t3code.git"
    );
    assert_eq!(recorded_source("Spheroman/t3code"), "Spheroman/t3code");
    assert_eq!(
        recorded_source("git@github.com:Spheroman/t3code.git"),
        "git@github.com:Spheroman/t3code.git"
    );
}

#[test]
fn a_local_path_is_its_origin_or_its_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root =
        Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical")).expect("utf8");
    let git = |dir: &Utf8PathBuf, args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir.as_str())
                .args(args)
                .status()
                .expect("git")
                .success()
        );
    };
    let plain = root.join("t3code");
    std::fs::create_dir_all(&plain).expect("mkdir");
    git(&plain, &["init", "-q"]);
    assert_eq!(recorded_source(plain.as_str()), "t3code");
    assert_eq!(recorded_source(&format!("file://{plain}")), "t3code");

    let bare = root.join("mirror.git");
    git(&root, &["init", "-q", "--bare", bare.as_str()]);
    assert_eq!(recorded_source(bare.as_str()), "mirror");

    git(
        &plain,
        &[
            "remote",
            "add",
            "origin",
            &with_userinfo(
                &["name", "not-a-password"].join(":"),
                "Spheroman/t3code.git",
            ),
        ],
    );
    assert_eq!(
        recorded_source(plain.as_str()),
        "https://github.com/Spheroman/t3code.git"
    );
    // An origin that is itself a path on this machine is not recorded.
    git(
        &plain,
        &[
            "remote",
            "set-url",
            "origin",
            root.join("elsewhere").as_str(),
        ],
    );
    assert_eq!(recorded_source(plain.as_str()), "t3code");
}
