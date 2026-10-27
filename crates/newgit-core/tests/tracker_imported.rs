//! An imported tracker's definition (newgit-trackers.md, *An imported
//! tracker joins through one merge*): `imported = { anchor, tip }` is read,
//! written back, and only valid on a code tracker naming full commit ids.

use camino::Utf8PathBuf;
use newgit_core::tracker::{Imported, TrackerDefinition};

#[test]
fn an_imported_anchor_is_read_and_must_name_full_commits_on_a_code_tracker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().join("t3code.toml")).expect("utf8 path");
    let anchor = "a".repeat(40);
    let tip = "b".repeat(40);
    let write = |secret: &str, anchor: &str| {
        std::fs::write(
            &path,
            format!(
                "audience = \"project-devs\"\nstorage = \"local\"\npaths = [\"apps/t3code\"]\n\
                 {secret}imported = {{ anchor = \"{anchor}\", tip = \"{tip}\" }}\n"
            ),
        )
        .expect("write definition");
    };

    write("secret = false\n", &anchor);
    let definition = TrackerDefinition::from_file("t3code", &path).expect("valid");
    assert_eq!(
        definition.imported,
        Some(Imported {
            anchor: anchor.clone(),
            tip: tip.clone()
        })
    );
    let written = toml::to_string(&definition.to_file()).expect("serialize");
    assert!(
        written.contains(&format!("anchor = \"{anchor}\"")),
        "{written}"
    );

    write("secret = false\n", "main");
    let error = TrackerDefinition::from_file("t3code", &path).expect_err("not an id");
    assert!(error.to_string().contains("full commit id"), "{error}");

    write("", &anchor);
    let error = TrackerDefinition::from_file("t3code", &path).expect_err("secret");
    assert!(error.to_string().contains("code tracker"), "{error}");
}
