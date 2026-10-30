//! Helpers shared by more than one test file. Cargo does not build
//! `tests/common/mod.rs` as a test target of its own; a file that wants these
//! says `mod common;`.

use camino::Utf8Path;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

/// Add the `supabase` resource from its template with every hook command
/// replaced by `true`.
///
/// `spawn` runs `prepare`, which in the template brings up a real Supabase
/// stack on any machine with the CLI and Docker, and dropping the tempdir
/// does not take the containers with it. Every caller checks what spawn
/// renders and binds, which happens before `prepare`, so no hook needs to do
/// anything.
///
/// This walks the parsed definition rather than matching the template's
/// text: #290 matched `command = "supabase start"`, #264 then changed
/// `prepare` to stop first, and the match quietly stopped protecting anyone.
#[allow(dead_code)]
pub fn add_supabase_without_starting_it(repo: &Utf8Path) {
    let outcome = BranchManager::open(MetadataStore::at(repo.to_owned()))
        .expect("manager")
        .add_resource("supabase", "supabase")
        .expect("add supabase");

    let definition = std::fs::read_to_string(&outcome.path).expect("read definition");
    let mut table: toml::Table = definition.parse().expect("the definition parses");
    let stubbed = stub_commands(&mut table);
    assert!(
        table["actions"]["prepare"]["command"].as_str() == Some("true"),
        "the supabase template no longer has an `[actions.prepare]` command to stub"
    );
    assert!(stubbed > 0);
    std::fs::write(
        &outcome.path,
        toml::to_string(&table).expect("serialize definition"),
    )
    .expect("write definition");
}

/// Replace every `command` string anywhere in `table` with `"true"`; returns
/// how many it replaced.
fn stub_commands(table: &mut toml::Table) -> usize {
    let mut stubbed = 0;
    for (key, value) in table.iter_mut() {
        match value {
            toml::Value::String(_) if key == "command" => {
                *value = toml::Value::String("true".to_owned());
                stubbed += 1;
            }
            toml::Value::Table(inner) => stubbed += stub_commands(inner),
            toml::Value::Array(items) => {
                for item in items {
                    if let toml::Value::Table(inner) = item {
                        stubbed += stub_commands(inner);
                    }
                }
            }
            _ => {}
        }
    }
    stubbed
}
