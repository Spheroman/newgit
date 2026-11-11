// `cap_forks` only ever narrows (#433 review). Against each upstream a fork
// is left alone when it is within the upstream, narrowed to the upstream
// when the upstream is within it, and treated as undefined when the two are
// not ordered: moving `user:jack`'s fork to `user:bob` would hand jack's own
// commits to bob.

use std::collections::BTreeMap;

use newgit_serve::policy::{Audience, Facts, cap_forks, parse_facts};

fn fact(text: &str) -> Facts {
    parse_facts(text, "access").expect("parses")
}

fn fork_of(upstream: &str, audience: &str) -> Facts {
    fact(&format!(
        "audience = \"{audience}\"\nstorage = \"remote\"\nfrom = \"{upstream}\"\n"
    ))
}

fn upstream(audience: &str) -> Facts {
    fact(&format!(
        "audience = \"{audience}\"\nstorage = \"remote\"\n"
    ))
}

#[test]
fn a_fork_for_another_user_is_not_defined_rather_than_moved() {
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    facts.insert("jack-db".to_owned(), fork_of("local-db", "user:jack"));
    cap_forks(&mut facts);
    assert!(!facts.contains_key("jack-db"), "{:?}", facts.get("jack-db"));
    assert!(facts.contains_key("local-db"));
}

#[test]
fn a_per_user_fork_of_one_users_tracker_is_not_defined() {
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    facts.insert("mine".to_owned(), fork_of("local-db", "user"));
    cap_forks(&mut facts);
    assert!(!facts.contains_key("mine"));

    // And the other way round.
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user"));
    facts.insert("bobs".to_owned(), fork_of("local-db", "user:bob"));
    cap_forks(&mut facts);
    assert!(!facts.contains_key("bobs"));
}

#[test]
fn an_upstream_within_the_fork_still_narrows_it() {
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    facts.insert("wide".to_owned(), fork_of("local-db", "project-devs"));
    facts.insert("narrow".to_owned(), fork_of("local-db", "user:bob"));
    cap_forks(&mut facts);
    assert_eq!(facts["wide"].audience, Audience::Named("bob".into()));
    assert!(facts["wide"].capped.is_some());
    assert_eq!(facts["narrow"].audience, Audience::Named("bob".into()));
    assert!(facts["narrow"].capped.is_none());
}

#[test]
fn down_a_chain_an_unordered_link_leaves_everything_below_it_undefined() {
    // `local-db` narrowed to bob; `jack-db` is jack's fork of it, and
    // `jack-scratch` a fork of `jack-db` that says `project-devs`.
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    facts.insert("jack-db".to_owned(), fork_of("local-db", "user:jack"));
    facts.insert(
        "jack-scratch".to_owned(),
        fork_of("jack-db", "project-devs"),
    );
    cap_forks(&mut facts);
    assert!(!facts.contains_key("jack-db"));
    assert!(!facts.contains_key("jack-scratch"));

    // A chain whose middle narrows into a user that the bottom isn't.
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    facts.insert("mid".to_owned(), fork_of("local-db", "project-devs"));
    facts.insert("jack-db".to_owned(), fork_of("mid", "user:jack"));
    cap_forks(&mut facts);
    assert_eq!(facts["mid"].audience, Audience::Named("bob".into()));
    assert!(!facts.contains_key("jack-db"));
}

#[test]
fn an_unordered_recorded_upstream_leaves_the_fork_undefined() {
    // Its definition dropped `from`; its repo was made from `local-db`,
    // since narrowed to bob.
    let mut facts = BTreeMap::new();
    facts.insert("local-db".to_owned(), upstream("user:bob"));
    let mut fork = upstream("user:jack");
    fork.recorded_from = Some("local-db".into());
    facts.insert("jack-db".to_owned(), fork);
    cap_forks(&mut facts);
    assert!(!facts.contains_key("jack-db"));

    // `from` repointed at a public tracker changes nothing.
    facts.insert("public-db".to_owned(), upstream("public"));
    let mut fork = fork_of("public-db", "user:jack");
    fork.recorded_from = Some("local-db".into());
    facts.insert("jack-db".to_owned(), fork);
    cap_forks(&mut facts);
    assert!(!facts.contains_key("jack-db"));
    assert!(facts.contains_key("public-db"));
}
