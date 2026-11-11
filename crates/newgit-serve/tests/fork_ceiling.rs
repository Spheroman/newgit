// A fork (`from`) is never readable by anyone its upstream is not (#415). The
// client refuses a definition that widens one, but the definitions the server
// reads are commits whoever governs them made, so the server holds every fork
// to its upstream's audience as it reads them: every rule and every store then
// sees it no wider than the history it holds.

use std::collections::BTreeMap;

use newgit_serve::policy::{Audience, Facts, audience_within, cap_forks, parse_facts};

fn facts(entries: &[(&str, &str)]) -> BTreeMap<String, Facts> {
    entries
        .iter()
        .map(|(name, text)| {
            (
                (*name).to_owned(),
                parse_facts(text, "access").expect("parses"),
            )
        })
        .collect()
}

#[test]
fn audiences_nest_public_then_members_then_one_user() {
    let named = Audience::Named("jack".into());
    assert!(audience_within(&Audience::ProjectDevs, &Audience::Public));
    assert!(audience_within(&named, &Audience::ProjectDevs));
    assert!(audience_within(&Audience::User, &Audience::ProjectDevs));
    assert!(audience_within(&named, &named));
    assert!(!audience_within(&Audience::Public, &Audience::ProjectDevs));
    assert!(!audience_within(&Audience::ProjectDevs, &named));
    assert!(!audience_within(&Audience::Named("eve".into()), &named));
    assert!(!audience_within(&named, &Audience::User));
}

#[test]
fn a_fork_wider_than_its_upstream_is_read_with_its_upstreams_audience() {
    let mut read = facts(&[
        (
            "local-db",
            "audience = \"user:jack\"\nstorage = \"remote\"\n",
        ),
        (
            "staging-db",
            "audience = \"public\"\nstorage = \"remote\"\nfrom = \"local-db\"\n",
        ),
        // A fork of the fork, held in turn.
        (
            "demo-db",
            "audience = \"project-devs\"\nstorage = \"remote\"\nfrom = \"staging-db\"\n",
        ),
        (
            "mine",
            "audience = \"user:jack\"\nstorage = \"remote\"\nfrom = \"local-db\"\n",
        ),
    ]);
    cap_forks(&mut read);
    let jack = Audience::Named("jack".into());
    assert_eq!(read["staging-db"].audience, jack);
    assert!(
        read["staging-db"]
            .capped
            .as_deref()
            .is_some_and(|why| why.contains("public") && why.contains("user:jack")),
        "{:?}",
        read["staging-db"].capped
    );
    assert_eq!(read["demo-db"].audience, jack);
    assert_eq!(read["mine"].audience, jack);
    assert!(read["mine"].capped.is_none());
}

#[test]
fn a_fork_whose_upstream_is_not_defined_is_not_defined_either() {
    let mut read = facts(&[(
        "staging-db",
        "audience = \"public\"\nstorage = \"remote\"\nfrom = \"gone\"\n",
    )]);
    cap_forks(&mut read);
    assert!(!read.contains_key("staging-db"));
}
