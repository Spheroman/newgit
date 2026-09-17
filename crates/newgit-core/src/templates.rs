/// Starter resource templates. `companions` are additional resource
/// definitions the template depends on, and `companion_trackers` are lanes
/// it deposits into; both are created alongside it when absent (e.g. the
/// user-owned `pnpm-store` that `deps` depends on, or the `db-snapshots`
/// lane a Postgres dump lands in).
#[derive(Debug, Clone, Copy)]
pub struct ResourceTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
    pub companions: &'static [CompanionFile],
    pub companion_trackers: &'static [CompanionTracker],
}

#[derive(Debug, Clone, Copy)]
pub struct CompanionFile {
    pub name: &'static str,
    pub contents: &'static str,
}

/// A tracker a template's checkpoint deposits into. Created through the same
/// CLI path as `newgit tracker create`, so trackers stay CLI-managed and
/// templates never hand-write tracker TOML.
#[derive(Debug, Clone, Copy)]
pub struct CompanionTracker {
    pub name: &'static str,
    pub audience: &'static str,
    pub merge_with_source: bool,
}

pub const RESOURCE_TEMPLATES: &[ResourceTemplate] = &[
    ResourceTemplate {
        name: "process",
        description: "a branch-local long-running process with its own port",
        companions: &[],
        companion_trackers: &[],
        contents: r#"# HOST TOOLS: whatever `[actions.start]` below runs. It ships as `npm run
# dev` because something has to be there; it is one of the two lines you are
# expected to replace.
ownership = "branch"
# Trackers or resources that must be ready first, e.g. ["deps", "runtime-env"].
depends_on = []

# In a monorepo, every action below likely runs from one package, not the
# workspace root; uncomment and every command runs from there instead.
# workdir = "packages/app"

[ports]
# `env` is the variable your dev server reads its port from. Every resource's
# environment lands in one process environment, so the name has to be unique
# across the project — it is named after this resource for that reason. If
# this is your only service and your tool insists on plain `PORT`, rename it.
app = { start = 3100, env = "RESOURCE_PORT" }

[actions.start]
# Edit to your dev command, e.g. "pnpm dev" or "bin/rails server".
command = "npm run dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
RESOURCE_URL = "http://127.0.0.1:{{ports.app}}"

# Most tools read their port from a committed config file rather than argv.
# Uncomment and point this at yours. There is no template file: `find` names
# the project's working default, so a clone without newgit still starts on it.
#
# `find` is literal, never a regex, and must match exactly once — which is
# also the drift detector. When the default changes upstream, the bind fails
# naming the file and the string instead of quietly doing nothing.
#
# [[render]]
# path = "vite.config.ts"
# replace = [
#   { find = "port: 3000", with = "port: {{ports.app}}" },
# ]
"#,
    },
    ResourceTemplate {
        name: "pnpm",
        description: "dependency install via pnpm; recomputed per instance, hardlinked from one store",
        // The shared package store is user-owned: newgit must never delete
        // or rewrite it. Created alongside so `depends_on` resolves.
        companions: &[CompanionFile {
            name: "pnpm-store",
            contents: r#"ownership = "user"

[identity]
paths = ["pnpm-lock.yaml"]

[checkpoint]
mode = "hash"

[restore]
mode = "none"
"#,
        }],
        companion_trackers: &[],
        contents: r#"# HOST TOOLS: `pnpm` and `node` on your PATH — `node` because
# `key_command` asks it what it is, not just to run the install.
#
# Every instance installs its own dependencies: two branches with
# different lockfiles must not share a tree, or one branch's install
# rewrites the other's. Two instances at the *same* lockfile are a
# different case — that is one tree built twice, so `produces` lets the
# second be cloned from the first instead of installed.
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]
# The tree `prepare` builds. A monorepo installing into several places
# names each one: paths are literal, and a directory means all of it.
produces = ["node_modules"]
# What the lockfile cannot see. Its hash says what was asked for, not what
# gets built — install scripts compile against this platform and this Node.
key_command = "node -v && uname -sm"

[actions.prepare]
command = "pnpm install --frozen-lockfile"

# Hashes `[identity] paths` above: what the install is derived from is
# declared once, and a `recompute` restore skips when it has not moved.
[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    },
    ResourceTemplate {
        name: "install",
        description: "dependency install via any package manager; lockfile and command are EDIT ME",
        companions: &[],
        companion_trackers: &[],
        contents: r#"# HOST TOOLS: your package manager, plus whatever `key_command` runs. Note
# that `key_command` ships as `node -v && uname -sm` — correct for a Node
# project and wrong for a uv or Cargo one, where it should ask *your*
# toolchain its version. It is an EDIT ME in everything but name.
#
# A generic starter: every package manager needs the same shape (install
# from a lockfile into a tree), so this template parameterizes nothing and
# marks the two lines that are actually yours to fill in. `pnpm` is the
# worked example — `newgit resource templates --show pnpm` — and also wires
# up a shared, content-addressed store; see "Installs: use a
# content-addressed store" in the reference for what that saves and what it
# costs to skip.
ownership = "workspace"
depends_on = []

[identity]
paths = ["EDIT ME: your lockfile, e.g. package-lock.json, uv.lock, Cargo.lock"]
# The tree `prepare` builds. A monorepo installing into several places
# names each one: paths are literal, and a directory means all of it.
produces = ["EDIT ME: what prepare installs, e.g. node_modules"]
# What the lockfile cannot see. Its hash says what was asked for, not what
# gets built — install scripts compile against this platform and this Node.
key_command = "node -v && uname -sm"

[actions.prepare]
command = "EDIT ME: your install command, e.g. npm ci, uv sync --frozen, cargo fetch"

# Hashes `[identity] paths` above: what the install is derived from is
# declared once, and a `recompute` restore skips when it has not moved.
[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    },
    ResourceTemplate {
        name: "command-snapshot",
        description: "a daemon-owned database captured through the daemon into a tracker",
        companions: &[],
        // The dump has to land somewhere versioned; this is the one seam
        // between the two primitives, so the lane ships with the template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"ownership = "branch"

# HOST TOOLS: `createdb`, `dropdb`, `pg_dump` and `psql` on your PATH,
# pointed at a Postgres running on the host. If your database lives in a
# container a tool manages (Docker Compose, the Supabase CLI), you have none
# of these against that database and the commands below will not work as
# written — this template is ready to adapt *if your database is on the
# host*. For Supabase specifically there is a `supabase` template that goes
# through the CLI and `docker exec` instead.
#
# A branch-local database, named after the instance so instances never share
# one. Edit the commands for your database; the shape is what matters:
# checkpoint emits a dump into the lane, restore reads it back.
#
# That shape assumes one thing: a FULL dump loaded into a database dropped
# and recreated fresh, so there is nothing for the load to collide with.
# That assumption breaks the moment the schema comes from migrations
# instead (Rails, Prisma, Supabase) — you cannot drop the database, because
# recreating it means replaying every migration. The restore is then reset,
# then load data over what the migrations already inserted, and the load
# needs a data-only dump plus an empty target or it dies on duplicate keys.
# Use the `command-snapshot-migrations` template for that shape instead of
# bending this one; the two are different strategies, not variations on one
# command.

[actions.prepare]
command = "createdb {{branch.slug}} || true"

# A convenience command, not a lifecycle hook: no stage runs `migrate`, and
# nothing but you ever will — `newgit action <this resource>.migrate`. Only
# `prepare` is run for you (by `spawn`, and by a `recompute` restore).
[actions.migrate]
command = "npm run db:migrate"

[checkpoint]
mode = "command"
command = "pg_dump {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "dropdb {{branch.slug}} --if-exists && createdb {{branch.slug}} && psql --quiet {{branch.slug}} < {{state_ref}}"

[cleanup]
command = "dropdb {{branch.slug}} --if-exists"

[exports]
RESOURCE_URL = "postgres://localhost/{{branch.slug}}"
"#,
    },
    ResourceTemplate {
        name: "command-snapshot-migrations",
        description: "a migration-managed database, restored by reset-empty-load instead of dropdb/createdb",
        companions: &[],
        // Shares the lane with `command-snapshot`: both deposit a Postgres
        // dump, and a lane is per-database, not per-template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"ownership = "branch"

# HOST TOOLS: `createdb`, `dropdb`, `pg_dump` and `psql` on your PATH,
# pointed at a Postgres running on the host. Note what that excludes: this
# template names Rails, Prisma and Supabase as its audience below, and a
# Supabase user — or a Prisma user on Docker Compose, which is most of them
# — has none of these binaries against the database, because it lives in a
# container the tool manages. This is ready to adapt *if your database is on
# the host*. If it is Supabase, start from the `supabase` template instead,
# which goes through the CLI and `docker exec`; if it is Compose, the shape
# below is still right and every command needs a `docker exec` in front.
#
# For a database whose schema comes from migrations, not from the dump —
# Rails, Prisma, Supabase, and anything else that runs `migrate` to build
# the schema. `command-snapshot`'s restore is dropdb/createdb, which only
# works when the dump is the whole database; here the schema has to come
# from replaying migrations, so the restore is reset, then load, and:
#
#   - the checkpoint is `--data-only`, since the schema isn't the dump's job
#   - reset re-runs the migrations, which insert their own seed rows, so the
#     database the load runs against is not empty
#   - the load has to empty it first, and only what this role may actually
#     truncate — a table the dump could not read (a stricter role owns it,
#     e.g. Supabase's storage internals) is not one the restore has any
#     business emptying either, and trying fails on a permission error
#     before the load ever runs
#
# Edit `reset` for your migration tool (`rails db:schema:load`, `prisma
# migrate reset --force`, `supabase db reset`, ...) and the `where` clause
# below for whichever schemas your dump actually covers.

[actions.prepare]
command = "createdb {{branch.slug}} || true"

[actions.reset]
command = "npm run db:migrate:reset"

[checkpoint]
mode = "command"
command = "pg_dump --data-only {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = """
npm run db:migrate:reset && psql --quiet {{branch.slug}} <<'SQL' && psql --quiet {{branch.slug}} < {{state_ref}}
do $$
declare t record;
begin
  for t in
    select schemaname, tablename from pg_tables
     where schemaname = 'public'
       and has_table_privilege(format('%I.%I', schemaname, tablename), 'truncate')
  loop
    execute format('truncate table %I.%I restart identity cascade', t.schemaname, t.tablename);
  end loop;
end $$;
SQL
"""

[cleanup]
command = "dropdb {{branch.slug}} --if-exists"

[exports]
RESOURCE_URL = "postgres://localhost/{{branch.slug}}"
"#,
    },
    ResourceTemplate {
        name: "supabase",
        description: "a per-instance Supabase stack: own project_id, own six ports, dump/reset/load",
        companions: &[],
        // Same lane as the other two database templates: a lane is per
        // database, not per template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"ownership = "branch"

# HOST TOOLS: `supabase` (CLI) and `docker`. Nothing here touches host
# `psql`, `pg_dump` or `createdb` — the database lives inside a container
# the CLI manages, so every database command goes through the CLI or
# through `docker exec`. That is the whole reason this template exists
# apart from `command-snapshot-migrations`, which assumes those binaries
# are on your PATH and pointed at a host cluster.
#
# The one line you MUST edit is `project_id` in the [[render]] below: it has
# to match, exactly and literally, what is committed in supabase/config.toml.
# It is load-bearing rather than cosmetic — `project_id` is what scopes every
# container name and Docker volume, so without a per-instance value the
# second instance's `supabase start` adopts the first instance's stack
# instead of starting its own. A mismatched `find` refuses at spawn naming
# the file and the string, which is the failure you want; a *missing* render
# would silently share a database between branches.

[ports]
# Supabase's committed defaults, one lane each. `start` is where scanning
# begins, so instance two lands just above instance one.
api       = { start = 54321, env = "RESOURCE_API_PORT" }
db        = { start = 54322, env = "RESOURCE_DB_PORT" }
shadow    = { start = 54320 }
studio    = { start = 54323 }
inbucket  = { start = 54324 }
analytics = { start = 54327 }

[exports]
# EDIT ME: the left half must match your committed `project_id`.
RESOURCE_PROJECT = "EDIT_ME_PROJECT-{{branch.slug}}"
# The CLI names containers `supabase_<service>_<project_id>`; the restore
# below needs the database one by name because it loads through `docker
# exec`, not through a host client.
RESOURCE_DB_CONTAINER = "supabase_db_{{exports.RESOURCE_PROJECT}}"
RESOURCE_API_URL = "http://127.0.0.1:{{ports.api}}"
RESOURCE_DB_URL = "postgresql://postgres:postgres@127.0.0.1:{{ports.db}}/postgres"

# There is no template file: each `find` below is your project's committed
# working default, so a clone without newgit still starts on it. Each must
# occur exactly once or the render refuses, which is also the drift detector
# when Supabase changes a default upstream.
#
# Careful with `shadow_port`: the literal `port = 54320` is a substring of
# `shadow_port = 54320`, so it is spelled out in full here rather than left
# to match the wrong line.
[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "EDIT_ME_PROJECT"', with = 'project_id = "{{exports.RESOURCE_PROJECT}}"' },
  { find = "port = 54321",        with = "port = {{ports.api}}" },
  { find = "port = 54322",        with = "port = {{ports.db}}" },
  { find = "shadow_port = 54320", with = "shadow_port = {{ports.shadow}}" },
  { find = "port = 54323",        with = "port = {{ports.studio}}" },
  { find = "port = 54324",        with = "port = {{ports.inbucket}}" },
  { find = "port = 54327",        with = "port = {{ports.analytics}}" },
]

# If a dev server in this project publishes a URL the auth config has to
# allow (Expo is the usual case), uncomment this. Naming another resource's
# export IS the dependency declaration — newgit reads the edge and binds that
# resource first. Do not also add it to `depends_on`: that would claim a
# lifecycle dependency and reverse into teardown order, which is not what
# needing one string means.
#
# [[render]]
# path = "supabase/config.toml"
# replace = [
#   { find = 'additional_redirect_urls = ["exp://127.0.0.1:8081"]',
#     with = 'additional_redirect_urls = ["{{exports.EXPO_URL}}"]' },
# ]

[actions.prepare]
# Reads the rendered config.toml, so it starts this instance's own stack.
# Ports and exports are bound before a resource's own `prepare` runs.
command = "supabase start"

# A convenience command, not a lifecycle hook: nothing runs this for you.
[actions.reset]
command = "supabase db reset"

[checkpoint]
mode = "command"
# `--data-only`, because the schema comes from migrations rather than from
# the dump — the same reasoning as `command-snapshot-migrations`.
command = "supabase db dump --local --data-only -f {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
# reset (replays migrations, which insert their own seed rows), empty what
# this role may truncate, then load the data-only dump. Loading straight
# onto a freshly reset database dies on duplicate keys.
#
# `has_table_privilege` is the filter, not a schema list: a table the dump
# could not read because a stricter role owns it (Supabase's storage and
# auth internals) is not one the restore has any business emptying either,
# and trying fails on a permission error before the load ever runs.
command = """
supabase db reset && docker exec -i {{exports.RESOURCE_DB_CONTAINER}} psql --quiet -U postgres -d postgres <<'SQL' && docker exec -i {{exports.RESOURCE_DB_CONTAINER}} psql --quiet -U postgres -d postgres < {{state_ref}}
do $$
declare t record;
begin
  for t in
    select schemaname, tablename from pg_tables
     where schemaname = 'public'
       and has_table_privilege(format('%I.%I', schemaname, tablename), 'truncate')
  loop
    execute format('truncate table %I.%I restart identity cascade', t.schemaname, t.tablename);
  end loop;
end $$;
SQL
"""

[cleanup]
# `--no-backup` because the instance is going away: the checkpoint lane is
# where a dump worth keeping already lives, and a backup volume left behind
# would outlive the instance that owned it.
command = "supabase stop --no-backup"
"#,
    },
    ResourceTemplate {
        name: "external",
        description: "a resource another system owns; newgit holds only a handle",
        companions: &[],
        companion_trackers: &[],
        contents: r#"# HOST TOOLS: your provider's CLI. `cloudctl` below is a stand-in and is
# not a real program — every command here is yours to write.
ownership = "external"

# newgit does not own this resource, so it never assumes deletion semantics
# it did not author: `ownership = "external"` means cleanup runs exactly the
# command below and nothing else.
#
# `captures` reads names out of the prepare command's stdout — either a flat
# JSON object or KEY=VALUE lines — and publishes them as this resource's
# exports, so `newgit run` and the hooks below can use them.
#
# When `captures` is set, stdout belongs to newgit: send anything else the
# command prints to stderr, or a chatty CLI's progress output will be
# interleaved with the values and they will not parse. A declared name that
# never turns up is reported as a warning, not an error.

[actions.prepare]
# Edit to your provisioning command.
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["RESOURCE_ID", "RESOURCE_URL"]

[checkpoint]
mode = "external"
state_ref = "{{exports.RESOURCE_ID}}"

[restore]
# The handle is recorded, not re-created: an undo does not rewind another
# system. Change to `mode = "command"` if yours can be rewound.
mode = "external"

[cleanup]
command = "cloudctl preview delete {{state_ref}}"
"#,
    },
];

/// The token every template uses where an environment variable name has to be
/// unique across the project. Replaced with the resource's own name when the
/// template is instantiated.
///
/// Templates cannot ship conventional names like `PORT` or `DATABASE_URL`:
/// every resource's environment lands in one process environment, so the
/// second `newgit resource add --template process` would claim a name the
/// first already owns and refuse the whole graph. The one thing newgit knows
/// is unique is the resource name, which is the file it is writing.
const NAME_TOKEN: &str = "RESOURCE_";

/// Instantiate a template for a resource of this name: `RESOURCE_PORT`
/// becomes `WEB_PORT` for a resource called `web`.
pub fn instantiate(contents: &str, resource_name: &str) -> String {
    contents.replace(NAME_TOKEN, &format!("{}_", env_prefix(resource_name)))
}

/// A resource name as an environment variable name fragment: `dev-db` ->
/// `DEV_DB`. Resource names are already validated to a conservative
/// character set, so uppercasing and swapping `-` is enough.
fn env_prefix(resource_name: &str) -> String {
    resource_name.to_uppercase().replace('-', "_")
}

pub fn resource_template(name: &str) -> Option<&'static ResourceTemplate> {
    RESOURCE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

/// Names of every starter template, in listing order — for error messages
/// that need to name the valid options (e.g. `resource templates --show`
/// given a name that doesn't exist).
pub fn resource_template_names() -> Vec<&'static str> {
    RESOURCE_TEMPLATES
        .iter()
        .map(|template| template.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two of the same template is the canonical setup (a web and an api),
    /// and it must not produce two resources fighting over one name.
    #[test]
    fn instantiating_one_template_twice_yields_disjoint_env_names() {
        let process = resource_template("process").expect("process template");
        let web = instantiate(process.contents, "web");
        let api = instantiate(process.contents, "api");

        assert!(web.contains(r#"env = "WEB_PORT""#) && web.contains("WEB_URL ="));
        assert!(api.contains(r#"env = "API_PORT""#) && api.contains("API_URL ="));
        assert!(
            !web.contains(NAME_TOKEN) && !api.contains(NAME_TOKEN),
            "no token survives instantiation"
        );
    }

    /// A hyphen is legal in a resource name and illegal in most shells'
    /// variable names.
    #[test]
    fn a_hyphenated_resource_name_becomes_a_usable_variable_name() {
        assert_eq!(
            instantiate("env = \"RESOURCE_PORT\"", "dev-db"),
            "env = \"DEV_DB_PORT\""
        );
    }

    /// Every shipped template has to survive its own instantiation, or
    /// `resource add` writes a file that will not parse.
    #[test]
    fn every_template_still_parses_after_instantiation() {
        for template in RESOURCE_TEMPLATES {
            let contents = instantiate(template.contents, "sample");
            toml::from_str::<toml::Table>(&contents)
                .unwrap_or_else(|e| panic!("template `{}` does not parse: {e}", template.name));
            for companion in template.companions {
                let contents = instantiate(companion.contents, companion.name);
                toml::from_str::<toml::Table>(&contents).unwrap_or_else(|e| {
                    panic!("companion `{}` does not parse: {e}", companion.name)
                });
            }
        }
    }

    #[test]
    fn resource_template_finds_every_listed_name() {
        for name in resource_template_names() {
            assert!(
                resource_template(name).is_some(),
                "`{name}` is listed but resource_template() can't find it"
            );
        }
    }

    #[test]
    fn resource_template_rejects_unknown_names() {
        assert!(resource_template("does-not-exist").is_none());
    }

    #[test]
    fn resource_template_names_matches_listing_order() {
        let expected: Vec<&str> = RESOURCE_TEMPLATES.iter().map(|t| t.name).collect();
        assert_eq!(resource_template_names(), expected);
    }
}
