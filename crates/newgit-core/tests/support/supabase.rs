//! Add the `supabase` template with every action stubbed to `true`.
//!
//! The template's `prepare` runs `supabase start`, which on any machine with
//! the CLI and Docker brings up a real stack per spawned instance. The
//! tempdir going away does not take the containers with it: they keep
//! running, under `EDIT_ME_PROJECT-<slug>`, until someone stops them by hand.
//! The tests that use this check what spawn renders and binds, which happens
//! before `prepare`, so they never need the stack.
//!
//! This parses the definition rather than matching the command's text. The
//! first version (#290) split on `command = "supabase start"`; when the
//! template's `prepare` became `supabase stop ...; supabase start`, the split
//! quietly found `[actions.resume]` instead and the real `prepare` ran again.
//! The check at the end makes the same drift fail the test instead.

use camino::Utf8Path;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

pub fn add_supabase_without_starting_it(repo: &Utf8Path) {
    let outcome = BranchManager::open(MetadataStore::at(repo.to_owned()))
        .expect("manager")
        .add_resource("supabase", "supabase")
        .expect("add supabase");

    let definition = std::fs::read_to_string(&outcome.path).expect("read definition");
    let mut table: toml::Table = definition.parse().expect("the template parses");
    let actions = table
        .get_mut("actions")
        .and_then(toml::Value::as_table_mut)
        .expect("the template has actions");
    for (_, action) in actions.iter_mut() {
        action
            .as_table_mut()
            .expect("an action is a table")
            .insert("command".to_owned(), toml::Value::from("true"));
    }
    std::fs::write(
        &outcome.path,
        toml::to_string(&table).expect("serialize definition"),
    )
    .expect("write definition");

    let definitions = MetadataStore::at(repo.to_owned())
        .load_resource_definitions()
        .expect("definitions");
    let supabase = definitions
        .iter()
        .find(|definition| definition.name == "supabase")
        .expect("supabase defined");
    for (name, action) in &supabase.actions {
        assert_eq!(
            action.command.as_deref(),
            Some("true"),
            "`{name}` would still run a real command"
        );
    }
}
