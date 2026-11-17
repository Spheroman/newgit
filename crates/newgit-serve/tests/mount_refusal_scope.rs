// A landing is refused for a mount the client refuses only when it changes
// that mount (#458's review, item 9): a definition already on `main` with a
// refused mount is not held against a landing that leaves the mount as it
// is, whatever else about it changes (a fork's `capped`, its paths), while
// one that makes such a mount, or moves one, is refused.

use std::collections::BTreeMap;

use newgit_serve::policy::{Facts, mount_refusal, parse_facts};

fn facts(mount: &str, paths: &str) -> Facts {
    parse_facts(
        &format!("audience = \"public\"\ngit = true\nmount = \"{mount}\"\npaths = [{paths}]\n"),
        "root",
    )
    .expect("parses")
}

fn defined(facts: Facts) -> BTreeMap<String, Facts> {
    [("blog".to_owned(), facts)].into()
}

#[test]
fn only_a_changed_mount_is_refused() {
    let refused = facts("/blog", "\".\"");
    assert!(refused.mount_refused.is_some());
    let before = defined(refused.clone());

    // Untouched, or touched elsewhere: not refused.
    assert_eq!(mount_refusal(&before, &before), None);
    let capped = Facts {
        capped: Some("wider than its upstream".to_owned()),
        ..refused.clone()
    };
    assert_eq!(mount_refusal(&before, &defined(capped)), None);
    assert_eq!(
        mount_refusal(&before, &defined(facts("/blog", "\"docs\""))),
        None
    );

    // Made, or moved to another refused mount: refused.
    let made = mount_refusal(&BTreeMap::new(), &before).expect("made");
    assert!(made.contains("blog"), "{made}");
    assert!(mount_refusal(&before, &defined(facts("../blog", "\".\""))).is_some());
    assert!(
        mount_refusal(&defined(facts("blog", "\".\"")), &before).is_some(),
        "an accepted mount made refused"
    );
    // Moved to one the client accepts: nothing to refuse.
    assert_eq!(
        mount_refusal(&before, &defined(facts("blog", "\".\""))),
        None
    );
}
