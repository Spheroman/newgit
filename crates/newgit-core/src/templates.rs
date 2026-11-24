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

# THE PORT IS OPTIONAL. This template assumes a dev server with a port of its
# own. If yours listens on none (a worker, a queue consumer, a file watcher
# like `tsc --watch`, or `supabase functions serve`, which is served through
# the `supabase` resource's `api` port), delete `[ports]`, `[exports]` and
# `[[render]]` below and keep only the actions. Name what it needs in
# `depends_on` above (`supabase functions serve` wants ["supabase"]).

[ports]
# `env` is the variable your dev server reads its port from. Every resource's
# environment lands in one process environment, so the name has to be unique
# across the project — it is named after this resource for that reason. If
# this is your only service and your tool insists on plain `PORT`, rename it.
#
# Pass every port explicitly. A dev server that finds its port taken, or a
# tool that picks a second one for itself (`remix dev`'s, a HMR socket), scans
# for a free one and can take a port newgit claimed for another instance.
# Give each such port a line here and hand it over: `--port {{ports.app}}`.
app = { start = 3100, env = "RESOURCE_PORT" }

[actions.start]
# Edit to your dev command, e.g. "pnpm dev" or "bin/rails server".
command = "npm run dev"
long_running = true
# Started at spawn, once every `prepare` has run. Remove it to start the dev
# server only when you ask, with `newgit action <resource>.start`.
autostart = true

[actions.stop]
signal = "term"

# What "up" means. With no `[ready]`, it is the port answering, given five
# seconds. A dev server that binds at once and serves errors until it has
# compiled is up too early by that measure, and one that takes longer than
# five seconds reads `starting`; what `depends_on` this one starts once it
# is up. Uncomment to wait for a page that answers instead.
#
# [ready]
# http = "http://127.0.0.1:{{ports.app}}/"
# timeout_seconds = 60

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

# pnpm writes this workspace's absolute path into its install state, and a
# tree that names its own workspace is never shared. The store reverses the
# path to `find` when it stores the tree and renders it back for each
# instance it fills. `find` is any string that does not otherwise occur.
[[render]]
path = "node_modules/.pnpm-workspace-state-v1.json"
replace = [{ find = "/__newgit_workspace__", with = "{{workspace}}" }]

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
        name: "postgres",
        description: "a Postgres cluster per instance, no Docker; add it as `database` to get DATABASE_URL",
        companions: &[],
        // The same lane as the other database templates: a lane is per
        // database, not per template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"# ADD IT AS `database`: `newgit resource add database --template postgres`.
# Every variable this file publishes is named after the resource, so that
# name makes them `DATABASE_URL` and `DATABASE_PORT`, which is what Rails,
# Prisma, Django, Laravel and most other frameworks read already. Under any
# other name they are `<NAME>_URL` and `<NAME>_PORT`, which no app reads
# until you point it at them.
#
# THE ONE THING TO ADD: `/.postgres/` in the project's .gitignore, committed.
# The cluster's files are in the workspace, and a file Git does not ignore
# would be captured into every checkpoint as source. `prepare` refuses until
# it is ignored rather than doing that quietly.
#
# HOST TOOLS: `initdb`, `postgres`, `pg_ctl`, `pg_isready`, `pg_dumpall` and
# `psql`, all from one Postgres install (`brew install postgresql@17`, apt's
# `postgresql`). Debian and Ubuntu put the server programs (`initdb`,
# `postgres`, `pg_ctl`) in /usr/lib/postgresql/<version>/bin, which is not
# on PATH; add it. Nothing here needs Docker, a running Postgres, or root.
#
# Each instance gets a whole cluster of its own: its own `postgres` process,
# on its own port, over its own data directory in the workspace. Nothing is
# shared, so a migration, a `drop database` or a full disk on one branch
# cannot reach another, and every database the app creates in the cluster
# is the branch's. Contrast `command-snapshot`, which makes one database per
# instance inside a server everybody shares: lighter, but every instance
# shares that server's roles, extensions, settings and uptime, and it is the
# server's owner, not newgit, who decides what cleanup may touch.
#
# The cluster lives and dies with the workspace, which is what
# `ownership = "workspace"` says: `newgit remove` dumps it into the
# checkpoint it takes first and deletes it with the tree, refusing when the
# dump fails (a stopped server: `newgit start` it first) unless `--force`,
# and `undo` puts it back from the last dump. `newgit stop --evict` refuses
# while it is there, since an ignored file no tracker owns is not something
# eviction brings back. `--discard-ignored` evicts anyway and the cluster is
# gone; the dump in the checkpoint the eviction took is what is left of it.
ownership = "workspace"
depends_on = []

# In a monorepo, uncomment to run everything from one package. `.postgres`
# below is relative to it, and so is the line in .gitignore.
# workdir = "packages/api"

[ports]
# 5433, not 5432: a host Postgres that happens to be down when an instance
# spawns would otherwise lose its port to that instance.
db = { start = 5433, env = "RESOURCE_PORT" }

[exports]
# Trust auth on 127.0.0.1, as a Homebrew install has: anyone with a shell on
# this machine can connect, the same as to every other dev server here.
RESOURCE_URL = "postgresql://postgres@127.0.0.1:{{ports.db}}/postgres"

[actions.prepare]
# A cluster that is already there is kept: `prepare` runs again for a
# resource a `tracker use` brings back, and must not wipe its data.
command = '''
git check-ignore -q .postgres/data || { echo ".postgres/ is not gitignored: add /.postgres/ to .gitignore and commit it, or the cluster's files are captured into every checkpoint" >&2; exit 1; }
test -f .postgres/data/PG_VERSION || initdb -D .postgres/data -U postgres --auth=trust --encoding=UTF8 --locale=C --no-instructions
'''

[actions.start]
# The server is a process newgit supervises, like a dev server: `newgit
# status` sees it die, `newgit stop` and `start` take it down and back, and
# its log is `newgit`'s. TCP only: with no `unix_socket_directories` the
# server would put its socket where its build says, which on Debian is
# /var/run/postgresql, writable only by root.
command = "exec postgres -D .postgres/data -p {{ports.db}} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''"
long_running = true
autostart = true

# SIGINT is Postgres's fast shutdown: it disconnects clients and stops
# cleanly. The default SIGTERM waits for every client to leave, and one psql
# left open in a terminal would hold the stop forever.
[actions.stop]
signal = "int"

# Up is accepting connections, not merely bound: Postgres listens before it
# has finished recovery. `pg_isready` makes no query, so it is cheap enough
# to run on every `newgit status`.
[ready]
command = "pg_isready -q -h 127.0.0.1 -p {{ports.db}} -U postgres"
timeout_seconds = 30

[checkpoint]
mode = "command"
# The whole cluster, so a database the app made (`rails db:create`,
# `createdb test`) comes back too, roles included. `--restrict-key=newgit`
# keeps the dump byte-identical for the same state; `command-snapshot`'s
# checkpoint explains why, and what a fixed key gives up. Drop the flag on a
# pg_dumpall older than 17.6, 16.10, 15.14 or 14.19, which rejects it and
# needs none.
command = "pg_dumpall --restrict-key=newgit -h 127.0.0.1 -p {{ports.db}} -U postgres > {{snapshot.path}}/cluster.sql && echo {{snapshot.path}}/cluster.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
# `undo` stops the server before restoring and starts it after, so this runs
# with the port free. It builds a fresh cluster beside the live one, loads
# the dump into it on the same port, and only then swaps it in: a load that
# fails leaves the cluster as it was. A fresh cluster has nothing to collide
# with, so the dump loads whole, schema and data, with ON_ERROR_STOP. The one
# line taken out is the `CREATE ROLE postgres` every pg_dumpall writes, since
# `initdb` already made that role and creating it again is an error.
command = '''
set -e
rm -rf .postgres/restoring .postgres/restore.log
initdb -D .postgres/restoring -U postgres --auth=trust --encoding=UTF8 --locale=C --no-instructions >/dev/null
pg_ctl -D .postgres/restoring -l .postgres/restore.log -w -o "-p {{ports.db}} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''" start >/dev/null
if ! grep -vx 'CREATE ROLE postgres;' "{{state_ref}}" | psql -X --quiet -v ON_ERROR_STOP=1 -h 127.0.0.1 -p {{ports.db}} -U postgres -d postgres >/dev/null; then
  pg_ctl -D .postgres/restoring -w -m fast stop >/dev/null
  exit 1
fi
pg_ctl -D .postgres/restoring -w -m fast stop >/dev/null
rm -rf .postgres/data
mv .postgres/restoring .postgres/data
'''

[cleanup]
# Nothing to run: the server is newgit's process, stopped before teardown,
# and its data is in the workspace, deleted with it. A `tracker drop` that
# parks this resource stops the server and keeps the data for the next
# `use`.
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
        contents: r#"# ADD IT AS `database` (`newgit resource add database --template
# command-snapshot`) and the export below is `DATABASE_URL`, which most
# frameworks read already: every variable is named after the resource.
ownership = "branch"

# HOST TOOLS: `createdb`, `dropdb`, `pg_dump`, `psql` and `pg_isready` on
# your PATH, pointed at a Postgres running on the host. If your database lives in a
# container a tool manages (Docker Compose, the Supabase CLI), you have none
# of these against that database and the commands below will not work as
# written — this template is ready to adapt *if your database is on the
# host*. For Compose there is a `compose` template and for Supabase a
# `supabase` one, which go through `docker compose exec` and the CLI. To
# give each instance a server of its own rather than a database in a shared
# one, use `postgres`.
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

# Connect if the database is there, create it if not. Not `createdb ||
# true`: that also swallows a server that is down, and the spawn would
# report a database that does not exist as ready.
[actions.prepare]
command = "psql -Xq -d {{branch.slug}} -c 'select 1' >/dev/null 2>&1 || createdb {{branch.slug}}"

# The server is shared and not newgit's to start, so up means it answers.
# Without this the resource reads `ready` whenever its `prepare` once
# passed, server running or not.
[ready]
command = "pg_isready -q -d {{branch.slug}}"

# A convenience command, not a lifecycle hook: no stage runs `migrate`, and
# nothing but you ever will — `newgit action <this resource>.migrate`. Only
# `prepare` is run for you (by `spawn`, and by a `recompute` restore).
[actions.migrate]
command = "npm run db:migrate"

[checkpoint]
mode = "command"
# `--restrict-key=newgit` keeps the dump byte-identical for the same state.
# pg_dump (since 17.6, 16.10, 15.14, 14.19, 13.22) otherwise wraps every
# dump in `\restrict <key>` / `\unrestrict <key>` with a fresh random key,
# so no two checkpoints of one database ever match and `checkpoint --verify`
# always fails. The cost, stated plainly: the key exists so a hostile
# *server* cannot smuggle psql meta-commands into a dump and have them run
# on the machine that restores it, and a fixed key is a known key. The
# server here is this instance's own local database, and anything that can
# make it hostile — an agent working in the instance — can already run
# commands on this machine. Drop the flag on an older pg_dump that rejects
# it; those never emit the wrapper, so their dumps are stable anyway.
command = "pg_dump --restrict-key=newgit {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
# `--force` ends the sessions still open on this instance's database (a
# psql left in a terminal), which would otherwise make the drop fail.
# ON_ERROR_STOP makes a load that breaks halfway fail the restore instead of
# reporting success over half a database.
command = "dropdb --force --if-exists {{branch.slug}} && createdb {{branch.slug}} && psql -X --quiet -v ON_ERROR_STOP=1 -d {{branch.slug}} < {{state_ref}}"

[cleanup]
command = "dropdb {{branch.slug}} --if-exists"

[exports]
# The commands above reach the server through libpq's defaults (`PGHOST`,
# `PGPORT`, `PGUSER`); this URL has to name the same one. Edit it if yours
# is not on localhost's default port.
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
        contents: r#"# ADD IT AS `database` (`newgit resource add database --template
# command-snapshot-migrations`) and the export below is `DATABASE_URL`, which
# most frameworks read already: every variable is named after the resource.
ownership = "branch"

# HOST TOOLS: `createdb`, `dropdb`, `pg_dump`, `psql` and `pg_isready` on
# your PATH, pointed at a Postgres running on the host. Note what that
# excludes: this template names Rails, Prisma and Supabase as its audience
# below, and a Supabase user — or a Prisma user on Docker Compose, which is
# most of them — has none of these binaries against the database, because
# it lives in a container the tool manages. This is ready to adapt *if your
# database is on the host*. If it is Supabase, start from the `supabase`
# template instead, which goes through the CLI and `docker exec`; if it is
# Compose, start from `compose`, whose commands go through `docker compose
# exec`.
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

# Connect if the database is there, create it if not. Not `createdb ||
# true`: that also swallows a server that is down, and the spawn would
# report a database that does not exist as ready.
[actions.prepare]
command = "psql -Xq -d {{branch.slug}} -c 'select 1' >/dev/null 2>&1 || createdb {{branch.slug}}"

# The server is shared and not newgit's to start, so up means it answers.
# Without this the resource reads `ready` whenever its `prepare` once
# passed, server running or not.
[ready]
command = "pg_isready -q -d {{branch.slug}}"

[actions.reset]
command = "npm run db:migrate:reset"

[checkpoint]
mode = "command"
# `--restrict-key=newgit` keeps the dump byte-identical for the same state;
# without it `checkpoint --verify` always fails. `command-snapshot`'s
# checkpoint explains why, and what a fixed key gives up.
command = "pg_dump --data-only --restrict-key=newgit {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = """
npm run db:migrate:reset && psql -X --quiet -v ON_ERROR_STOP=1 -d {{branch.slug}} <<'SQL' && psql -X --quiet -v ON_ERROR_STOP=1 -d {{branch.slug}} < {{state_ref}}
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
# The commands above reach the server through libpq's defaults (`PGHOST`,
# `PGPORT`, `PGUSER`); this URL has to name the same one. Edit it if yours
# is not on localhost's default port.
RESOURCE_URL = "postgres://localhost/{{branch.slug}}"
"#,
    },
    ResourceTemplate {
        name: "compose",
        description: "a Docker Compose project per instance, worked for a Postgres `db` service; add it as `database`",
        companions: &[],
        // Same lane as the other database templates: a lane is per
        // database, not per template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"# ADD IT AS `database`: `newgit resource add database --template compose`.
# Every variable this file publishes is named after the resource, so that
# name makes them `DATABASE_URL` and `DATABASE_PORT`, which is what most
# frameworks read already. Under any other name they are `<NAME>_URL` and
# `<NAME>_PORT`, which no app reads until you point it at them.
#
# HOST TOOLS: `docker` with the Compose plugin (`docker compose`), and a
# daemon running. Nothing here needs a Postgres client on the host: every
# database command runs inside the service, through `docker compose exec`.
#
# Each instance is a Compose project of its own: its own containers, its own
# named volumes, its own published ports. Compose keeps projects apart by
# name, and `COMPOSE_PROJECT_NAME` below gives each instance one. It is a
# variable Compose itself reads, so every `docker compose` run through
# `newgit run` (`logs`, `ps`, `exec`) reaches this instance's stack with no
# `-p`. One project has one such name, so a second resource from this
# template in the same project would claim it twice and the graph refuses;
# put every service in the one Compose project instead.
#
# TWO EDITS, both load-bearing:
#
# 1. `EDIT_ME_PROJECT` below becomes your project's name, lowercase. The
#    instance name alone is not unique on this machine: two projects with a
#    `feature-a` would share one stack. Left as it is, Compose refuses it
#    (uppercase), which is the failure you want.
#
# 2. Your compose file publishes the port from `DATABASE_PORT`, with your
#    current port as the default, so a clone without newgit still works:
#
#      services:
#        db:
#          image: postgres:17
#          environment:
#            POSTGRES_PASSWORD: postgres
#            POSTGRES_DB: app
#          ports:
#            - "127.0.0.1:${DATABASE_PORT:-5432}:5432"
#          volumes:
#            - db-data:/var/lib/postgresql/data
#      volumes:
#        db-data:
#
#    A fixed `"5432:5432"` publishes every instance on one port, and the
#    second instance's `up` fails to bind it. Another service with a port
#    gets a lane of its own in `[ports]`, the same way.
#
# THERE IS NO GENERIC CHECKPOINT FOR COMPOSE. A stack is whatever services
# you wrote, and their state is in volumes newgit cannot read consistently
# while they run: a copy of a live database's files is not a snapshot of the
# database. So `[checkpoint]` and `[restore]` below go through the service's
# own dump tool, and are written for a Postgres service named `db` holding a
# database named `app`. For another database, change those three things to
# its tool (`mysqldump` and `mysql` for MySQL). For a stack with no state
# worth rewinding (a cache, a mail catcher), set both to `mode = "none"`
# and know that `undo` then leaves the stack exactly as it is.
ownership = "branch"
depends_on = []

[ports]
# 5433, not 5432: a host Postgres that happens to be down when an instance
# spawns would otherwise lose its port to that instance.
db = { start = 5433, env = "RESOURCE_PORT" }

[exports]
# EDIT ME: your project's name, lowercase.
COMPOSE_PROJECT_NAME = "EDIT_ME_PROJECT-{{branch.slug}}"
RESOURCE_URL = "postgresql://postgres:postgres@127.0.0.1:{{ports.db}}/app"

[actions.prepare]
# Exports are bound before `prepare` runs, so this brings up this
# instance's project on this instance's ports. `--wait` returns once every
# service is running, or healthy where it has a healthcheck.
command = "docker compose up --detach --wait"

# `newgit stop` and `newgit start`: the stack is not a process newgit
# supervises, so these are how it comes down and back. `stop` keeps the
# containers and volumes; `down` would not, and is `[cleanup]`.
[actions.suspend]
command = "docker compose stop"

[actions.resume]
command = "docker compose start"

# Up is the database accepting connections, which a running container is
# not yet. Over TCP (`-h 127.0.0.1`), not the socket: on a fresh volume the
# official image first runs a server on the socket alone to create
# `POSTGRES_DB`, then restarts it, and a check over the socket passes during
# that first server and lets a query land in the restart. This runs on every
# `newgit status`, and a `docker compose exec` costs a round trip to the
# daemon (tens of milliseconds); a stack with an HTTP service can use `http
# = "http://127.0.0.1:{{ports.web}}/health"` instead, which costs nothing.
[ready]
command = "docker compose exec -T db pg_isready -q -h 127.0.0.1 -U postgres"
timeout_seconds = 60

[checkpoint]
mode = "command"
# `--restrict-key=newgit` keeps the dump byte-identical for the same state;
# `command-snapshot`'s checkpoint explains why, and what a fixed key gives
# up. Drop it on an image older than Postgres 17.6, 16.10, 15.14 or 14.19,
# whose pg_dump rejects it and needs none.
command = "docker compose exec -T db pg_dump --restrict-key=newgit -U postgres app > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
# The stack keeps running through an `undo`, so the database is dropped
# and made again under it, then loaded. `--force` ends sessions still open
# on it; ON_ERROR_STOP makes a load that breaks halfway fail the restore.
command = "docker compose exec -T db sh -c 'dropdb -U postgres --force --if-exists app && createdb -U postgres app' && docker compose exec -T db psql -X --quiet -v ON_ERROR_STOP=1 -U postgres -d app < {{state_ref}}"

[cleanup]
# The instance is going away, and so do its containers, network and named
# volumes. A volume your compose file marks `external` is not the
# project's, and `down` leaves it alone.
command = "docker compose down --volumes --remove-orphans"
"#,
    },
    ResourceTemplate {
        name: "supabase",
        description: "a per-instance Supabase stack: own project_id, own nine ports, dump/reset/load",
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
#
# The same goes for every port below. The CLI uses a built-in default for a
# service whose section is missing from config.toml, and every instance
# shares that default. A fresh `supabase init` file has all six sections; an
# older or trimmed one may lack `[inbucket]` or `[analytics]`. Newgit cannot
# rewrite a line that is not there, so that bind fails naming the string it
# did not find. Add the section to config.toml (`[inbucket]` with `port =
# 54324`, `[analytics]` with `port = 54327`). Do not delete the rule to make
# the bind pass: the port would then collide between instances.

# In a monorepo, `supabase` usually lives in a package, not the workspace
# root. Uncomment and every action below runs from there. Change `path` in
# the [[render]] along with it ("apps/db/supabase/config.toml"), or the
# render rewrites a file the CLI never reads.
# workdir = "apps/db"

[ports]
# Supabase's committed defaults, one lane each. `start` is where scanning
# begins, so instance two lands just above instance one.
#
# Every 543xx port `supabase init` writes has a lane here, including the
# disabled pooler's and the commented-out `smtp_port` and `pop3_port`. The
# lanes scan up through that block, so a number the file names but no lane
# owns is one some instance will land on, and its render would then write a
# value the file already has, which `newgit capture` could not reverse
# (#205). If your config.toml names another port in that range, give it a
# lane and a `[[render]]` line the same way. `inspector_port = 8083` is
# outside it and stays shared.
api       = { start = 54321, env = "RESOURCE_API_PORT" }
db        = { start = 54322, env = "RESOURCE_DB_PORT" }
shadow    = { start = 54320 }
studio    = { start = 54323 }
inbucket  = { start = 54324 }
analytics = { start = 54327 }
smtp      = { start = 54325 }
pop3      = { start = 54326 }
pooler    = { start = 54329 }

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
# to match the wrong line. `smtp_port` and `pop3_port` are spelled out for
# the same reason, and without their `# ` so the rule still matches once you
# uncomment them.
[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "EDIT_ME_PROJECT"', with = 'project_id = "{{exports.RESOURCE_PROJECT}}"' },
  { find = "port = 54321",        with = "port = {{ports.api}}" },
  { find = "port = 54322",        with = "port = {{ports.db}}" },
  { find = "shadow_port = 54320", with = "shadow_port = {{ports.shadow}}" },
  { find = "port = 54323",        with = "port = {{ports.studio}}" },
  { find = "port = 54324",        with = "port = {{ports.inbucket}}" },
  { find = "smtp_port = 54325",   with = "smtp_port = {{ports.smtp}}" },
  { find = "pop3_port = 54326",   with = "pop3_port = {{ports.pop3}}" },
  { find = "port = 54327",        with = "port = {{ports.analytics}}" },
  { find = "port = 54329",        with = "port = {{ports.pooler}}" },
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
#
# The `stop` comes first because `supabase start` against a stack that is
# still running from an earlier binding (same `project_id`) prints its status
# and exits 0 without re-reading the rendered ports, so the instance would
# report ready on ports nothing listens on. `stop` keeps the database volume
# (only `--no-backup`, used in `cleanup`, deletes it), so the branch's data
# comes back on the new ports. Its output and exit code are ignored: there is
# nothing to stop on a first start.
#
# When a container is unhealthy, `supabase start` stops the whole stack
# before it exits, so the log ends in empty `container logs:` sections. To
# see why, start it by hand without the teardown, in this instance's
# environment: `newgit run <instance> -- supabase start
# --ignore-health-check`, then `docker logs <container>`.
command = "supabase stop >/dev/null 2>&1; supabase start"

# A convenience command, not a lifecycle hook: nothing runs this for you.
[actions.reset]
command = "supabase db reset"

# `newgit stop` and `newgit start`: the stack is not a process newgit
# supervises, so these are how it comes down and back up. `supabase stop`
# keeps the database volumes, so `resume` finds the data where it was.
[actions.suspend]
command = "supabase stop"

[actions.resume]
command = "supabase start"

# Up is the API gateway answering, and Auth behind it. Without this the
# stack reads `ready` whenever its `prepare` once passed, so one that has
# since gone down (Docker quit, a container died) still looks up, and what
# `depends_on` it starts against nothing. This GET goes through Kong to
# Auth's health endpoint, which needs no API key, and costs one local HTTP
# request, so it is cheap enough for every `newgit status`. Not `supabase
# status`: that is a CLI run and several Docker API calls each time.
[ready]
http = "http://127.0.0.1:{{ports.api}}/auth/v1/health"
timeout_seconds = 120

[checkpoint]
mode = "command"
# `--data-only`, because the schema comes from migrations rather than from
# the dump — the same reasoning as `command-snapshot-migrations`.
#
# The `sed` after the dump swaps pg_dump's per-run random `\restrict` key
# for a fixed one, so the same database always dumps to the same bytes —
# without it `checkpoint --verify` always fails. `supabase db dump` has no
# way to pass pg_dump `--restrict-key`, which is what `command-snapshot`
# uses for this; that template's checkpoint explains what a fixed key gives
# up. The CLI writes the pair as SQL comments, `-- \restrict <key>` and
# `-- \unrestrict <key>`, where a plain pg_dump writes them bare, so the
# match takes the line either way. A dump with no `\restrict` line is left
# alone.
command = '''
supabase db dump --local --data-only -f {{snapshot.path}}/db.sql || exit 1
key=$(sed -n 's/^\(-- \)\{0,1\}\\restrict //p' {{snapshot.path}}/db.sql | head -n 1)
if [ -n "$key" ]; then
  sed "s/$key/newgit/" {{snapshot.path}}/db.sql > {{snapshot.path}}/db.sql.tmp || exit 1
  mv {{snapshot.path}}/db.sql.tmp {{snapshot.path}}/db.sql || exit 1
fi
echo {{snapshot.path}}/db.sql
'''
into_tracker = "db-snapshots"

[restore]
mode = "command"
# reset (replays the migrations, then the seed), empty every table the dump
# carries, then load the data-only dump. Loading straight onto a freshly
# reset database dies on duplicate keys.
#
# The dump carries `auth` and `storage` as well as `public`, and the reset
# puts rows back into all three: a migration's `insert into
# storage.buckets`, a seed's `insert into auth.users`, the buckets
# config.toml declares. So all three are emptied, except the migration
# history the dump leaves out (`auth.schema_migrations`,
# `storage.migrations`), which Auth and Storage read to decide what to
# migrate. `has_table_privilege` skips what this role may not truncate. No
# `restart identity`: the load sets every sequence the dump carries, and
# only a sequence's owner may restart it, which `postgres` is not for
# `auth`'s.
#
# A schema of your own that a migration or the seed writes rows into goes
# in the list too. `ON_ERROR_STOP` stops the restore when a truncate
# fails, instead of loading on top of what it could not empty, and fails it
# when the load breaks halfway, instead of reporting a half-loaded database
# as restored.
command = """
supabase db reset && docker exec -i {{exports.RESOURCE_DB_CONTAINER}} psql --quiet -v ON_ERROR_STOP=1 -U postgres -d postgres <<'SQL' && docker exec -i {{exports.RESOURCE_DB_CONTAINER}} psql --quiet -v ON_ERROR_STOP=1 -U postgres -d postgres < {{state_ref}}
do $$
declare t record;
begin
  for t in
    select schemaname, tablename from pg_tables
     where schemaname in ('public', 'auth', 'storage')
       and (schemaname, tablename) not in (('auth', 'schema_migrations'), ('storage', 'migrations'))
       and has_table_privilege(format('%I.%I', schemaname, tablename), 'truncate')
  loop
    execute format('truncate table %I.%I cascade', t.schemaname, t.tablename);
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
        contents: r#"# HOST TOOLS: your provider's CLI. `EDIT_ME_CLI` below stands in for it
# and is not a real program — every command here is yours to write, and
# `newgit resource check` fails until none of them names it.
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
command = "EDIT_ME_CLI preview create --branch {{branch.name}} --json"
captures = ["RESOURCE_ID", "RESOURCE_URL"]

[checkpoint]
mode = "external"
state_ref = "{{exports.RESOURCE_ID}}"

[restore]
# The handle is recorded, not re-created: an undo does not rewind another
# system. Change to `mode = "command"` if yours can be rewound.
mode = "external"

[cleanup]
command = "EDIT_ME_CLI preview delete {{state_ref}}"
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
