//! The bundled database templates, run for real: spawn, write a row,
//! checkpoint, change it, `undo`, and the row is back; a second instance
//! has a database of its own on a port of its own.
//!
//! Each test needs the tools its template names and skips, saying so, when
//! they are missing: `postgres` and `command-snapshot*` need a Postgres
//! install on PATH, `compose` needs `docker compose` and a daemon that
//! answers, `supabase` needs the Supabase CLI and the same daemon. Every
//! instance, cluster, container and volume a test makes is removed when it
//! ends, passed or not.

#![allow(dead_code)]

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::SourceSubstrate;
use newgit_core::config::WorkspaceSection;
use newgit_core::store::MetadataStore;
use serde_json::Value;

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

/// Whether `program` is on PATH.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The programs of `programs` that are not on PATH. A test that needs any
/// of them prints which and returns.
fn missing(programs: &[&str]) -> Vec<String> {
    programs
        .iter()
        .filter(|program| !on_path(program))
        .map(|program| (*program).to_owned())
        .collect()
}

fn skip(test: &str, why: &str) {
    eprintln!("skipping {test}: {why}");
}

/// Whether a Docker daemon answers, so a machine with the client installed
/// and Docker Desktop quit skips rather than fails.
fn docker_answers() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// One test's project. On drop it removes every instance the test spawned
/// (which stops their processes and runs their `[cleanup]`), runs whatever
/// extra teardown the test registered for what `remove` cannot reach, and
/// deletes the directory.
struct Project {
    dir: Utf8PathBuf,
    repo: Utf8PathBuf,
    instances: std::cell::RefCell<Vec<String>>,
    teardown: std::cell::RefCell<Vec<Vec<String>>>,
    /// Set on every command the test runs, newgit and teardown alike.
    env: std::cell::RefCell<Vec<(String, String)>>,
}

impl Drop for Project {
    fn drop(&mut self) {
        for instance in self.instances.borrow().iter().rev() {
            let _ = Command::new(env!("CARGO_BIN_EXE_newgit"))
                .current_dir(self.repo.as_str())
                .envs(self.env.borrow().iter().cloned())
                .args(["remove", "--purge", "--force", instance])
                .output();
        }
        for argv in self.teardown.borrow().iter() {
            let _ = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(self.repo.as_str())
                .envs(self.env.borrow().iter().cloned())
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Project {
    /// A Git repository holding `files`, committed, with newgit
    /// initialized and its workspaces under the same temporary directory.
    fn new(label: &str, files: &[(&str, &str)]) -> Project {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ngdb-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let dir = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
            .expect("utf8 tempdir");
        let repo = dir.join("store");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let project = Project {
            dir: dir.clone(),
            repo: repo.clone(),
            instances: Default::default(),
            teardown: Default::default(),
            env: Default::default(),
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        for (path, contents) in files {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write fixture");
        }
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        let store = MetadataStore::init(&repo, "proj", SourceSubstrate::Git).expect("init");
        let mut config = store.load_config().expect("config");
        config.workspace = Some(WorkspaceSection {
            root: Some(dir.join("workspaces")),
            materializer: None,
        });
        store.write_config(&config).expect("write config");
        project
    }

    fn newgit(&self, args: &[&str]) -> (Option<i32>, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.repo.as_str())
            .args(args)
            .envs(self.env.borrow().iter().cloned())
            .output()
            .expect("newgit runs");
        (
            output.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }

    /// `newgit <args>`, which must succeed; returns what it printed.
    fn ok(&self, args: &[&str]) -> String {
        let (code, said) = self.newgit(args);
        assert_eq!(code, Some(0), "`newgit {}` failed:\n{said}", args.join(" "));
        said
    }

    fn spawn(&self, instance: &str) -> Value {
        self.instances.borrow_mut().push(instance.to_owned());
        let (code, stdout) = {
            let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
                .current_dir(self.repo.as_str())
                .args(["spawn", instance, "--json"])
                .envs(self.env.borrow().iter().cloned())
                .output()
                .expect("newgit runs");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        };
        let report: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("spawn --json is not JSON ({error}):\n{stdout}"));
        if code != Some(0) {
            let log = self.logs(instance);
            panic!("spawn {instance} failed:\n{report:#}\n{log}");
        }
        report
    }

    /// Every log of `instance`, for a failure message.
    fn logs(&self, instance: &str) -> String {
        let mut said = String::new();
        let mut pending = vec![self.repo.join(".newgit/logs").into_std_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                if !entry.path().to_string_lossy().contains(instance) {
                    continue;
                }
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                said.push_str(&format!("--- {}\n{text}\n", entry.path().display()));
            }
        }
        said
    }

    /// `sh -c <script>` in `instance`'s command environment; its stdout.
    fn run(&self, instance: &str, script: &str) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
            .current_dir(self.repo.as_str())
            .args(["run", instance, "--", "sh", "-c", script])
            .envs(self.env.borrow().iter().cloned())
            .output()
            .expect("newgit runs");
        assert!(
            output.status.success(),
            "`{script}` in {instance} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// The value `instance` publishes as `name`.
    fn env(&self, instance: &str, name: &str) -> String {
        self.run(instance, &format!("printf %s \"${name}\""))
    }

    /// Wait for `newgit status` to call `instance` ready. `spawn` waits for
    /// what it started to come up; `undo` and `start` restart processes and
    /// return without waiting, so a query straight after them can find the
    /// server still starting.
    fn wait_ready(&self, instance: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let (_, said) = self.newgit(&["status", instance]);
            let row = said.lines().nth(1).unwrap_or_default();
            if row.contains(" ready ") {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "`{instance}` never came up:\n{said}"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    fn resource(&self, name: &str) -> Utf8PathBuf {
        self.repo.join(format!(".newgit/resources/{name}.toml"))
    }

    /// Replace `find`, which must occur, in resource `name`'s definition.
    fn edit_resource(&self, name: &str, find: &str, with: &str) {
        let path = self.resource(name);
        let contents = std::fs::read_to_string(&path).expect("read definition");
        assert!(contents.contains(find), "`{find}` not in {path}");
        std::fs::write(&path, contents.replace(find, with)).expect("write definition");
    }
}

/// The round trip every database template has to survive, given how to
/// run SQL in an instance. `sql` takes an instance and a statement and
/// returns what it printed, unaligned and tuples-only.
fn round_trip(project: &Project, sql: &dyn Fn(&str, &str) -> String) {
    sql(
        "a",
        "create table notes (body text); insert into notes values ('kept')",
    );
    let said = project.ok(&["checkpoint", "a", "-m", "with kept"]);
    assert!(
        said.contains("db-snapshots") || said.contains("database"),
        "{said}"
    );

    sql(
        "a",
        "update notes set body = 'changed'; insert into notes values ('added')",
    );
    assert_eq!(
        sql("a", "select body from notes order by body"),
        "added\nchanged"
    );

    let said = project.ok(&["undo", "a"]);
    assert!(!said.contains("INCOMPLETE"), "{said}");
    project.wait_ready("a");
    assert_eq!(
        sql("a", "select body from notes order by body"),
        "kept",
        "undo did not bring the data back:\n{said}"
    );
}

/// `checkpoint --verify`: the restore lands where the checkpoint started,
/// and the dump is the same bytes for the same state.
fn verifies(project: &Project, instance: &str) {
    let said = project.ok(&["checkpoint", "--verify", instance]);
    assert!(!said.contains("MISMATCH"), "{said}");
}

// ---------------------------------------------------------------- postgres

const POSTGRES_TOOLS: &[&str] = &[
    "initdb",
    "postgres",
    "pg_ctl",
    "pg_isready",
    "pg_dumpall",
    "psql",
];

fn postgres_sql(project: &Project) -> impl Fn(&str, &str) -> String + '_ {
    move |instance, statement| {
        project.run(
            instance,
            &format!("psql -X -qAt -v ON_ERROR_STOP=1 \"$DATABASE_URL\" -c \"{statement}\""),
        )
    }
}

#[test]
fn postgres_template_round_trips_and_isolates() {
    let missing = missing(POSTGRES_TOOLS);
    if !missing.is_empty() {
        return skip(
            "postgres_template_round_trips_and_isolates",
            &format!("not on PATH: {}", missing.join(", ")),
        );
    }
    let project = Project::new("postgres", &[(".gitignore", "/.postgres/\n")]);
    project.ok(&["resource", "add", "database", "--template", "postgres"]);

    project.spawn("a");
    let a_url = project.env("a", "DATABASE_URL");
    assert!(
        a_url.starts_with("postgresql://postgres@127.0.0.1:"),
        "{a_url}"
    );
    let sql = postgres_sql(&project);
    round_trip(&project, &sql);
    // A database the app made itself comes back too: the dump is the
    // whole cluster.
    sql("a", "create database extra");
    project.ok(&["checkpoint", "a"]);
    sql("a", "drop database extra");
    project.ok(&["undo", "a"]);
    project.wait_ready("a");
    assert_eq!(
        sql(
            "a",
            "select count(*) from pg_database where datname = 'extra'"
        ),
        "1"
    );
    verifies(&project, "a");
    project.wait_ready("a");

    project.spawn("b");
    let b_url = project.env("b", "DATABASE_URL");
    assert_ne!(a_url, b_url, "two instances share a database URL");
    assert_eq!(
        sql(
            "b",
            "select count(*) from pg_tables where tablename = 'notes'"
        ),
        "0",
        "instance b sees a's table"
    );
    sql(
        "b",
        "create table notes (body text); insert into notes values ('b')",
    );
    assert_eq!(sql("a", "select body from notes"), "kept");

    // Stopped and started, the data is where it was.
    project.ok(&["stop", "a"]);
    project.ok(&["start", "a"]);
    project.wait_ready("a");
    assert_eq!(sql("a", "select body from notes"), "kept");
}

#[test]
fn postgres_template_refuses_a_cluster_git_would_capture() {
    let missing = missing(POSTGRES_TOOLS);
    if !missing.is_empty() {
        return skip(
            "postgres_template_refuses_a_cluster_git_would_capture",
            &format!("not on PATH: {}", missing.join(", ")),
        );
    }
    let project = Project::new("pg-unignored", &[]);
    project.ok(&["resource", "add", "database", "--template", "postgres"]);
    project.instances.borrow_mut().push("a".to_owned());
    let (code, said) = project.newgit(&["spawn", "a"]);
    assert_eq!(code, Some(1), "{said}");
    let said = format!("{said}{}", project.logs("a"));
    assert!(said.contains(".postgres/ is not gitignored"), "{said}");
}

// ------------------------------------------------- command-snapshot(-migrations)

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Start a Postgres server of the test's own, standing in for the one a
/// developer runs on the host, and point libpq at it through `PG*` for
/// every command the project runs. Stopped when the project drops.
fn shared_server(project: &Project) {
    let data = project.dir.join("shared-server");
    let port = free_port().to_string();
    let output = Command::new("initdb")
        .args(["-D", data.as_str(), "-U", "postgres", "--auth=trust"])
        .args(["--encoding=UTF8", "--locale=C", "--no-instructions"])
        .output()
        .expect("initdb runs");
    assert!(output.status.success(), "{output:?}");
    let options = format!("-p {port} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''");
    let output = Command::new("pg_ctl")
        .args(["-D", data.as_str(), "-w", "-o", &options, "-l"])
        .arg(project.dir.join("shared-server.log").as_str())
        .arg("start")
        .output()
        .expect("pg_ctl runs");
    assert!(output.status.success(), "{output:?}");
    project.teardown.borrow_mut().push(
        ["pg_ctl", "-D", data.as_str(), "-m", "immediate", "stop"]
            .map(str::to_owned)
            .to_vec(),
    );
    project.env.borrow_mut().extend(
        [
            ("PGHOST", "127.0.0.1"),
            ("PGPORT", &port),
            ("PGUSER", "postgres"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned())),
    );
}

/// SQL against the instance's database in the shared server, which is
/// named after the instance.
fn shared_sql(project: &Project) -> impl Fn(&str, &str) -> String + '_ {
    move |instance, statement| {
        project.run(
            instance,
            &format!("psql -X -qAt -v ON_ERROR_STOP=1 -d {instance} -c \"{statement}\""),
        )
    }
}

#[test]
fn command_snapshot_template_round_trips_and_isolates() {
    let missing = missing(&[
        "initdb",
        "pg_ctl",
        "createdb",
        "dropdb",
        "pg_dump",
        "psql",
        "pg_isready",
    ]);
    if !missing.is_empty() {
        return skip(
            "command_snapshot_template_round_trips_and_isolates",
            &format!("not on PATH: {}", missing.join(", ")),
        );
    }
    let project = Project::new("snapshot", &[]);
    shared_server(&project);
    project.ok(&[
        "resource",
        "add",
        "database",
        "--template",
        "command-snapshot",
    ]);

    project.spawn("a");
    let sql = shared_sql(&project);
    round_trip(&project, &sql);
    verifies(&project, "a");

    project.spawn("b");
    assert_eq!(
        sql(
            "b",
            "select count(*) from pg_tables where tablename = 'notes'"
        ),
        "0",
        "instance b sees a's table"
    );

    // A spawn against a server that is down fails, rather than reporting a
    // database that was never created as ready.
    project
        .env
        .borrow_mut()
        .push(("PGPORT".to_owned(), free_port().to_string()));
    project.instances.borrow_mut().push("c".to_owned());
    let (code, said) = project.newgit(&["spawn", "c"]);
    project.env.borrow_mut().pop();
    assert_eq!(
        code,
        Some(1),
        "spawn against a server that is down:\n{said}"
    );
    assert!(said.contains("createdb"), "{said}");
}

#[test]
fn command_snapshot_migrations_template_round_trips() {
    let missing = missing(&[
        "initdb",
        "pg_ctl",
        "createdb",
        "dropdb",
        "pg_dump",
        "psql",
        "pg_isready",
    ]);
    if !missing.is_empty() {
        return skip(
            "command_snapshot_migrations_template_round_trips",
            &format!("not on PATH: {}", missing.join(", ")),
        );
    }
    let project = Project::new("migrations", &[]);
    shared_server(&project);
    project.ok(&[
        "resource",
        "add",
        "database",
        "--template",
        "command-snapshot-migrations",
    ]);
    // The migration tool, standing in for `rails db:reset`: the schema
    // comes from here, and so does a seed row the restore must not double.
    project.edit_resource(
        "database",
        "npm run db:migrate:reset",
        "psql -Xq -v ON_ERROR_STOP=1 -d {{branch.slug}} -c 'drop schema public cascade; create schema public; create table notes (id serial primary key, body text); insert into notes (body) values ($$seed$$)'",
    );
    project.spawn("a");
    project.ok(&["action", "database.reset", "a"]);
    let sql = shared_sql(&project);
    sql("a", "insert into notes (body) values ('kept')");
    project.ok(&["checkpoint", "a"]);
    sql(
        "a",
        "update notes set body = 'changed'; insert into notes (body) values ('added')",
    );

    let said = project.ok(&["undo", "a"]);
    assert!(!said.contains("INCOMPLETE"), "{said}");
    assert_eq!(
        sql("a", "select id || ':' || body from notes order by id"),
        "1:seed\n2:kept"
    );
    verifies(&project, "a");
}

// ----------------------------------------------------------------- compose

const COMPOSE_FILE: &str = r#"services:
  db:
    image: postgres:17-alpine
    environment:
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: app
    ports:
      - "127.0.0.1:${DATABASE_PORT:-5432}:5432"
    volumes:
      - db-data:/var/lib/postgresql/data
volumes:
  db-data:
"#;

fn compose_sql(project: &Project) -> impl Fn(&str, &str) -> String + '_ {
    move |instance, statement| {
        project.run(
            instance,
            &format!(
                "docker compose exec -T db psql -X -qAt -v ON_ERROR_STOP=1 -U postgres -d app -c \"{statement}\""
            ),
        )
    }
}

/// Whether Compose has a container for `project` at all, running or not.
fn compose_containers(project_name: &str) -> String {
    let output = Command::new("docker")
        .args(["ps", "-a", "-q", "--filter"])
        .arg(format!("label=com.docker.compose.project={project_name}"))
        .output()
        .expect("docker runs");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn compose_template_round_trips_and_isolates() {
    if !missing(&["docker"]).is_empty() || !docker_answers() {
        return skip(
            "compose_template_round_trips_and_isolates",
            "`docker` is not on PATH or its daemon does not answer",
        );
    }
    let project = Project::new("compose", &[("compose.yaml", COMPOSE_FILE)]);
    project.ok(&["resource", "add", "database", "--template", "compose"]);
    let name = format!("ngdb{}", std::process::id());
    project.edit_resource("database", "EDIT_ME_PROJECT", &name);
    for instance in ["a", "b"] {
        project.teardown.borrow_mut().push(
            ["docker", "compose", "-p", &format!("{name}-{instance}")]
                .into_iter()
                .chain(["down", "--volumes", "--remove-orphans"])
                .map(str::to_owned)
                .collect(),
        );
    }

    project.spawn("a");
    assert_eq!(
        project.env("a", "COMPOSE_PROJECT_NAME"),
        format!("{name}-a")
    );
    let sql = compose_sql(&project);
    round_trip(&project, &sql);
    verifies(&project, "a");

    project.spawn("b");
    assert_ne!(
        project.env("a", "DATABASE_URL"),
        project.env("b", "DATABASE_URL"),
        "two instances share a database URL"
    );
    assert_eq!(
        sql(
            "b",
            "select count(*) from pg_tables where tablename = 'notes'"
        ),
        "0",
        "instance b sees a's table"
    );

    // Suspended and resumed, the data is where it was.
    project.ok(&["stop", "a"]);
    project.ok(&["start", "a"]);
    project.wait_ready("a");
    assert_eq!(sql("a", "select body from notes"), "kept");

    // Removing an instance takes its containers and volume with it.
    project.ok(&["remove", "--purge", "a"]);
    assert_eq!(compose_containers(&format!("{name}-a")), "");
    assert!(!compose_containers(&format!("{name}-b")).is_empty());
}

// ---------------------------------------------------------------- supabase

/// What `supabase init` writes, with `project_id` set and the services a
/// database round trip does not need switched off, so a test stack is the
/// database, Auth, PostgREST and Kong in front of them.
fn supabase_config(project_id: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!("ngdb-sbinit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let output = Command::new("supabase")
        .arg("init")
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("supabase runs");
    let config = std::fs::read_to_string(dir.join("supabase/config.toml"));
    let _ = std::fs::remove_dir_all(&dir);
    if !output.status.success() {
        return None;
    }
    let mut config = config.expect("supabase init writes config.toml");
    let line = config
        .lines()
        .find(|line| line.starts_with("project_id = "))
        .expect("project_id")
        .to_owned();
    config = config.replace(&line, &format!("project_id = \"{project_id}\""));
    for section in [
        "realtime",
        "studio",
        "local_smtp",
        "storage",
        "edge_runtime",
        "analytics",
    ] {
        let enabled = format!("[{section}]\nenabled = true");
        if config.contains(&enabled) {
            config = config.replace(&enabled, &format!("[{section}]\nenabled = false"));
        }
    }
    Some(config)
}

fn supabase_sql(project: &Project) -> impl Fn(&str, &str) -> String + '_ {
    move |instance, statement| {
        project.run(
            instance,
            &format!(
                "docker exec -i \"$SUPABASE_DB_CONTAINER\" psql -X -qAt -v ON_ERROR_STOP=1 -U postgres -d postgres -c \"{statement}\""
            ),
        )
    }
}

#[test]
fn supabase_template_round_trips_and_isolates() {
    if !missing(&["docker", "supabase"]).is_empty() || !docker_answers() {
        return skip(
            "supabase_template_round_trips_and_isolates",
            "`docker` or `supabase` is not on PATH, or the Docker daemon does not answer",
        );
    }
    let project_id = format!("ngdbsb{}", std::process::id());
    let Some(config) = supabase_config(&project_id) else {
        return skip(
            "supabase_template_round_trips_and_isolates",
            "`supabase init` failed",
        );
    };
    let project = Project::new(
        "supabase",
        &[
            ("supabase/config.toml", &config),
            (
                "supabase/migrations/20260101000000_notes.sql",
                "create table notes (id serial primary key, body text);\n",
            ),
            (
                "supabase/seed.sql",
                "insert into notes (body) values ('seed');\n",
            ),
        ],
    );
    project.ok(&["resource", "add", "supabase", "--template", "supabase"]);
    project.edit_resource("supabase", "EDIT_ME_PROJECT", &project_id);
    for instance in ["a", "b"] {
        project.teardown.borrow_mut().push(
            ["supabase", "stop", "--no-backup", "--project-id"]
                .into_iter()
                .map(str::to_owned)
                .chain([format!("{project_id}-{instance}")])
                .collect(),
        );
    }

    let spawned = project.spawn("a");
    assert_eq!(
        spawned["starting"],
        serde_json::json!([]),
        "the [ready] check did not pass after `supabase start`:\n{spawned:#}"
    );
    let sql = supabase_sql(&project);
    assert_eq!(sql("a", "select body from notes"), "seed");
    sql("a", "insert into notes (body) values ('kept')");
    project.ok(&["checkpoint", "a"]);
    sql(
        "a",
        "update notes set body = 'changed'; insert into notes (body) values ('added')",
    );

    let said = project.ok(&["undo", "a"]);
    assert!(!said.contains("INCOMPLETE"), "{said}");
    assert_eq!(
        sql("a", "select id || ':' || body from notes order by id"),
        "1:seed\n2:kept",
        "undo did not bring the data back:\n{said}"
    );
    verifies(&project, "a");

    project.spawn("b");
    assert_ne!(
        project.env("a", "SUPABASE_DB_URL"),
        project.env("b", "SUPABASE_DB_URL"),
        "two instances share a database URL"
    );
    assert_eq!(sql("b", "select body from notes"), "seed");
    assert_eq!(sql("a", "select count(*) from notes"), "2");

    // The [ready] check sees a stack that went down behind newgit's back.
    project.run(
        "b",
        "docker stop \"supabase_kong_$SUPABASE_PROJECT\" >/dev/null",
    );
    let (_, status) = project.newgit(&["status", "b"]);
    assert!(status.contains("starting"), "{status}");
}
