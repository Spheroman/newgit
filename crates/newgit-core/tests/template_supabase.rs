//! The `supabase` template's value is entirely in whether its `[[render]]`
//! `find` strings match a real `supabase/config.toml` exactly once each
//! (#90). A `find` that matches zero times refuses at spawn — loud, fine — but
//! a template whose strings never match is a template nobody can use, and
//! that is not something reading the TOML tells you. So this renders it
//! against the CLI's committed defaults and checks the result.
//!
//! Its checkpoint and restore are the other half (#241): they have to round
//! trip under `checkpoint --verify`, and they run here against stand-ins for
//! the CLI and Docker rather than a real stack.

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::manager::BranchManager;
use newgit_core::store::MetadataStore;

mod common;
use common::{add_supabase_with_snapshot_commands, add_supabase_without_starting_it};

/// `supabase init` output from CLI 2.111.0, verbatim apart from `project_id`,
/// which the CLI sets to the directory name and the template's `find` expects
/// as `EDIT_ME_PROJECT`.
const CONFIG_TOML: &str = include_str!("fixtures/supabase-init-config.toml");

/// What the template's `[checkpoint]` and `[restore]` run against below. The
/// scripts say what they model, and what they do not.
const FAKE_SUPABASE: &str = include_str!("fixtures/fake-supabase-cli/supabase");
const FAKE_DOCKER: &str = include_str!("fixtures/fake-supabase-cli/docker");

/// A migration that makes a bucket, and a seed that makes a user: the rows
/// `supabase db reset` puts back before a restore loads the dump (#241).
const AVATARS_MIGRATION: &str = include_str!("fixtures/supabase-avatars-migration.sql");
const SEED: &str = include_str!("fixtures/supabase-seed.sql");

fn git(dir: &Utf8Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (temp, path)
}

/// A store repo with a committed `supabase/config.toml`, which is what a
/// `[[render]]` requires: it substitutes into committed content.
fn setup(temp: &Utf8Path) -> MetadataStore {
    setup_with(temp, &[])
}

/// [`setup`], with more files committed beside `supabase/config.toml`.
fn setup_with(temp: &Utf8Path, files: &[(&str, &str)]) -> MetadataStore {
    let repo = temp.join("store");
    std::fs::create_dir_all(repo.join("supabase")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("supabase/config.toml"), CONFIG_TOML).expect("write config");
    for (path, contents) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write file");
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
    let mut config = store.load_config().expect("config");
    config.workspace = Some(newgit_core::config::WorkspaceSection {
        root: Some(temp.join("workspaces")),
        materializer: None,
    });
    store.write_config(&config).expect("write config");
    store
}

/// `(section, key) -> port`, so an assertion can name the line it means
/// rather than substring-matching a number that appears in several places.
fn parse_ports(config: &str) -> std::collections::BTreeMap<(String, String), u16> {
    let mut ports = std::collections::BTreeMap::new();
    let mut section = String::new();
    for line in config.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        if key.ends_with("port")
            && let Ok(port) = value.parse::<u16>()
        {
            ports.insert((section.clone(), key.to_owned()), port);
        }
    }
    ports
}

#[test]
fn the_supabase_template_renders_against_the_clis_committed_defaults() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    add_supabase_without_starting_it(&repo);

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    let binding = &spawned.branch.resources["supabase"];

    // Every `find` matched exactly once. A miss refuses the render and is
    // reported here rather than thrown, so assert on the error, not a panic.
    let resource = spawned
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("supabase bound");
    assert_eq!(
        resource.render_error, None,
        "the template's find strings must match `supabase init` output"
    );
    assert_eq!(resource.export_error, None);

    let rendered =
        std::fs::read_to_string(spawned.branch.workspace_path.join("supabase/config.toml"))
            .expect("read rendered config");

    // project_id is what scopes every container and volume. Without a
    // per-instance value the second `supabase start` adopts the first
    // instance's stack, which is the failure this template exists to prevent.
    assert!(
        rendered.contains(r#"project_id = "EDIT_ME_PROJECT-feature-a""#),
        "project_id must carry the instance slug, got:\n{rendered}"
    );

    // Every port moved to its allocated value. The allocated numbers
    // depend on what is listening on the machine, so assert against the
    // binding record rather than against literals — a suite running in
    // parallel legitimately pushes these around.
    let rendered_ports = parse_ports(&rendered);
    for (section, key, port_name) in [
        ("api", "port", "api"),
        ("db", "port", "db"),
        ("db", "shadow_port", "shadow"),
        ("studio", "port", "studio"),
        ("local_smtp", "port", "inbucket"),
        ("analytics", "port", "analytics"),
        ("db.pooler", "port", "pooler"),
    ] {
        assert_eq!(
            rendered_ports
                .get(&(section.to_owned(), key.to_owned()))
                .copied(),
            Some(binding.resolved_ports[port_name]),
            "[{section}] {key} did not render to the allocated `{port_name}`:\n{rendered}"
        );
    }

    // `supabase init` leaves these two commented out; they move anyway, so
    // uncommenting one does not reintroduce the default.
    for (key, port_name) in [("smtp_port", "smtp"), ("pop3_port", "pop3")] {
        let line = format!("# {key} = {}", binding.resolved_ports[port_name]);
        assert!(
            rendered.contains(&line),
            "`{line}` missing from the render:\n{rendered}"
        );
    }

    // `shadow_port = 54320` must not have been eaten by the `port = 54320`
    // that is its own substring.
    assert_ne!(
        rendered_ports[&("db".to_owned(), "shadow_port".to_owned())],
        rendered_ports[&("db".to_owned(), "port".to_owned())],
        "shadow_port and port collided"
    );

    // The exports the restore hook depends on resolved to real values.
    let exports = &binding.resolved_exports;
    assert_eq!(
        exports["SUPABASE_DB_CONTAINER"],
        "supabase_db_EDIT_ME_PROJECT-feature-a"
    );
    assert_eq!(
        exports["SUPABASE_API_URL"],
        format!("http://127.0.0.1:{}", binding.resolved_ports["api"])
    );
}

/// The smaller half of #90: `command-snapshot-migrations` named Supabase as
/// its audience and handed out `createdb`/`pg_dump`/`psql`, which a Supabase
/// user does not have against their database. "Ready to adapt" and "ready to
/// adapt *if your database is on the host*" are different claims, so every
/// template now states which host tools it assumes — and the spec says every,
/// so this checks every.
#[test]
fn every_template_states_the_host_tools_it_assumes() {
    for template in newgit_core::templates::RESOURCE_TEMPLATES {
        assert!(
            template.contents.contains("HOST TOOLS:"),
            "`{}` does not say what it assumes is installed",
            template.name
        );
    }
}

/// Two instances must not land on one stack. The ports are what newgit
/// allocates and the `project_id` is what scopes the containers; both have to
/// differ, or `supabase start` adopts the neighbour's stack.
#[test]
fn two_instances_get_different_ports_and_different_project_ids() {
    let (_guard, temp) = tempdir();
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();

    add_supabase_without_starting_it(&repo);

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let first = manager.spawn("feature-a", None).expect("spawn a");
    let second = manager.spawn("feature-b", None).expect("spawn b");

    let a = &first.branch.resources["supabase"];
    let b = &second.branch.resources["supabase"];

    assert_ne!(
        a.resolved_exports["SUPABASE_PROJECT"],
        b.resolved_exports["SUPABASE_PROJECT"]
    );
    for port_name in [
        "api",
        "db",
        "shadow",
        "studio",
        "inbucket",
        "smtp",
        "pop3",
        "analytics",
        "pooler",
    ] {
        assert_ne!(
            a.resolved_ports[port_name], b.resolved_ports[port_name],
            "`{port_name}` was handed to both instances"
        );
    }
}

/// The stand-in `supabase` and `docker`, and where they keep each stack's
/// tables: one file per table, one row per line.
struct FakeCli {
    bin: Utf8PathBuf,
    db: Utf8PathBuf,
    /// How the fake dump writes pg_dump's `\restrict` pair: `commented`, as
    /// the CLI does, or `bare`, as a plain pg_dump does.
    restrict: &'static str,
}

impl FakeCli {
    fn install(temp: &Utf8Path, restrict: &'static str) -> Self {
        let bin = temp.join("fake-bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        for (name, script) in [("supabase", FAKE_SUPABASE), ("docker", FAKE_DOCKER)] {
            let path = bin.join(name);
            std::fs::write(&path, script).expect("write the stand-in");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make the stand-in executable");
        }
        Self {
            bin,
            db: temp.join("fake-db"),
            restrict,
        }
    }

    /// Shell that makes the commands after it find the stand-ins.
    fn prelude(&self) -> String {
        format!(
            "PATH='{}':\"$PATH\" FAKE_SUPABASE_DB='{}' FAKE_SUPABASE_RESTRICT={}\n\
             export PATH FAKE_SUPABASE_DB FAKE_SUPABASE_RESTRICT\n",
            self.bin, self.db, self.restrict
        )
    }

    /// `supabase db reset` in `workspace`: what `supabase start` leaves, the
    /// migrations and the seed applied.
    fn reset(&self, workspace: &Utf8Path) {
        let output = Command::new(self.bin.join("supabase"))
            .args(["db", "reset"])
            .current_dir(workspace)
            .env("FAKE_SUPABASE_DB", &self.db)
            .output()
            .expect("the stand-in runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A table of `feature-a`'s stack.
    fn table(&self, table: &str) -> Utf8PathBuf {
        self.db
            .join("supabase_db_EDIT_ME_PROJECT-feature-a")
            .join(table)
    }

    fn rows(&self, table: &str) -> Vec<String> {
        std::fs::read_to_string(self.table(table))
            .expect("read table")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A row the app wrote at runtime.
    fn insert(&self, table: &str, row: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.table(table))
            .expect("the table exists");
        writeln!(file, "{row}").expect("append row");
    }
}

/// `feature-a` of a project with a migration that makes a bucket and a seed
/// that makes a user, spawned with the template's own checkpoint and restore
/// running against `fake`, and its stack up as `supabase start` leaves it.
fn spawn_with_fake_cli(temp: &Utf8Path, fake: &FakeCli) -> BranchManager {
    let store = setup_with(
        temp,
        &[
            (
                "supabase/migrations/20240101000000_avatars.sql",
                AVATARS_MIGRATION,
            ),
            ("supabase/seed.sql", SEED),
        ],
    );
    let repo = store.paths().project_root.clone();
    add_supabase_with_snapshot_commands(&repo, &fake.prelude());

    let manager = BranchManager::open(MetadataStore::at(repo)).expect("reopen");
    let spawned = manager.spawn("feature-a", None).expect("spawn");
    fake.reset(&spawned.branch.workspace_path);
    manager
}

/// The supabase resource's state ref, and the dump it deposited.
fn checkpoint_dump(manager: &BranchManager) -> (String, String) {
    let outcome = manager.checkpoint("feature-a", None).expect("checkpoint");
    let state = outcome
        .record
        .resource_states
        .iter()
        .find(|state| state.name == "supabase")
        .expect("supabase checkpointed");
    let dump = std::fs::read_to_string(state.state_path.as_ref().expect("a deposit"))
        .expect("read the dump");
    (state.state_ref.clone().expect("a state ref"), dump)
}

/// Two checkpoints of one unchanged database must be the same bytes, or
/// `checkpoint --verify` can never pass. pg_dump puts a fresh random key in
/// its `\restrict` / `\unrestrict` pair on every run, and the checkpoint
/// swaps it for `newgit` — on both lines, in whichever form the dump writes
/// them.
fn assert_the_restrict_key_is_fixed(restrict: &'static str, line_start: &str) {
    let (_guard, temp) = tempdir();
    let fake = FakeCli::install(&temp, restrict);
    let manager = spawn_with_fake_cli(&temp, &fake);

    let (first_ref, first) = checkpoint_dump(&manager);
    let (second_ref, second) = checkpoint_dump(&manager);

    let pair: Vec<&str> = first
        .lines()
        .filter(|line| line.contains("restrict "))
        .collect();
    assert_eq!(
        pair,
        [
            format!("{line_start}\\restrict newgit"),
            format!("{line_start}\\unrestrict newgit"),
        ],
        "the key was not swapped:\n{first}"
    );
    assert_eq!(first, second);
    assert_eq!(first_ref, second_ref);
}

/// #241: `supabase db dump` comments the pair out (`-- \restrict <key>`) so
/// that a dump carries no psql meta-commands, and the swap only matched a
/// line that starts with the backslash. So the key stayed random, and every
/// verify failed.
#[test]
fn the_checkpoint_fixes_the_restrict_key_the_cli_writes_as_a_comment() {
    assert_the_restrict_key_is_fixed("commented", "-- ");
}

/// The form a plain pg_dump writes, which the swap matched before #241.
#[test]
fn the_checkpoint_still_fixes_a_bare_restrict_key() {
    assert_the_restrict_key_is_fixed("bare", "");
}

/// #241: `[restore]` resets the database, which replays the migrations and
/// the seed, and then emptied only `public` before loading the dump. A
/// migration's bucket and a seed's user were then in `storage.buckets` and
/// `auth.users` twice over: the dump's INSERT for each of those tables died
/// on the duplicate key and took every row made at runtime with it, and the
/// reset's own copies differ from the checkpoint's anyway.
#[test]
fn a_seeded_bucket_and_user_survive_the_verify_round_trip() {
    let (_guard, temp) = tempdir();
    let fake = FakeCli::install(&temp, "commented");
    let manager = spawn_with_fake_cli(&temp, &fake);
    // What the app did since: a sign-up with a profile, a bucket of its own,
    // an upload.
    let user = "('00000000-0000-0000-0000-000000000002', 'signed-up@example.com')";
    fake.insert("auth.users", user);
    fake.insert(
        "public.profiles",
        "('00000000-0000-0000-0000-000000000002', 'signed-up')",
    );
    fake.insert("storage.buckets", "('docs', 'docs', false)");
    fake.insert("storage.objects", "('object-1', 'avatars', 'me.png')");

    let outcome = manager
        .checkpoint_verify("feature-a", None)
        .expect("verify runs");
    let resource = outcome
        .resources
        .iter()
        .find(|resource| resource.name == "supabase")
        .expect("supabase verified");
    assert!(resource.exercised);
    assert!(
        outcome.is_proven(),
        "the restore did not land on the checkpoint: {:?}\n{:#?}",
        resource.cause,
        resource.diff
    );

    // The checkpoint's rows came back, not the reset's...
    assert_eq!(
        fake.rows("storage.buckets"),
        [
            "('avatars', 'avatars', true, 'reset 1')",
            "('docs', 'docs', false)"
        ]
    );
    assert_eq!(
        fake.rows("auth.users"),
        [
            "('00000000-0000-0000-0000-000000000001', 'seed@example.com', 'reset 1')",
            user
        ]
    );
    // ...and the migration history the dump leaves out was not emptied.
    // Auth and Storage read theirs to decide what to migrate on start.
    assert_eq!(fake.rows("auth.schema_migrations"), ["('20240101000000')"]);
    assert_eq!(
        fake.rows("storage.migrations"),
        ["(0, 'create-migrations-table')"]
    );
    assert_eq!(
        fake.rows("supabase_migrations.schema_migrations"),
        ["('20240101000000')"]
    );
}
