//! A definition's `history`: how its parent records it rolling up on the
//! remote (docs/design/anchoring.md). `commits` unless it says `pins`, and
//! written only when `pins`.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use camino::Utf8Path;
use newgit_core::tracker::{History, TrackerDefinition};

const BASE: &str = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\nmount = \"users\"\n";

fn parse(text: &str) -> newgit_core::error::Result<TrackerDefinition> {
    TrackerDefinition::from_text("users", Utf8Path::new("users.toml"), text)
}

#[test]
fn history_is_commits_unless_it_says_pins() {
    assert_eq!(parse(BASE).expect("parses").history, History::Commits);
    let pins = parse(&format!("{BASE}history = \"pins\"\n")).expect("parses");
    assert_eq!(pins.history, History::Pins);
    let written = toml::to_string(&pins.to_file()).expect("writes");
    assert!(written.contains("history = \"pins\""), "{written}");
    let default = toml::to_string(&parse(BASE).expect("parses").to_file()).expect("writes");
    assert!(
        !default.contains("history"),
        "the default is not written: {default}"
    );
}

#[test]
fn any_other_history_is_refused() {
    assert!(parse(&format!("{BASE}history = \"snapshots\"\n")).is_err());
}
