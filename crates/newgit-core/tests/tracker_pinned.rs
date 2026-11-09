//! A definition's `pinned`: whether its parent holds it where it is
//! (docs/design/anchoring.md, *Follow and pinned*). False unless it says,
//! and written only when true.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use camino::Utf8Path;
use newgit_core::tracker::TrackerDefinition;

const BASE: &str = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\nmount = \"env/prod\"\n";

fn parse(text: &str) -> newgit_core::error::Result<TrackerDefinition> {
    TrackerDefinition::from_text("prod", Utf8Path::new("prod.toml"), text)
}

#[test]
fn pinned_is_false_unless_it_says_and_is_written_only_when_true() {
    assert!(!parse(BASE).expect("parses").pinned);
    let pinned = parse(&format!("{BASE}pinned = true\n")).expect("parses");
    assert!(pinned.pinned);
    assert!(
        toml::to_string(&pinned.to_file())
            .expect("writes")
            .contains("pinned = true")
    );
    assert!(
        !toml::to_string(&parse(BASE).expect("parses").to_file())
            .expect("writes")
            .contains("pinned")
    );
}
