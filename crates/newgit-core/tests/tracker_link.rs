//! A tracker linked from another project (`link = "<owner>/<project>[/<path>]"`):
//! its definition, and the repo it names. New tests live in their own file
//! per AGENTS.md's "Working in parallel".

use camino::Utf8PathBuf;
use newgit_core::manager::home_repo;
use newgit_core::tracker::TrackerDefinition;

fn definition(text: &str) -> newgit_core::Result<TrackerDefinition> {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("env.toml")).expect("a UTF-8 path");
    let whole = format!("storage = \"local\"\nmerge_with_source = true\npaths = [\".\"]\n{text}");
    std::fs::write(&path, whole).expect("the definition written");
    TrackerDefinition::from_file("env", &path)
}

#[test]
fn a_link_names_its_home() {
    let linked = definition(
        "audience = \"project-devs\"\ngit = true\nmount = \".env\"\nlink = \"Jack/env/dots/zsh\"\n",
    )
    .expect("a link parses");
    assert_eq!(linked.link.as_deref(), Some("Jack/env/dots/zsh"));
    assert_eq!(home_repo("Jack/env/dots/zsh"), "jack.env.t.dots__zsh");
    assert_eq!(home_repo("jack/env"), "jack.env.t.root");
}

#[test]
fn a_link_is_a_tracker_in_git_with_a_whole_home() {
    for text in [
        "audience = \"project-devs\"\nmount = \".env\"\nlink = \"jack/env\"\n",
        "audience = \"project-devs\"\ngit = true\nmount = \".env\"\nlink = \"jack\"\n",
        "audience = \"project-devs\"\ngit = true\nmount = \".env\"\nlink = \"jack//env\"\n",
    ] {
        assert!(definition(text).is_err(), "{text}");
    }
}
