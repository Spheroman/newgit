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
        contents: r#"kind = "process"
ownership = "branch"
# Trackers or resources that must be ready first, e.g. ["deps", "runtime-env"].
depends_on = []

[ports]
app = { start = 3100, env = "PORT" }

[actions.start]
# Edit to your dev command, e.g. "pnpm dev" or "bin/rails server".
command = "npm run dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
"#,
    },
    ResourceTemplate {
        name: "pnpm",
        description: "dependency install via pnpm; recomputed, never copied",
        // The shared package store is user-owned: newgit must never delete
        // or rewrite it. Created alongside so `depends_on` resolves.
        companions: &[CompanionFile {
            name: "pnpm-store",
            contents: r#"kind = "external-store"
ownership = "user"

[checkpoint]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[restore]
mode = "none"
"#,
        }],
        companion_trackers: &[],
        contents: r#"kind = "command"
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]

[actions.prepare]
command = "pnpm install --frozen-lockfile"

[checkpoint]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

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
        contents: r#"kind = "command-snapshot"
ownership = "branch"

# A branch-local database, named after the instance so instances never share
# one. Edit the commands for your database; the shape is what matters:
# checkpoint emits a dump into the lane, restore reads it back.

[actions.prepare]
command = "createdb {{branch.slug}} || true"

[actions.migrate]
# Edit to your migration command.
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
DATABASE_URL = "postgres://localhost/{{branch.slug}}"
"#,
    },
    ResourceTemplate {
        name: "external",
        description: "a resource another system owns; newgit holds only a handle",
        companions: &[],
        companion_trackers: &[],
        contents: r#"kind = "external"
ownership = "external"

# newgit does not own this resource, so it never assumes deletion semantics
# it did not author: `ownership = "external"` means cleanup runs exactly the
# command below and nothing else.
#
# `captures` reads names out of the prepare command's stdout — either a flat
# JSON object or KEY=VALUE lines — and publishes them as this resource's
# exports, so `newgit run` and the hooks below can use them.

[actions.prepare]
# Edit to your provisioning command.
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]

[checkpoint]
mode = "external"
state_ref = "{{exports.PREVIEW_ID}}"

[restore]
# The handle is recorded, not re-created: an undo does not rewind another
# system. Change to `mode = "command"` if yours can be rewound.
mode = "external"

[cleanup]
command = "cloudctl preview delete {{state_ref}}"
"#,
    },
];

pub fn resource_template(name: &str) -> Option<&'static ResourceTemplate> {
    RESOURCE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}
