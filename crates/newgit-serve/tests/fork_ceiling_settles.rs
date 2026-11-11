// `cap_forks` runs until nothing changes, and fails closed (#415 review): a
// chain of any length is held all the way down, and a fork whose ceiling
// can't be settled (a cycle, an upstream that isn't defined, two upstreams no
// one audience fits under) is not defined. A fork is also held to the
// upstream its repo was made from (`recorded_from`) when its definition
// drops or changes `from`. Starting a fork checks its audience and that the
// caller may read the upstream, before anything is made.

use std::collections::BTreeMap;

use newgit_serve::names::parse_project_key;
use newgit_serve::policy::{Audience, Facts, Requester, cap_forks, may_start_fork, parse_facts};

fn fact(text: &str) -> Facts {
    parse_facts(text, "access").expect("parses")
}

fn fork_of(upstream: &str, audience: &str) -> Facts {
    fact(&format!(
        "audience = \"{audience}\"\nstorage = \"remote\"\nfrom = \"{upstream}\"\n"
    ))
}

#[test]
fn a_long_chain_sorted_before_its_upstreams_is_held_all_the_way_down() {
    // `fork-00` is a fork of `fork-01`, … of `fork-09`, of `zz-upstream`:
    // each sorts before its upstream, so one pass in name order moves the
    // ceiling down by one fork only.
    let mut facts = BTreeMap::new();
    facts.insert(
        "zz-upstream".to_owned(),
        fact("audience = \"user:alice\"\nstorage = \"remote\"\n"),
    );
    for n in 0..10 {
        let upstream = if n == 9 {
            "zz-upstream".to_owned()
        } else {
            format!("fork-{:02}", n + 1)
        };
        facts.insert(format!("fork-{n:02}"), fork_of(&upstream, "public"));
    }
    cap_forks(&mut facts);
    for n in 0..10 {
        let name = format!("fork-{n:02}");
        assert_eq!(
            facts[&name].audience,
            Audience::Named("alice".into()),
            "{name} is held to alice"
        );
    }
}

#[test]
fn forks_in_a_cycle_are_not_defined() {
    let mut facts = BTreeMap::new();
    facts.insert("a".to_owned(), fork_of("b", "public"));
    facts.insert("b".to_owned(), fork_of("a", "public"));
    facts.insert("c".to_owned(), fork_of("a", "public"));
    facts.insert("plain".to_owned(), fact("audience = \"public\"\n"));
    cap_forks(&mut facts);
    assert!(!facts.contains_key("a"));
    assert!(!facts.contains_key("b"));
    // A fork of one in a cycle has no upstream left.
    assert!(!facts.contains_key("c"));
    assert!(facts.contains_key("plain"));
}

#[test]
fn a_fork_of_an_undefined_upstream_is_not_defined_and_neither_are_its_forks() {
    let mut facts = BTreeMap::new();
    facts.insert("staging-db".to_owned(), fork_of("gone", "project-devs"));
    facts.insert("demo-db".to_owned(), fork_of("staging-db", "project-devs"));
    cap_forks(&mut facts);
    assert!(facts.is_empty(), "{:?}", facts.keys().collect::<Vec<_>>());
}

#[test]
fn dropping_from_leaves_the_fork_held_to_the_upstream_it_was_made_from() {
    let mut facts = BTreeMap::new();
    facts.insert(
        "local-db".to_owned(),
        fact("audience = \"project-devs\"\nstorage = \"remote\"\n"),
    );
    // Its definition no longer says `from`, and says `public`; its repo was
    // made from `local-db`'s.
    let mut fork = fact("audience = \"public\"\nstorage = \"remote\"\n");
    fork.recorded_from = Some("local-db".into());
    facts.insert("staging-db".to_owned(), fork);
    cap_forks(&mut facts);
    assert_eq!(facts["staging-db"].audience, Audience::ProjectDevs);
    assert!(
        facts["staging-db"]
            .capped
            .as_deref()
            .is_some_and(|why| why.contains("fork of `local-db`")),
        "{:?}",
        facts["staging-db"].capped
    );

    // Repointed at a public tracker: still held to the one it was made from.
    facts.insert("public-db".to_owned(), fact("audience = \"public\"\n"));
    let mut fork = fork_of("public-db", "public");
    fork.recorded_from = Some("local-db".into());
    facts.insert("staging-db".to_owned(), fork);
    cap_forks(&mut facts);
    assert_eq!(facts["staging-db"].audience, Audience::ProjectDevs);

    // Its recorded upstream deleted: not defined.
    facts.remove("local-db");
    let mut fork = fact("audience = \"public\"\n");
    fork.recorded_from = Some("local-db".into());
    facts.insert("staging-db".to_owned(), fork);
    cap_forks(&mut facts);
    assert!(!facts.contains_key("staging-db"));
}

#[test]
fn a_fork_held_to_two_unrelated_upstreams_is_not_defined() {
    let mut facts = BTreeMap::new();
    facts.insert("alice-db".to_owned(), fact("audience = \"user:alice\"\n"));
    facts.insert("bob-db".to_owned(), fact("audience = \"user:bob\"\n"));
    let mut fork = fork_of("alice-db", "user:alice");
    fork.recorded_from = Some("bob-db".into());
    facts.insert("both".to_owned(), fork);
    cap_forks(&mut facts);
    assert!(!facts.contains_key("both"));
    assert!(facts.contains_key("alice-db") && facts.contains_key("bob-db"));
}

#[test]
fn starting_a_fork_needs_a_reader_of_its_upstream_and_a_fork_within_it() {
    let project = parse_project_key("jack/store").expect("key");
    let member = Requester {
        login: "bob".into(),
        member: true,
    };
    let stranger = Requester {
        login: "eve".into(),
        member: false,
    };
    let upstream = fact("audience = \"project-devs\"\nstorage = \"remote\"\n");
    let fork = fork_of("local-db", "user:bob");
    assert!(may_start_fork(&project, "local-db", &fork, &upstream, &member).is_ok());
    let why = may_start_fork(&project, "local-db", &fork, &upstream, &stranger)
        .expect_err("eve can't read local-db");
    assert!(why.contains("starting it reads `local-db`"), "{why}");

    // Wider than its upstream: refused whoever asks.
    let wide = fork_of("local-db", "public");
    let why = may_start_fork(&project, "local-db", &wide, &upstream, &member)
        .expect_err("wider than local-db");
    assert!(
        why.contains("wider than `local-db`'s `project-devs`"),
        "{why}"
    );

    // One user's upstream: only that user may start a fork from it.
    let alice = fact("audience = \"user:alice\"\n");
    let fork = fork_of("alice-db", "user:alice");
    assert!(may_start_fork(&project, "alice-db", &fork, &alice, &member).is_err());
}
